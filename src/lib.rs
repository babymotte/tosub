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

use miette::{Diagnostic, IntoDiagnostic, miette};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    any::Any,
    collections::BTreeMap,
    convert::Infallible,
    fmt::{self, Debug, Display},
    io::stdin,
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
use tracing::{debug, error, info, trace, warn};

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsystemId {
    name: String,
    task_id: u64,
    parent: Option<Arc<SubsystemId>>,
}

impl fmt::Debug for SubsystemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.task_id, self.path())
    }
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
    fn unassigned_root(name: String) -> Self {
        SubsystemId {
            name,
            task_id: 0,
            parent: None,
        }
    }

    fn unassigned_child(&self, name: String) -> Self {
        SubsystemId {
            name,
            task_id: 0,
            parent: Some(Arc::new(self.clone())),
        }
    }

    fn set_task_id(&mut self, task_id: u64) {
        self.task_id = task_id;
    }

    fn path(&self) -> String {
        let mut path = self.path_iter().collect::<Vec<_>>();
        path.reverse();
        path.join("/")
    }

    fn path_iter<'a>(&'a self) -> IdPathIter<'a> {
        IdPathIter(Some(self))
    }

    fn mock(name: String) -> SubsystemId {
        SubsystemId {
            name,
            task_id: 0,
            parent: None,
        }
    }
}

struct IdPathIter<'a>(Option<&'a SubsystemId>);

impl<'a> Iterator for IdPathIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0 {
            Some(it) => {
                let name = it.name.as_str();
                self.0 = it.parent.as_deref();
                Some(name)
            }
            None => None,
        }
    }
}

impl fmt::Display for SubsystemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path())
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

impl fmt::Display for MetricsEvent {
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
    crash: Arc<Mutex<Option<RootSystemResult>>>,
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
    fn set_exit_code(&self, code: ExitCode, cause: &SubsystemId) {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        if guard.is_none() {
            *guard = Some(Ok(code));
            info!(
                "Requesting global shutdown because subsystem {} terminated with non-zero exit code.",
                cause
            );
            self.cancel.cancel();
        }
    }

    fn set_crash(&self, err: RootSystemError) {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        if guard.is_none() {
            let msg = err.to_string();
            *guard = Some(Err(err));
            error!("Requesting global shutdown due to crash: {}", msg);
            self.cancel.cancel();
        }
    }

    fn take_crash(&self) -> Option<RootSystemResult> {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        (*guard).replace(Ok(ExitCode::SUCCESS))
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

pub type ExitCodeResult = GenericResult<ExitCode>;

pub trait IntoExitCodeResult {
    type E: IntoGenericError;
    fn into_exit_code_result(self) -> ExitCodeResult;
}

impl<C: IntoExitCode> IntoExitCodeResult for C {
    type E = Infallible;

    fn into_exit_code_result(self) -> ExitCodeResult {
        Ok(self.into_exit_code())
    }
}

impl<C: IntoExitCode> IntoExitCodeResult for Option<C> {
    type E = Infallible;

    fn into_exit_code_result(self) -> ExitCodeResult {
        Ok(self.map_or(ExitCode::SUCCESS, C::into_exit_code))
    }
}

impl<E: IntoGenericError> IntoExitCodeResult for std::result::Result<(), E> {
    type E = E;

    fn into_exit_code_result(self) -> ExitCodeResult {
        match self {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(err) => Err(err.into_generic_error()),
        }
    }
}

impl RootBuilder {
    pub async fn start<R, F>(
        self,
        subsys: impl FnOnce(Subsystem) -> F + Send + 'static,
    ) -> miette::Result<ExitCode>
    where
        F: std::future::Future<Output = R> + Send + 'static,
        R: IntoExitCodeResult,
    {
        self.start_raw(subsys).await.into_diagnostic()
    }

    pub async fn start_raw<R, F>(
        mut self,
        subsys: impl FnOnce(Subsystem) -> F + Send + 'static,
    ) -> RootSystemResult
    where
        F: std::future::Future<Output = R> + Send + 'static,
        R: IntoExitCodeResult,
    {
        let global = CancellationToken::new();
        let local = global.child_token();

        let crash = CrashHolder {
            crash: Arc::new(Mutex::new(None)),
            cancel: global.clone(),
        };

        if self.catch_signals {
            self.register_signal_handlers(crash.clone());
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

        let subsystems = GC::default();

        let handle = Subsystem {
            id: SubsystemId::unassigned_root(self.name.clone()),
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
                let subsystems = subsystems.take_remaining_subsystems();
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
                            crash.set_crash(RootSystemError::ForcedShutdown);
                        }
                    }

                    // trigger join handle of root system
                    let _ = res_tx.send(crash.take_crash());
                    let _ = join_tx.send(Some(Err(RootSystemError::ForcedShutdown)));
                } else {
                    info!("Shutdown initiated, waiting for clean shutdown.");

                    // wait for subsystems shutting down
                    subsys_shutdown_future.await;
                    info!("All subsystems have shut down.");

                    let _ = res_tx.send(crash.take_crash());
                    let _ = join_tx.send(Some(Err(RootSystemError::OrderlyShutdown)));
                }
            },
            &task_name,
        );

