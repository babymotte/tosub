/*
 *  Copyright 2026 Michael Bachmann
 *
 * Licensed under either the MIT or the Apache License, Version 2.0,
 * as per the user's preference.
 * You may not use this file except in compliance with at least one
 * of these two licenses.
 * You may obtain a copy of the Licenses at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *     and
 *     https://opensource.org/license/MIT
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use miette::{Diagnostic, IntoDiagnostic};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fmt::{self, Debug, Display},
    io::stdin,
    mem,
    process::{self, ExitCode},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use thiserror::Error;
#[cfg(feature = "tokio_unstable")]
use tokio::task::JoinHandle;
#[cfg(not(feature = "tokio_unstable"))]
use tokio::task::JoinHandle;
use tokio::{
    select,
    sync::{oneshot, watch},
    task::{self, JoinError},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsystemId {
    name: String,
    task_id: u64,
}

impl PartialOrd for SubsystemId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SubsystemId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.task_id
            .cmp(&other.task_id)
            .then_with(|| self.name.cmp(&other.name))
    }
}

impl SubsystemId {
    fn unassigned(name: String) -> Self {
        SubsystemId { name, task_id: 0 }
    }

    fn set_task_id(&mut self, task_id: u64) {
        self.task_id = task_id;
    }
}

impl Display for SubsystemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.task_id, self.name)
    }
}

type SubsystemMap = Arc<Mutex<BTreeMap<SubsystemId, oneshot::Receiver<()>>>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MetricsEvent {
    TaskSpawned(TaskInfo),
    #[serde(rename_all = "camelCase")]
    RootSystemStarted {
        id: SubsystemId,
    },
    #[serde(rename_all = "camelCase")]
    RootSystemStopped {
        id: SubsystemId,
        outcome: Outcome,
    },
    #[serde(rename_all = "camelCase")]
    SubsystemStarted {
        id: SubsystemId,
        all_running: Box<[SubsystemId]>,
    },
    #[serde(rename_all = "camelCase")]
    SubsystemStopped {
        id: SubsystemId,
        outcome: Outcome,
        all_running: Box<[SubsystemId]>,
    },
}

impl Display for MetricsEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", json!(self))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outcome {
    TerminatedNormally,
    TerminatedWithError(String),
    ShutdownForced,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInfo {
    pub id: u64,
    #[serde(rename = "type")]
    pub task_type: TaskType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskType {
    RootSystem(SubsystemId),
    Subsystem(SubsystemId),
    CancellationTask(SubsystemId),
    JoinSignal(SubsystemId),
    SignalHandler(String),
    LocalShutdownTimeout(SubsystemId),
    MetricsLogger,
}

pub struct RootBuilder {
    name: String,
    catch_signals: bool,
    shutdown_timeout: Option<std::time::Duration>,
    shutdown_on_stdin_close: bool,
    stdin_consumer: Option<Box<dyn Fn(String) + Send + 'static>>,
}

struct CrashHolder {
    crash: Arc<Mutex<SubsystemResult>>,
    cancel: CancellationToken,
}

impl Clone for CrashHolder {
    fn clone(&self) -> Self {
        CrashHolder {
            crash: self.crash.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

impl CrashHolder {
    fn set_exit_code(&self, code: ExitCode) {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        if guard.is_ok() {
            *guard = Ok(code);
            self.cancel.cancel();
        }
    }

    fn set_crash(&self, err: SubsystemError) {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        if guard.is_ok() {
            *guard = Err(err);
            self.cancel.cancel();
        }
    }

    fn take_crash(&self) -> SubsystemResult {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        mem::replace(&mut *guard, Ok(ExitCode::SUCCESS))
    }
}

pub trait IntoExitCode {
    fn into_exit_code(self) -> ExitCode;
}

impl IntoExitCode for ExitCode {
    fn into_exit_code(self) -> ExitCode {
        self
    }
}

impl IntoExitCode for () {
    fn into_exit_code(self) -> ExitCode {
        ExitCode::SUCCESS
    }
}

pub trait IntoExitCodeResult {
    type Output: IntoExitCode;
    type E: IntoGenericError;
    fn into_exit_code_result(self) -> Result<Self::Output, Self::E>;
}

pub struct Infallible;

impl Display for Infallible {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unreachable!()
    }
}

impl IntoGenericError for Infallible {
    fn into_generic_error(self) -> GenericError {
        unreachable!()
    }
}

impl<C: IntoExitCode> IntoExitCodeResult for C {
    type Output = C;
    type E = Infallible;

    fn into_exit_code_result(self) -> Result<Self::Output, Self::E> {
        Ok(self)
    }
}

impl<E: IntoGenericError> IntoExitCodeResult for Result<(), E> {
    type Output = ExitCode;
    type E = E;

    fn into_exit_code_result(self) -> Result<Self::Output, Self::E> {
        match self {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(err) => Err(err),
        }
    }
}

impl RootBuilder {
    pub async fn start<R, F>(
        mut self,
        subsys: impl FnOnce(Subsystem) -> F + Send + 'static,
    ) -> miette::Result<ExitCode>
    where
        F: std::future::Future<Output = R> + Send + 'static,
        R: IntoExitCodeResult,
    {
        let global = CancellationToken::new();
        let local = global.child_token();

        let crash = CrashHolder {
            crash: Arc::new(Mutex::new(Ok(ExitCode::SUCCESS))),
            cancel: global.clone(),
        };

        if self.catch_signals {
            self.register_signal_handlers(&global, crash.clone());
        }

        let stdin_consumer = self.stdin_consumer.take();
        let shutdown_on_stdin_close = self.shutdown_on_stdin_close;
        if stdin_consumer.is_some() || shutdown_on_stdin_close {
            self.register_stdin_handler(
                &global,
                crash.clone(),
                shutdown_on_stdin_close,
                stdin_consumer,
            );
        }

        let (res_tx, res_rx) = oneshot::channel();
        let (join_tx, join_rx) = watch::channel(None);

        let cancel_clean_global_shutdown = CancellationToken::new();
        let cancel_clean_local_shutdown = CancellationToken::new();

        let subsystems = Arc::new(Mutex::new(BTreeMap::new()));

        let handle = Subsystem {
            id: SubsystemId::unassigned(self.name.clone()),
            global: global.clone(),
            local: local.clone(),
            cancel_clean_global_shutdown: cancel_clean_global_shutdown.clone(),
            cancel_clean_local_shutdown,
            subsystems: subsystems.clone(),
            crash: crash.clone(),
            join_handle: (join_tx.clone(), join_rx),
            shutdown_timeout: self.shutdown_timeout,
        };

        let glob = global.clone();
        let task_name = format!("Root system: {}", self.name);

        spawn_task(
            async move {
                Self::run_root_system(subsys, &global, &crash, handle, &glob).await;

                // collect terminaion signals of remaining subsystems and wait for them to trigger
                let subsystems = {
                    let mut subsystems = subsystems.lock().expect("mutex is poisoned");
                    std::mem::take(&mut *subsystems)
                };
                let subsys_shutdown_future = wait_for_subsystems_shutdown(subsystems);

                if let Some(to) = self.shutdown_timeout {
                    info!(
                        "Shutdown initiated, waiting for clean shutdown for up to {:?}.",
                        to
                    );

                    // race the timeout against the subsystems shutting down
                    match timeout(to, subsys_shutdown_future).await {
                        Ok(_) => {
                            info!("All subsystems have shut down in time.");
                        }
                        Err(_) => {
                            error!(
                                "Global shutdown timeout reached, forcing shutdown of remaining subsystems …"
                            );
                            cancel_clean_global_shutdown.cancel();
                            crash.set_crash(SubsystemError::ForcedShutdown);
                        }
                    }

                    // trigger join handle of root system
                    res_tx.send(crash.take_crash()).ok();
                    join_tx.send(Some(Err(SubsystemError::ForcedShutdown))).ok();
                } else {
                    info!("Shutdown initiated, waiting for clean shutdown.");

                    // wait for subsystems shutting down
                    subsys_shutdown_future.await;
                    info!("All subsystems have shut down.");

                    res_tx.send(crash.take_crash()).ok();
                    join_tx
                        .send(Some(Err(SubsystemError::OrderlyShutdown)))
                        .ok();
                }
            },
            &task_name,
        );

        // block on root system to run and clean up
        res_rx
            .await
            .unwrap_or(Err(SubsystemError::ForcedShutdown))
            .into_diagnostic()
    }

    async fn run_root_system<F, R>(
        subsys: impl (FnOnce(Subsystem) -> F) + Send + 'static,
        global: &CancellationToken,
        crash: &CrashHolder,
        mut handle: Subsystem,
        glob: &CancellationToken,
    ) where
        F: std::future::Future<Output = R> + Send + 'static,
        R: IntoExitCodeResult,
    {
        // set root system ID
        let task_id = task_id();
        handle.id.set_task_id(task_id);
        let id = handle.id.clone();

        // trace root system launch
        let event = MetricsEvent::TaskSpawned(TaskInfo {
            id: task_id,
            task_type: TaskType::RootSystem(id.clone()),
        });
        trace!(metrics_event = %event);
        let event = MetricsEvent::RootSystemStarted { id: id.clone() };
        trace!(metrics_event = %event);

        // run the actual root system function
        match subsys(handle)
            .await
            .into_exit_code_result()
            .map(IntoExitCode::into_exit_code)
        {
            Ok(ExitCode::SUCCESS) => {
                let event = MetricsEvent::RootSystemStopped {
                    id,
                    outcome: Outcome::TerminatedNormally,
                };
                trace!(metrics_event = %event);
            }
            Ok(exit_code) => {
                let event = MetricsEvent::RootSystemStopped {
                    id: id.clone(),
                    outcome: Outcome::TerminatedWithError(
                        "Root system exited with non-zero exit code.".to_owned(),
                    ),
                };
                trace!(metrics_event = %event);
                crash.set_exit_code(exit_code);
            }
            Err(e) => {
                let generic_error = e.into_generic_error();
                let event = MetricsEvent::RootSystemStopped {
                    id: id.clone(),
                    outcome: Outcome::TerminatedWithError(generic_error.to_string()),
                };
                trace!(metrics_event = %event);
                crash.set_crash(SubsystemError::Error(id, generic_error));
            }
        }

        // root system completed, request shutdown of all subsystems

        if !global.is_cancelled() {
            glob.cancel();
        }
    }

    pub fn catch_signals(mut self) -> Self {
        self.catch_signals = true;
        self
    }

    pub fn catch_no_signals(mut self) -> Self {
        self.catch_signals = false;
        self
    }

    pub fn shutdown_on_stdin_close(mut self) -> Self {
        self.shutdown_on_stdin_close = true;
        self
    }

    pub fn no_shutdown_on_stdin_close(mut self) -> Self {
        self.shutdown_on_stdin_close = false;
        self
    }

    pub fn with_timeout(mut self, shutdown_timeout: Duration) -> Self {
        self.shutdown_timeout = Some(shutdown_timeout);
        self
    }

    pub fn without_timeout(mut self) -> Self {
        self.shutdown_timeout = None;
        self
    }

    pub fn with_stdin_consumer(mut self, consumer: impl Fn(String) + Send + 'static) -> Self {
        self.stdin_consumer = Some(Box::new(consumer));
        self
    }

    pub fn without_stdin_consumer(mut self) -> Self {
        self.stdin_consumer = None;
        self
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    fn register_signal_handlers(&self, global: &CancellationToken, _: CrashHolder) {
        use tokio::signal::ctrl_c;

        let global = global.clone();
        let task_id = spawn(async move {
            let mut counter = 0;
            loop {
                ctrl_c().await.expect("Ctrl+C handler not supported");
                counter += 1;
                if counter > 1 {
                    break;
                }
                info!("Received Ctrl+C, initiating shutdown.");
                global.cancel();
            }
            process::exit(1);
        })
        .id();
        let event = MetricsEvent::TaskSpawned(TaskInfo {
            id: to_u64(&task_id),
            task_type: TaskType::SignalHandler("Ctrl+C".to_owned()),
        });
        self.metrics_tx.try_send(event).ok();
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
    fn register_signal_handlers(&self, global: &CancellationToken, crash: CrashHolder) {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(signal) = signal(SignalKind::hangup()) {
            handle_unix_signal(
                global,
                signal,
                "SIGHUP",
                SignalKind::hangup().as_raw_value(),
                crash.clone(),
            );
        } else {
            error!("Failed to register SIGHUP handler");
        }

        if let Ok(signal) = signal(SignalKind::interrupt()) {
            handle_unix_signal(
                global,
                signal,
                "SIGINT",
                SignalKind::interrupt().as_raw_value(),
                crash.clone(),
            );
        } else {
            error!("Failed to register SIGINT handler");
        }

        if let Ok(signal) = signal(SignalKind::quit()) {
            handle_unix_signal(
                global,
                signal,
                "SIGQUIT",
                SignalKind::quit().as_raw_value(),
                crash.clone(),
            );
        } else {
            error!("Failed to register SIGQUIT handler");
        }

        if let Ok(signal) = signal(SignalKind::terminate()) {
            handle_unix_signal(
                global,
                signal,
                "SIGTERM",
                SignalKind::terminate().as_raw_value(),
                crash.clone(),
            );
        } else {
            error!("Failed to register SIGTERM handler");
        }
    }

    fn register_stdin_handler<F>(
        &self,
        global: &CancellationToken,
        crash: CrashHolder,
        shutdown_on_stdin_close: bool,
        consumer: Option<F>,
    ) where
        F: Fn(String) + Send + 'static,
    {
        let global = global.clone();
        thread::spawn(move || gobble_stdin(global, crash, shutdown_on_stdin_close, consumer));
    }
}

fn gobble_stdin<F: Fn(String)>(
    global: CancellationToken,
    crash: CrashHolder,
    shutdown_on_stdin_close: bool,
    consumer: Option<F>,
) {
    for line in stdin().lines() {
        match line {
            Ok(line) => {
                if let Some(consumer) = &consumer {
                    consumer(line);
                }
            }
            Err(e) => {
                warn!("Stdin closed abnormally: {e}");

                if shutdown_on_stdin_close {
                    info!("Initiating shutdown.");
                    crash.set_exit_code(ExitCode::FAILURE);
                    global.cancel();
                }
                return;
            }
        }
    }
    if shutdown_on_stdin_close {
        info!("Stdin closed, initiating shutdown.");
        global.cancel();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn handle_unix_signal(
    global: &CancellationToken,
    mut signal: tokio::signal::unix::Signal,
    signal_name: &'static str,
    code: i32,
    crash: CrashHolder,
) {
    let global = global.clone();
    let task_name = format!("Signal handler: {signal_name}");
    spawn_task(
        async move {
            let task_id = task_id();
            let event = MetricsEvent::TaskSpawned(TaskInfo {
                id: task_id,
                task_type: TaskType::SignalHandler(signal_name.to_owned()),
            });
            trace!(metrics_event = %event);

            let mut already_triggered = false;
            loop {
                signal.recv().await;
                if already_triggered {
                    break;
                }
                already_triggered = true;
                info!("Received {signal_name} signal, initiating shutdown.");
                crash.set_exit_code(ExitCode::from(128 + code as u8));
                global.cancel();
            }
            process::exit(128 + code);
        },
        &task_name,
    );
}

#[derive(Debug, Clone, Error, Diagnostic)]
pub enum SubsystemError {
    #[error("Subsystem '{0}' terminated with error")]
    Error(SubsystemId, #[source] GenericError),
    #[error("Subsystem '{0}' panicked")]
    Panic(SubsystemId, #[source] GenericError),
    #[error("Subsystem did not complete because it was asked to shut down")]
    OrderlyShutdown,
    #[error("Subsystem did not complete because it was forced to shut down")]
    ForcedShutdown,
}

pub trait GenErr: Debug + Display + Send + Sync + 'static {}

impl<E> GenErr for E where E: Debug + Display + Send + Sync + 'static {}

#[derive(Clone, Error, Diagnostic)]
pub struct GenericError(Arc<dyn GenErr>);

impl From<miette::Report> for GenericError {
    fn from(err: miette::Report) -> Self {
        GenericError(Arc::new(err))
    }
}

pub trait IntoGenericError {
    fn into_generic_error(self) -> GenericError;
}

pub trait IntoGenericResult {
    type Output: Clone + Send + Sync + 'static;
    fn into_generic_result(self) -> Result<Self::Output, GenericError>;
}

impl<T: Clone + Send + Sync + 'static, Err> IntoGenericResult for Result<T, Err>
where
    Err: IntoGenericError,
{
    type Output = T;

    fn into_generic_result(self) -> Result<Self::Output, GenericError> {
        match self {
            Ok(it) => Ok(it),
            Err(e) => Err(e.into_generic_error()),
        }
    }
}

impl IntoGenericResult for () {
    type Output = ();

    fn into_generic_result(self) -> Result<Self::Output, GenericError> {
        Ok(())
    }
}

pub trait IntoSubsystemResult<T> {
    fn into_subsystem_result(self, id: SubsystemId) -> Result<T, SubsystemError>;
}

impl<T, E: IntoGenericError> IntoSubsystemResult<T> for Result<T, E> {
    fn into_subsystem_result(self, id: SubsystemId) -> Result<T, SubsystemError> {
        match self {
            Ok(it) => Ok(it),
            Err(e) => Err(SubsystemError::Error(id, e.into_generic_error())),
        }
    }
}

impl<E: GenErr> IntoGenericError for E {
    fn into_generic_error(self) -> GenericError {
        GenericError(Arc::new(self))
    }
}

impl fmt::Debug for GenericError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for GenericError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

type SubsystemResult = Result<ExitCode, SubsystemError>;

async fn wait_for_subsystems_shutdown(subsystems: BTreeMap<SubsystemId, oneshot::Receiver<()>>) {
    for rx in subsystems.into_values() {
        rx.await.ok();
    }
}

type ResultWatchSender<T> = watch::Sender<Option<Result<T, SubsystemError>>>;
type ResultWatchReceiver<T> = watch::Receiver<Option<Result<T, SubsystemError>>>;

type ResultWatchChannel<T> = (ResultWatchSender<T>, ResultWatchReceiver<T>);

pub struct Subsystem<T = ()> {
    id: SubsystemId,
    local: CancellationToken,
    global: CancellationToken,
    cancel_clean_global_shutdown: CancellationToken,
    cancel_clean_local_shutdown: CancellationToken,
    subsystems: SubsystemMap,
    crash: CrashHolder,
    join_handle: ResultWatchChannel<T>,
    shutdown_timeout: Option<Duration>,
}

impl<T> Clone for Subsystem<T> {
    fn clone(&self) -> Self {
        Subsystem {
            id: self.id.clone(),
            local: self.local.clone(),
            global: self.global.clone(),
            cancel_clean_global_shutdown: self.cancel_clean_global_shutdown.clone(),
            cancel_clean_local_shutdown: self.cancel_clean_local_shutdown.clone(),
            subsystems: self.subsystems.clone(),
            crash: self.crash.clone(),
            join_handle: (self.join_handle.0.clone(), self.join_handle.1.clone()),
            shutdown_timeout: self.shutdown_timeout,
        }
    }
}

impl<T> fmt::Debug for Subsystem<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubsystemHandle")
            .field("id", &self.id)
            .finish()
    }
}

pub trait SubsystemFuture: Future<Output = Self::Res> + Send + 'static {
    type T: Send + Sync + 'static;
    type Res: IntoGenericResult<Output = Self::T>;
}

pub trait SubsystemFunction<T: Send + Sync + 'static>:
    FnOnce(Subsystem<T>) -> Self::F + Send + 'static
{
    type F: SubsystemFuture<T = T>;
}

impl<Fut> SubsystemFuture for Fut
where
    Fut: Future + Send + 'static,
    Fut::Output: IntoGenericResult,
{
    type T = <Fut::Output as IntoGenericResult>::Output;
    type Res = Fut::Output;
}

impl<Func, Fut, T> SubsystemFunction<T> for Func
where
    Func: FnOnce(Subsystem<T>) -> Fut + Send + 'static,
    Fut: SubsystemFuture<T = T>,
    T: Send + Sync + 'static,
{
    type F = Fut;
}

impl<T> Subsystem<T> {
    pub fn name(&self) -> &str {
        &self.id.name
    }

    pub fn id(&self) -> &SubsystemId {
        &self.id
    }

    pub async fn shutdown_requested(&self) {
        self.local.cancelled().await
    }

    pub async fn into_shutdown_requested(self) {
        self.local.cancelled().await
    }

    pub fn is_shut_down(&self) -> bool {
        self.local.is_cancelled()
    }

    pub fn request_global_shutdown(&self) {
        info!("Global shutdown requested from subsystem '{}'", self.id);
        self.global.cancel();
    }

    pub fn spawn<ChildT>(
        &self,
        name: impl AsRef<str>,
        subsys: impl SubsystemFunction<ChildT>,
    ) -> Subsystem<ChildT>
    where
        ChildT: Clone + Send + Sync + 'static,
    {
        let cancel_clean_global_shutdown = self.cancel_clean_global_shutdown.clone();
        let cancel_clean_local_shutdown = CancellationToken::new();

        let (mut handle, gc_rx) = self.create_child(
            name,
            cancel_clean_global_shutdown.clone(),
            cancel_clean_local_shutdown.clone(),
        );
        let full_name = handle.name().to_owned();

        let fname = full_name.clone();
        let subsys_task_name = format!("Subsystem: {fname}");
        let cancellation_task_name = format!("Cancellation monitor: {fname}");
        let subsystems = self.subsystems.clone();
        let mut crash = self.crash.clone();
        let glob = self.global.clone();
        let mut h = handle.clone();
        let res_tx = h.join_handle.0.clone();

        let mut join_handle = spawn_task(
            async move {
                let task_id = task_id();
                h.id.set_task_id(task_id);
                let id = h.id.clone();

                let event = MetricsEvent::TaskSpawned(TaskInfo {
                    id: task_id,
                    task_type: TaskType::Subsystem(id.clone()),
                });
                trace!(metrics_event = %event);

                subsys(h).await.into_generic_result()
            },
            &subsys_task_name,
        );

        let subsys_task_id = to_u64(join_handle.id());
        handle.id.set_task_id(subsys_task_id);
        let subsystem_id = handle.id.clone();
        let subsystem_id_2 = subsystem_id.clone();

        spawn_task(
            async move {
                let tid = task_id();
                let event = MetricsEvent::TaskSpawned(TaskInfo {
                    id: tid,
                    task_type: TaskType::CancellationTask(subsystem_id_2.clone()),
                });
                trace!(metrics_event = %event);

                select! {
                    res = &mut join_handle => Self::subsystem_joined(res, subsystems, subsystem_id_2, &mut crash, res_tx).await,
                    _ = cancel_clean_global_shutdown.cancelled() => Self::global_shutdown_timed_out(join_handle, subsystem_id_2, &glob, &mut crash).await,
                    _ = cancel_clean_local_shutdown.cancelled() => Self::subsystem_timed_out(join_handle, subsystems, subsystem_id_2, res_tx).await,
                };
            },
            &cancellation_task_name,
        );

        let all_running = {
            let mut gc = self.subsystems.lock().expect("mutex is poisoned");
            gc.insert(subsystem_id.clone(), gc_rx);
            gc.keys().cloned().collect::<Box<[_]>>()
        };

        let event = MetricsEvent::SubsystemStarted {
            id: subsystem_id.clone(),
            all_running: all_running.clone(),
        };
        trace!(metrics_event = %event, "Subsystem '{}' started. List of all running subsytems: {:#?}", subsystem_id.name, all_running);

        handle
    }

    fn create_child<ChildT: Send + Sync + 'static>(
        &self,
        name: impl AsRef<str>,
        cancel_clean_global_shutdown: CancellationToken,
        cancel_clean_local_shutdown: CancellationToken,
    ) -> (Subsystem<ChildT>, oneshot::Receiver<()>) {
        let (res_tx, res_rx) = watch::channel::<Option<Result<ChildT, SubsystemError>>>(None);
        let name = format!("{}/{}", self.name(), name.as_ref());
        let global = self.global.clone();
        let local = self.local.child_token();
        let subsystems = self.subsystems.clone();
        let crash = self.crash.clone();

        let (gc_tx, gc_rx) = oneshot::channel();
        let mut gc_res_rx = res_rx.clone();

        let id = SubsystemId::unassigned(name.clone());
        let id_2 = id.clone();
        let task_name = format!("Join: {name}");

        spawn_task(
            async move {
                let task_id = task_id();
                let event = MetricsEvent::TaskSpawned(TaskInfo {
                    id: task_id,
                    task_type: TaskType::JoinSignal(id_2),
                });
                trace!(metrics_event = %event);
                gc_res_rx.wait_for(|it| it.is_some()).await.ok();
                gc_tx.send(()).ok();
            },
            &task_name,
        );

        (
            Subsystem {
                id,
                global,
                local,
                cancel_clean_global_shutdown,
                cancel_clean_local_shutdown,
                subsystems,
                crash,
                join_handle: (res_tx, res_rx),
                shutdown_timeout: self.shutdown_timeout,
            },
            gc_rx,
        )
    }

    async fn subsystem_joined<ChildT: Send + Sync>(
        res: Result<Result<ChildT, GenericError>, JoinError>,
        subsystems: SubsystemMap,
        id: SubsystemId,
        crash: &mut CrashHolder,
        res_tx: ResultWatchSender<ChildT>,
    ) {
        let all_running = {
            let mut gc = subsystems.lock().expect("mutex is poisoned");
            gc.remove(&id);
            gc.keys().cloned().collect::<Box<[_]>>()
        };

        match res {
            Ok(Ok(it)) => {
                let event = MetricsEvent::SubsystemStopped {
                    id: id.clone(),
                    outcome: Outcome::TerminatedNormally,
                    all_running: all_running.clone(),
                };
                trace!(metrics_event = %event, "Subsystem '{}' terminated normally. List of all remaining subsytems: {:#?}", id.name, all_running);
                res_tx.send(Some(Ok(it))).ok();
            }
            Ok(Err(e)) => {
                let event = MetricsEvent::SubsystemStopped {
                    id: id.clone(),
                    outcome: Outcome::TerminatedWithError(e.to_string()),
                    all_running: all_running.clone(),
                };
                trace!(metrics_event = %event, "Subsystem '{}' terminated with error: {e}. List of all remaining subsytems: {:#?}", id.name, all_running);
                let err = SubsystemError::Error(id, e);
                crash.set_crash(err.clone());
                res_tx.send(Some(Err(err))).ok();
            }
            Err(e) => {
                if e.is_panic() {
                    error!("Subsystem '{}' panicked: {}", id.name, e);
                    let event = MetricsEvent::SubsystemStopped {
                        id: id.clone(),
                        outcome: Outcome::TerminatedWithError(e.to_string()),
                        all_running: all_running.clone(),
                    };
                    trace!(metrics_event = %event, "Subsystem '{}' panicked: {e}. List of all remaining subsytems: {:#?}", id.name, all_running);
                    let err = SubsystemError::Panic(id, e.into_generic_error());
                    crash.set_crash(err.clone());
                    res_tx.send(Some(Err(err))).ok();
                } else {
                    warn!("Subsystem '{}' was shut down forcefully.", id.name);
                    let event = MetricsEvent::SubsystemStopped {
                        id: id.clone(),
                        outcome: Outcome::ShutdownForced,
                        all_running: all_running.clone(),
                    };
                    trace!(metrics_event = %event, "Subsystem '{}' was shut down forcefully. List of all remaining subsytems: {:#?}", id.name, all_running);
                    let err = SubsystemError::ForcedShutdown;
                    crash.set_crash(err.clone());
                    res_tx.send(Some(Err(err))).ok();
                }
            }
        };
    }

    async fn subsystem_timed_out<ChildT, Err>(
        join_handle: tokio::task::JoinHandle<Result<ChildT, Err>>,
        subsystems: SubsystemMap,
        id: SubsystemId,
        res_tx: watch::Sender<Option<Result<ChildT, SubsystemError>>>,
    ) where
        Err: Debug + Display + Send + Sync + 'static,
    {
        Self::local_shutdown_timed_out(join_handle, &id).await;

        let all_running = {
            let mut gc = subsystems.lock().expect("mutex is poisoned");
            gc.remove(&id);
            gc.keys().cloned().collect::<Box<[_]>>()
        };

        warn!("Subsystem '{}' was shut down forcefully.", id);

        let event = MetricsEvent::SubsystemStopped {
            id: id.clone(),
            outcome: Outcome::ShutdownForced,
            all_running: all_running.clone(),
        };
        trace!(metrics_event = %event, "Subsystem '{}' was shut down forcefully. List of all remaining subsytems: {:#?}", id.name, all_running);

        res_tx.send(Some(Err(SubsystemError::ForcedShutdown))).ok();
    }

    async fn global_shutdown_timed_out<ChildT, Err>(
        join_handle: tokio::task::JoinHandle<Result<ChildT, Err>>,
        id: SubsystemId,
        global: &CancellationToken,
        crash: &mut CrashHolder,
    ) where
        Err: Debug + Display + Send + Sync + 'static,
    {
        warn!("Subsystem '{}' is being shut down forcefully.", id);
        join_handle.abort();
        global.cancel();
        crash.set_crash(SubsystemError::ForcedShutdown);
    }

    async fn local_shutdown_timed_out<ChildT, Err>(
        join_handle: tokio::task::JoinHandle<Result<ChildT, Err>>,
        id: &SubsystemId,
    ) where
        Err: Debug + Display + Send + Sync + 'static,
    {
        warn!("Subsystem '{}' is being shut down forcefully.", id);
        join_handle.abort();
    }
}

impl<T: Send + Sync + 'static> Subsystem<T> {
    pub fn request_local_shutdown(&self) {
        info!("Local shutdown requested for subsystem '{}'", self.id);
        self.local.cancel();
        let id = self.id.clone();

        if let Some(shutdown_timeout) = self.shutdown_timeout {
            let task_name = format!("Shutdown monitor: {}", id.name);
            spawn_task(
                {
                    let task_id = task_id();
                    let event = MetricsEvent::TaskSpawned(TaskInfo {
                        id: task_id,
                        task_type: TaskType::LocalShutdownTimeout(id),
                    });
                    trace!(metrics_event = %event);
                    local_shutdown_timeout(
                        self.join_handle.1.clone(),
                        self.id.clone(),
                        self.cancel_clean_local_shutdown.clone(),
                        shutdown_timeout,
                    )
                },
                &task_name,
            );
        }
    }
}

impl<T: Clone> Subsystem<T> {
    pub async fn join(&self) -> Result<T, SubsystemError> {
        let mut join_handle = self.join_handle.1.clone();

        if join_handle.wait_for(|it| it.is_some()).await.is_err() {
            return Err(SubsystemError::ForcedShutdown);
        }

        join_handle
            .borrow()
            .clone()
            .expect("completed subsystem went back to running")
    }
}

async fn local_shutdown_timeout<T>(
    mut join_handle: watch::Receiver<Option<Result<T, SubsystemError>>>,
    id: SubsystemId,
    cancel_clean_shutdown: CancellationToken,
    shutdown_timeout: Duration,
) {
    let complete = join_handle.wait_for(|it| it.is_some());
    let timed_out = timeout(shutdown_timeout, complete).await.is_err();
    if timed_out {
        error!(
            "Local shutdown timeout of subsystem '{}' reached, forcing shutdown …",
            id
        );
        cancel_clean_shutdown.cancel();
    }
}

pub fn build_root(name: impl Into<String>) -> RootBuilder {
    RootBuilder {
        name: name.into(),
        catch_signals: false,
        shutdown_timeout: None,
        shutdown_on_stdin_close: false,
        stdin_consumer: None,
    }
}

pub fn build_default_root(name: impl Into<String>) -> RootBuilder {
    RootBuilder {
        name: name.into(),
        catch_signals: true,
        shutdown_timeout: Some(Duration::from_secs(1)),
        shutdown_on_stdin_close: false,
        stdin_consumer: None,
    }
}

fn task_id() -> u64 {
    to_u64(task::id())
}

fn to_u64(id: task::Id) -> u64 {
    id.to_string().parse().expect("task ID is not a valid u64")
}

#[cfg(feature = "tokio_unstable")]
pub fn spawn_task<F>(future: F, name: &str) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let builder = task::Builder::new();
    builder
        .name(name)
        .spawn(future)
        .expect("Failed to spawn task")
}

#[cfg(not(feature = "tokio_unstable"))]
pub fn spawn_task<F>(future: F, _name: &str) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

pub trait CancelOnShutdown {
    type Output;
    fn or_cancel_on_shutdown<T>(
        self,
        subsystem: &Subsystem<T>,
    ) -> impl Future<Output = Option<Self::Output>>;
}

impl<F: Future> CancelOnShutdown for F {
    type Output = F::Output;

    async fn or_cancel_on_shutdown<T>(self, subsystem: &Subsystem<T>) -> Option<Self::Output> {
        select! {
            _ = subsystem.shutdown_requested() => None,
            output = self => Some(output),
        }
    }
}
