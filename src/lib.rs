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

use miette::Diagnostic;
use std::{
    collections::HashMap,
    fmt::{self, Debug, Display},
    io::stdin,
    mem,
    process::{self, ExitCode},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    select, spawn,
    sync::{oneshot, watch},
    task::JoinError,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

type SubsystemMap = Arc<Mutex<HashMap<String, oneshot::Receiver<()>>>>;

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

impl RootBuilder {
    pub async fn start<E, F>(
        mut self,
        subsys: impl FnOnce(Subsystem) -> F + Send + 'static,
    ) -> SubsystemResult
    where
        F: std::future::Future<Output = Result<(), E>> + Send + 'static,
        E: IntoGenericError + Display,
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

        let cancel_clean_shutdown = CancellationToken::new();

        let subsystems = Arc::new(Mutex::new(HashMap::new()));

        let handle = Subsystem {
            name: self.name.clone(),
            global: global.clone(),
            local: local.clone(),
            cancel_clean_shutdown: cancel_clean_shutdown.clone(),
            subsystems: subsystems.clone(),
            crash: crash.clone(),
            join_handle: (join_tx.clone(), join_rx),
        };

        let glob = global.clone();
        if let Some(to) = self.shutdown_timeout {
            spawn(async move {
                match subsys(handle).await {
                    Ok(_) => info!("Root system '{}' terminated normally.", self.name),
                    Err(e) => {
                        error!("Root system '{}' terminated with error: {e}", self.name);
                        crash.set_crash(SubsystemError::Error(
                            self.name.clone(),
                            e.into_generic_error(),
                        ));
                    }
                }

                glob.cancel();
                info!(
                    "Shutdown initiated, waiting for clean shutdown for up to {:?}.",
                    to
                );

                let subsystems = {
                    let mut subsystems = subsystems.lock().expect("mutex is poisoned");
                    subsystems.drain().collect()
                };
                let subsys_shutdown_future = wait_for_subsystems_shutdown(subsystems);

                match timeout(to, subsys_shutdown_future).await {
                    Ok(_) => {
                        info!("All subsystems have shut down in time.");
                    }
                    Err(_) => {
                        error!("Shutdown timeout reached, forcing shutdown …");
                        cancel_clean_shutdown.cancel();
                        crash.set_crash(SubsystemError::ForcedShutdown);
                    }
                }

                res_tx.send(crash.take_crash()).ok();
                join_tx.send(Some(Err(SubsystemError::ForcedShutdown))).ok();
            });
        } else {
            spawn(async move {
                match subsys(handle).await {
                    Ok(_) => info!("Root system '{}' terminated normally.", self.name),
                    Err(e) => error!("Root system '{}' terminated with error: {e}", self.name),
                }

                if !global.is_cancelled() {
                    glob.cancel();
                }
                info!("Shutdown initiated, waiting for clean shutdown.");

                let subsystems = {
                    let mut subsystems = subsystems.lock().expect("mutex is poisoned");
                    subsystems.drain().collect()
                };
                let subsys_shutdown_future = wait_for_subsystems_shutdown(subsystems);
                subsys_shutdown_future.await;
                info!("All subsystems have shut down.");

                res_tx.send(crash.take_crash()).ok();
                join_tx
                    .send(Some(Err(SubsystemError::OrderlyShutdown)))
                    .ok();
            });
        }

        res_rx.await.unwrap_or(Err(SubsystemError::ForcedShutdown))
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
        spawn(async move {
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
        });
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
                    crash.set_exit_code(ExitCode::from(1));
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
    spawn(async move {
        let mut counter = 0;
        loop {
            signal.recv().await;
            counter += 1;
            if counter > 1 {
                break;
            }
            info!("Received {signal_name} signal, initiating shutdown.");
            crash.set_exit_code(ExitCode::from(128 + code as u8));
            global.cancel();
        }
        process::exit(128 + code);
    });
}

#[derive(Debug, Clone, Error, Diagnostic)]
pub enum SubsystemError {
    #[error("Subsystem '{0}' terminated with error: {1}")]
    Error(String, GenericError),
    #[error("Subsystem '{0}' panicked: {1}")]
    Panic(String, String),
    #[error("Subsystem did not complete because it was asked to shut down")]
    OrderlyShutdown,
    #[error("Subsystem did not complete because it was forced to shut down")]
    ForcedShutdown,
    #[error("{0}")]
    Custom(String),
}

pub trait GenErr: Debug + Display + Send + Sync + 'static {}

impl<E> GenErr for E where E: Debug + Display + Send + Sync + 'static {}

#[derive(Clone)]
pub struct GenericError(Arc<dyn GenErr>);

impl From<miette::Report> for GenericError {
    fn from(err: miette::Report) -> Self {
        GenericError(Arc::new(err))
    }
}

pub trait IntoGenericError {
    fn into_generic_error(self) -> GenericError;
}

pub trait IntoSubsystemResult<T> {
    fn into_subsystem_result(self, message: impl Into<String>) -> Result<T, SubsystemError>;
}

impl<T, E: IntoGenericError> IntoSubsystemResult<T> for Result<T, E> {
    fn into_subsystem_result(self, message: impl Into<String>) -> Result<T, SubsystemError> {
        match self {
            Ok(it) => Ok(it),
            Err(e) => Err(SubsystemError::Error(
                message.into(),
                e.into_generic_error(),
            )),
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

pub type SubsystemResult = Result<ExitCode, SubsystemError>;

async fn wait_for_subsystems_shutdown(subsystems: HashMap<String, oneshot::Receiver<()>>) {
    for rx in subsystems.into_values() {
        rx.await.ok();
    }
}

pub struct Subsystem<T = ()> {
    name: String,
    local: CancellationToken,
    global: CancellationToken,
    cancel_clean_shutdown: CancellationToken,
    subsystems: SubsystemMap,
    crash: CrashHolder,
    join_handle: (
        watch::Sender<Option<Result<T, SubsystemError>>>,
        watch::Receiver<Option<Result<T, SubsystemError>>>,
    ),
}

impl<T> Clone for Subsystem<T> {
    fn clone(&self) -> Self {
        Subsystem {
            name: self.name.clone(),
            local: self.local.clone(),
            global: self.global.clone(),
            cancel_clean_shutdown: self.cancel_clean_shutdown.clone(),
            subsystems: self.subsystems.clone(),
            crash: self.crash.clone(),
            join_handle: (self.join_handle.0.clone(), self.join_handle.1.clone()),
        }
    }
}

impl<T> fmt::Debug for Subsystem<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubsystemHandle")
            .field("name", &self.name)
            .finish()
    }
}

fn convert_result<T, Err>(res: Result<T, Err>) -> Result<T, GenericError>
where
    Err: IntoGenericError,
{
    match res {
        Ok(it) => Ok(it),
        Err(e) => Err(e.into_generic_error()),
    }
}

impl<T: Clone + Send> Subsystem<T> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn spawn<ChildT, Err, F>(
        &self,
        name: impl AsRef<str>,
        subsys: impl FnOnce(Subsystem<ChildT>) -> F + Send + 'static,
    ) -> Subsystem<ChildT>
    where
        ChildT: Clone + Send + Sync + 'static,
        F: Future<Output = Result<ChildT, Err>> + Send + 'static,
        Err: IntoGenericError,
    {
        let cancel_clean_shutdown = self.cancel_clean_shutdown.clone();

        let handle = self.create_child(name, cancel_clean_shutdown.clone());
        let full_name = handle.name().to_owned();

        let fname = full_name.clone();
        let subsystems = self.subsystems.clone();
        let mut crash = self.crash.clone();
        let glob = self.global.clone();
        let h = handle.clone();
        let res_tx = h.join_handle.0.clone();
        info!("Spawning subsystem '{}' …", fname);
        tokio::spawn(async move {
            let name = fname.clone();
            let mut join_handle = tokio::spawn(async move {
                info!("Subsystem '{}' started.", name);
                let res = subsys(h).await;
                convert_result(res)
            });
            select! {
                res = &mut join_handle => Self::subsystem_joined(res, subsystems, &fname, &mut crash, res_tx).await,
                _ = cancel_clean_shutdown.cancelled() => Self::shutdown_timed_out(join_handle, &fname, &glob, &mut crash).await,
            };
        });

        handle
    }

    pub fn request_global_shutdown(&self) {
        self.global.cancel();
    }

    pub fn request_local_shutdown(&self) {
        self.local.cancel();
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

    fn create_child<ChildT: Send + Sync + 'static>(
        &self,
        name: impl AsRef<str>,
        cancel_clean_shutdown: CancellationToken,
    ) -> Subsystem<ChildT> {
        let (res_tx, res_rx) = watch::channel::<Option<Result<ChildT, SubsystemError>>>(None);
        let name = format!("{}/{}", self.name, name.as_ref());
        let global = self.global.clone();
        let local = self.local.child_token();
        let subsystems = self.subsystems.clone();
        let crash = self.crash.clone();

        let mut gc = self.subsystems.lock().expect("mutex is poisoned");
        let (gc_tx, gc_rx) = oneshot::channel();
        let mut gc_res_rx = res_rx.clone();
        spawn(async move {
            gc_res_rx.wait_for(|it| it.is_some()).await.ok();
            gc_tx.send(()).ok();
        });
        gc.insert(name.clone(), gc_rx);

        Subsystem {
            name,
            global,
            local,
            cancel_clean_shutdown,
            subsystems,
            crash,
            join_handle: (res_tx, res_rx),
        }
    }

    async fn subsystem_joined<ChildT: Clone + Send + Sync>(
        res: Result<Result<ChildT, GenericError>, JoinError>,
        subsystems: SubsystemMap,
        subsystem_name: &str,
        crash: &mut CrashHolder,
        res_tx: watch::Sender<Option<Result<ChildT, SubsystemError>>>,
    ) {
        let mut gc = subsystems.lock().expect("mutex is poisoned");
        gc.remove(subsystem_name);

        debug!(
            "Subsystem '{}' removed. Remaining subsystems: {:?}",
            subsystem_name,
            gc.keys()
        );

        match res {
            Ok(Ok(it)) => {
                info!("Subsystem '{}' terminated normally.", subsystem_name);
                res_tx.send(Some(Ok(it))).ok();
            }
            Ok(Err(e)) => {
                error!(
                    "Subsystem '{}' terminated with error: {}",
                    subsystem_name, e
                );
                let err = SubsystemError::Error(subsystem_name.to_owned(), e);
                crash.set_crash(err.clone());
                res_tx.send(Some(Err(err))).ok();
            }
            Err(e) => {
                if e.is_panic() {
                    error!("Subsystem '{}' panicked: {}", subsystem_name, e);
                    let err = SubsystemError::Panic(subsystem_name.to_owned(), e.to_string());
                    crash.set_crash(err.clone());
                    res_tx.send(Some(Err(err))).ok();
                } else {
                    warn!("Subsystem '{}' was shut down forcefully.", subsystem_name);
                    let err = SubsystemError::ForcedShutdown;
                    crash.set_crash(err.clone());
                    res_tx.send(Some(Err(err))).ok();
                }
            }
        };
    }

    async fn shutdown_timed_out<ChildT, Err>(
        join_handle: tokio::task::JoinHandle<Result<ChildT, Err>>,
        subsystem_name: &str,
        global: &CancellationToken,
        crash: &mut CrashHolder,
    ) where
        Err: Debug + Display + Send + Sync + 'static,
    {
        warn!(
            "Subsystem '{}' is being shut down forcefully.",
            subsystem_name
        );
        join_handle.abort();
        global.cancel();
        crash.set_crash(SubsystemError::ForcedShutdown);
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