        // block on root system to run and clean up
        res_rx
            .await
            .unwrap_or(Some(Err(RootSystemError::ForcedShutdown)))
            .unwrap_or(Ok(ExitCode::SUCCESS))
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
        match subsys(handle).await.into_exit_code_result() {
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
                crash.set_exit_code(exit_code, &id);
            }
            Err(generic_error) => {
                let event = MetricsEvent::RootSystemStopped {
                    id: id.clone(),
                    outcome: Outcome::TerminatedWithError(generic_error.to_string()),
                };
                trace!(metrics_event = %event);
                crash.set_crash(RootSystemError::Error(id, generic_error));
            }
        }

        // root system completed, request shutdown of all subsystems

        if !global.is_cancelled() {
            info!("Root system completed, initiating shutdown of all subsystems.");
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
    fn register_signal_handlers(&self, crash: CrashHolder) {
        use tokio::signal::ctrl_c;

        let task_name = format!("Signal handler: Ctrl+C");
        let task_id = spawn_task(
            async move {
                let mut counter = 0;
                loop {
                    ctrl_c().await.expect("Ctrl+C handler not supported");
                    counter += 1;
                    if counter > 1 {
                        break;
                    }
                    info!("Received Ctrl+C, initiating shutdown.");
                    crash.set_exit_code(
                        ExitCode::from(1),
                        &SubsystemId::mock("Signal handler (Ctrl+C)".to_owned()),
                    );
                }
                process::exit(1);
            },
            &task_name,
        )
        .id();
        let event = MetricsEvent::TaskSpawned(TaskInfo {
            id: to_u64(task_id),
            task_type: TaskType::SignalHandler("Ctrl+C".to_owned()),
        });
        trace!(metrics_event = %event);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
    fn register_signal_handlers(&self, crash: CrashHolder) {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(signal) = signal(SignalKind::hangup()) {
            handle_unix_signal(
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
                    crash.set_exit_code(
                        ExitCode::FAILURE,
                        &SubsystemId::mock("Stdin handler".to_owned()),
                    );
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
    mut signal: tokio::signal::unix::Signal,
    signal_name: &'static str,
    code: i32,
    crash: CrashHolder,
) {
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
                crash.set_exit_code(
                    ExitCode::from(128 + code as u8),
                    &SubsystemId::mock(format!("Signal handler ({code})")),
                );
            }
            process::exit(128 + code);
        },
        &task_name,
    );
}

#[derive(Clone, Debug, Error, Diagnostic)]
pub enum RootSystemError {
    #[error("Subsystem '{0}' terminated with error")]
    Error(
        SubsystemId,
        #[source]
        #[diagnostic_source]
        GenericError,
    ),
    #[error("Subsystem '{0}' panicked")]
    Panic(
        SubsystemId,
        #[source]
        #[diagnostic_source]
        GenericError,
    ),
    #[error("Subsystem did not complete because it was asked to shut down")]
    OrderlyShutdown,
    #[error("Subsystem did not complete because it was forced to shut down")]
    ForcedShutdown,
}

type BoxedStdError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Anything that can be turned into a boxed [`std::error::Error`]: every error type, `String`,
/// `&str` and [`miette::Report`]. Going through `std::error::Error` keeps the cause chain intact.
pub trait GenErr: Into<BoxedStdError> + Send + Sync + 'static {}

impl<E> GenErr for E where E: Into<BoxedStdError> + Send + Sync + 'static {}

// Box<dyn Error> does not implement Error itself, so this forwards Display, Debug and source().
struct BoxedErrorWrapper(BoxedStdError);

impl fmt::Debug for BoxedErrorWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for BoxedErrorWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for BoxedErrorWrapper {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

#[derive(Clone)]
pub struct GenericError(Arc<miette::Report>);

type GenericResult<T> = std::result::Result<T, GenericError>;

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

// GenericError is a transparent proxy: its Display already is the wrapped message, so
// source()/diagnostic_source() continue from the wrapped report's own chain instead of adding a node.
impl std::error::Error for GenericError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl Diagnostic for GenericError {
    fn code<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.0.code()
    }

    fn severity(&self) -> Option<miette::Severity> {
        self.0.severity()
    }

    fn help<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.0.help()
    }

    fn url<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.0.url()
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        self.0.source_code()
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        self.0.labels()
    }

    fn related<'a>(&'a self) -> Option<Box<dyn Iterator<Item = &'a dyn Diagnostic> + 'a>> {
        self.0.related()
    }

    fn diagnostic_source(&self) -> Option<&dyn Diagnostic> {
        self.0.diagnostic_source()
    }
}

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
    fn into_generic_result(self) -> std::result::Result<Self::Output, GenericError>;
}

impl IntoGenericResult for Option<()> {
    type Output = ();

    fn into_generic_result(self) -> std::result::Result<Self::Output, GenericError> {
        Ok(())
    }
}

impl<T> IntoGenericResult for Option<Subsystem<T>>
where
    T: Send + Sync + 'static,
{
    type Output = Subsystem<T>;

    fn into_generic_result(self) -> std::result::Result<Self::Output, GenericError> {
        self.ok_or_else(|| {
            miette!("Could not spawn child subsystem because shutdown has been requested")
                .into_generic_error()
        })
    }
}

impl<T: Clone + Send + Sync + 'static, Err> IntoGenericResult for std::result::Result<T, Err>
where
    Err: IntoGenericError,
{
    type Output = T;

    fn into_generic_result(self) -> std::result::Result<Self::Output, GenericError> {
        match self {
            Ok(it) => Ok(it),
            Err(e) => Err(e.into_generic_error()),
        }
    }
}

impl IntoGenericResult for () {
    type Output = ();

    fn into_generic_result(self) -> std::result::Result<Self::Output, GenericError> {
        Ok(())
    }
}

pub trait IntoRootSystemResult<T> {
    fn into_subsystem_result(self, id: SubsystemId) -> std::result::Result<T, RootSystemError>;
}

impl<T, E: IntoGenericError> IntoRootSystemResult<T> for std::result::Result<T, E> {
    fn into_subsystem_result(self, id: SubsystemId) -> std::result::Result<T, RootSystemError> {
        match self {
            Ok(it) => Ok(it),
            Err(e) => Err(RootSystemError::Error(id, e.into_generic_error())),
        }
    }
}

impl<E: GenErr> IntoGenericError for E {
    fn into_generic_error(self) -> GenericError {
        // No specialization available, so detect miette::Report at runtime to keep its diagnostics.
        let mut slot = Some(self);
        if let Some(report) = (&mut slot as &mut dyn Any)
            .downcast_mut::<Option<miette::Report>>()
            .and_then(Option::take)
        {
            return report.into();
        }
        let err = slot.expect("slot is only emptied on the Report path");
        let report = Err::<(), _>(BoxedErrorWrapper(err.into()))
            .into_diagnostic()
            .expect_err("we explicitly constructed an Err here, so it can't be an Ok");
        report.into()
    }
}

pub type RootSystemResult<T = ExitCode> = std::result::Result<T, RootSystemError>;

async fn wait_for_subsystems_shutdown(subsystems: BTreeMap<SubsystemId, oneshot::Receiver<()>>) {
    for rx in subsystems.into_values() {
        let _ = rx.await;
    }
}

type ResultWatchSender<T> = watch::Sender<Option<RootSystemResult<T>>>;
type ResultWatchReceiver<T> = watch::Receiver<Option<RootSystemResult<T>>>;

type ResultWatchChannel<T> = (ResultWatchSender<T>, ResultWatchReceiver<T>);

#[derive(Debug, Clone, Default)]
struct GC(SubsystemMap);

impl GC {
    /// Registers a subsystem. Returns a snapshot of all running subsystems only if
    /// it is going to be logged, since taking it is O(n) while holding the global lock.
    fn insert_and_get_all(
        &self,
        subsystem_id: SubsystemId,
        gc_rx: oneshot::Receiver<()>,
    ) -> Option<Box<[SubsystemId]>> {
        let snapshot = tracing::enabled!(tracing::Level::DEBUG);
        let mut gc = self.0.lock().expect("mutex is poisoned");
        gc.insert(subsystem_id, gc_rx);
        snapshot.then(|| gc.keys().cloned().collect::<Box<[_]>>())
    }

    /// Unregisters a subsystem. Returns a snapshot of the remaining subsystems only if
    /// it is going to be logged, since taking it is O(n) while holding the global lock.
    fn remove_and_get_remaining(&self, id: &SubsystemId) -> Option<Box<[SubsystemId]>> {
        let snapshot = tracing::enabled!(tracing::Level::DEBUG);
        let mut gc = self.0.lock().expect("mutex is poisoned");
        gc.remove(id);
        snapshot.then(|| gc.keys().cloned().collect::<Box<[_]>>())
    }

    fn take_remaining_subsystems(self) -> BTreeMap<SubsystemId, oneshot::Receiver<()>> {
        let mut gc = self.0.lock().expect("mutex is poisoned");
        std::mem::take(&mut *gc)
    }
}

/// Logs the termination of a subsystem along with the list of remaining subsystems.
/// `all_running` is only `Some` if debug logging is enabled, see [`GC::remove_and_get_remaining`].
fn log_subsystem_stopped(
    id: &SubsystemId,
    outcome: impl FnOnce() -> Outcome,
    all_running: Option<Box<[SubsystemId]>>,
    what: impl Display,
) {
    let Some(all_running) = all_running else {
        return;
    };
    debug!(
        "Subsystem '{}' {what}. List of all remaining subsytems: {:#?}",
        id.name, all_running
    );
    let event = MetricsEvent::SubsystemStopped {
        id: id.clone(),
        outcome: outcome(),
        all_running,
    };
    trace!(metrics_event = %event);
}

pub struct Subsystem<T = ()> {
    id: SubsystemId,
    local: CancellationToken,
    global: CancellationToken,
    cancel_clean_global_shutdown: CancellationToken,
    cancel_clean_local_shutdown: CancellationToken,
    subsystems: GC,
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
        self.request_global_shutdown_because("global shutdown explicitly requested");
    }

    pub fn request_global_shutdown_because(&self, msg: impl Display) {
        info!(
            "Global shutdown requested from subsystem '{}': {}",
            self.id, msg
        );
        self.global.cancel();
    }

    pub fn spawn<ChildT>(
        &self,
        name: impl Display,
        subsys: impl SubsystemFunction<ChildT>,
    ) -> Option<Subsystem<ChildT>>
    where
        ChildT: Clone + Send + Sync + 'static,
    {
        if self.is_shut_down() {
            warn!(
                "Cannot spawn new subsystem '{}/{}' because parent subsystem '{}' has shut down.",
                self.id.path(),
                name,
                self.id.path()
            );
            return None;
        }

        let cancel_clean_global_shutdown = self.cancel_clean_global_shutdown.clone();
        let cancel_clean_local_shutdown = CancellationToken::new();

        let mut handle = self.create_child(
            name,
            cancel_clean_global_shutdown.clone(),
            cancel_clean_local_shutdown.clone(),
        );
        let (gc_tx, gc_rx) = oneshot::channel();
        let full_name = handle.id.path();
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
                    biased;
                    res = &mut join_handle => Self::subsystem_joined(res, subsystems, subsystem_id_2, &mut crash, res_tx, gc_tx).await,
                    _ = cancel_clean_global_shutdown.cancelled() => Self::global_shutdown_timed_out(join_handle, subsystem_id_2, &glob, &mut crash).await,
                    _ = cancel_clean_local_shutdown.cancelled() => Self::subsystem_timed_out(join_handle, subsystems, subsystem_id_2, res_tx, gc_tx).await,
                };
            },
            &cancellation_task_name,
        );

        if let Some(all_running) = self
            .subsystems
            .insert_and_get_all(subsystem_id.clone(), gc_rx)
        {
            debug!(
                "Subsystem '{}' started. List of all running subsytems: {:#?}",
                subsystem_id.name, all_running
            );
            let event = MetricsEvent::SubsystemStarted {
                id: subsystem_id,
                all_running,
            };
            trace!(metrics_event = %event);
        }

        Some(handle)
    }

    fn create_child<ChildT: Send + Sync + 'static>(
        &self,
        name: impl Display,
        cancel_clean_global_shutdown: CancellationToken,
        cancel_clean_local_shutdown: CancellationToken,
    ) -> Subsystem<ChildT> {
        let (res_tx, res_rx) =
            watch::channel::<Option<std::result::Result<ChildT, RootSystemError>>>(None);
        let name = name.to_string();
        let global = self.global.clone();
        let local = self.local.child_token();
        let subsystems = self.subsystems.clone();
        let crash = self.crash.clone();
        let id = self.id.unassigned_child(name.clone());

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
        }
    }

    async fn subsystem_joined<ChildT: Send + Sync>(
        res: std::result::Result<GenericResult<ChildT>, JoinError>,
        subsystems: GC,
        id: SubsystemId,
        crash: &mut CrashHolder,
        res_tx: ResultWatchSender<ChildT>,
        gc_tx: oneshot::Sender<()>,
    ) {
        trace!("tokio task of subsystem '{}' joined", id.path());
        let _ = gc_tx.send(());

        let all_running = subsystems.remove_and_get_remaining(&id);

        match res {
            Ok(Ok(it)) => {
                log_subsystem_stopped(
                    &id,
                    || Outcome::TerminatedNormally,
                    all_running,
                    "terminated normally",
                );
                let _ = res_tx.send(Some(Ok(it)));
            }
            Ok(Err(e)) => {
                log_subsystem_stopped(
                    &id,
                    || Outcome::TerminatedWithError(e.to_string()),
                    all_running,
                    format_args!("terminated with error: {e}"),
                );
                let err = RootSystemError::Error(id, e);
                crash.set_crash(err.clone());
                let _ = res_tx.send(Some(Err(err)));
            }
            Err(e) => {
                if e.is_panic() {
                    error!("Subsystem '{}' panicked: {}", id.name, e);
                    log_subsystem_stopped(
                        &id,
                        || Outcome::TerminatedWithError(e.to_string()),
                        all_running,
                        format_args!("panicked: {e}"),
                    );
                    let err = RootSystemError::Panic(id, miette::Report::from_err(e).into());
                    crash.set_crash(err.clone());
                    let _ = res_tx.send(Some(Err(err)));
                } else {
                    warn!("Subsystem '{}' was shut down forcefully.", id.name);
                    log_subsystem_stopped(
                        &id,
                        || Outcome::ShutdownForced,
                        all_running,
                        "was shut down forcefully",
                    );
                    let err = RootSystemError::ForcedShutdown;
                    crash.set_crash(err.clone());
                    let _ = res_tx.send(Some(Err(err)));
                }
            }
        };
    }

    async fn subsystem_timed_out<ChildT, Err>(
        join_handle: tokio::task::JoinHandle<std::result::Result<ChildT, Err>>,
        subsystems: GC,
        id: SubsystemId,
        res_tx: watch::Sender<Option<std::result::Result<ChildT, RootSystemError>>>,
        gc_tx: oneshot::Sender<()>,
    ) where
        Err: Debug + Display + Send + Sync + 'static,
    {
        let _ = gc_tx.send(());

        Self::local_shutdown_timed_out(join_handle, &id).await;

        let all_running = subsystems.remove_and_get_remaining(&id);

        warn!("Subsystem '{}' was shut down forcefully.", id);

        log_subsystem_stopped(
            &id,
            || Outcome::ShutdownForced,
            all_running,
            "was shut down forcefully",
        );

        let _ = res_tx.send(Some(Err(RootSystemError::ForcedShutdown)));
    }

    async fn global_shutdown_timed_out<ChildT, Err>(
        join_handle: tokio::task::JoinHandle<std::result::Result<ChildT, Err>>,
        id: SubsystemId,
        global: &CancellationToken,
        crash: &mut CrashHolder,
    ) where
        Err: Debug + Display + Send + Sync + 'static,
    {
        warn!("Subsystem '{}' is being shut down forcefully.", id);
        join_handle.abort();
        global.cancel();
        crash.set_crash(RootSystemError::ForcedShutdown);
    }

    async fn local_shutdown_timed_out<ChildT, Err>(
        join_handle: tokio::task::JoinHandle<std::result::Result<ChildT, Err>>,
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
        self.request_local_shutdown_because("local shutdown explicitly requested");
    }

    pub fn request_local_shutdown_because(&self, msg: impl Display) {
        info!(
            "Local shutdown requested for subsystem '{}': {}",
            self.id, msg
        );
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
    pub async fn join(&self) -> std::result::Result<T, RootSystemError> {
        let mut join_handle = self.join_handle.1.clone();

        if join_handle.wait_for(|it| it.is_some()).await.is_err() {
            return Err(RootSystemError::ForcedShutdown);
        }

        join_handle
            .borrow()
            .clone()
            .expect("completed subsystem went back to running")
    }
}

async fn local_shutdown_timeout<T>(
    mut join_handle: watch::Receiver<Option<std::result::Result<T, RootSystemError>>>,
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
            biased;
            _ = subsystem.shutdown_requested() => None,
            output = self => Some(output),
        }
    }
}

#[cfg(test)]
mod generic_error_tests {

    use super::*;
    use miette::{Context, IntoDiagnostic, bail};
    use std::future::pending;

    fn chained() -> miette::Result<()> {
        let inner: miette::Result<()> = (|| {
            bail!("aargh");
        })();
        inner
            .wrap_err("failed to do the first thing")
            .wrap_err("failed to do the second thing")
    }

    #[test]
    fn report_chain_survives_into_diagnostic() {
        let err = chained().expect_err("chain must fail").into_generic_error();
        let root: std::result::Result<(), _> = Err(RootSystemError::Error(
            SubsystemId::unassigned_root("test".into()),
            err,
        ));
        let rendered = format!("{:?}", root.into_diagnostic().expect_err("must be Err"));
        for needle in ["second thing", "first thing", "aargh"] {
            assert!(
                rendered.contains(needle),
                "missing {needle:?} in:\n{rendered}"
            );
        }
    }

    #[derive(Debug, Error)]
    enum StdChain {
        #[error("io failed")]
        Io(#[source] std::io::Error),
        #[error("{0}")]
        Wrapped(&'static str, #[source] Box<StdChain>),
    }

    #[test]
    fn std_error_chain_survives() {
        let err = StdChain::Wrapped(
            "failed to do the second thing",
            Box::new(StdChain::Wrapped(
                "failed to do the first thing",
                Box::new(StdChain::Io(std::io::Error::other("aargh"))),
            )),
        )
        .into_generic_error();
        let root: std::result::Result<(), _> = Err(RootSystemError::Error(
            SubsystemId::unassigned_root("test".into()),
            err,
        ));
        let rendered = format!("{:?}", root.into_diagnostic().expect_err("must be Err"));
        for needle in ["second thing", "first thing", "io failed", "aargh"] {
            assert!(
                rendered.contains(needle),
                "missing {needle:?} in:\n{rendered}"
            );
        }
    }

    #[test]
    fn opaque_errors_still_work() {
        let err = "just a string".to_string().into_generic_error();
        assert_eq!(err.to_string(), "just a string");
        assert!(std::error::Error::source(&err).is_none());
    }

    #[tokio::test]
    async fn aborted_task_still_joins() {
        let handle = tokio::spawn(async {
            pending::<()>().await;
        });

        handle.abort();

        timeout(Duration::from_secs(1), handle)
            .await
            .expect("this should not time out")
            .expect_err("this should fail because the task was cancelled");
    }
}
