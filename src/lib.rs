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
    mem, process,
    sync::{Arc, Mutex},
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

type SubsystemMap = Arc<Mutex<HashMap<String, (watch::Sender<bool>, watch::Receiver<bool>)>>>;

pub struct RootBuilder {
    name: String,
    catch_signals: bool,
    shutdown_timeout: Option<std::time::Duration>,
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
    fn set_crash(&self, err: SubsystemError) {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        if guard.is_ok() {
            *guard = Err(err);
            self.cancel.cancel();
        }
    }

    fn take_crash(&self) -> SubsystemResult {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        mem::replace(&mut *guard, Ok(()))
    }
}

impl RootBuilder {
    pub async fn start<E, F>(
        self,
        subsys: impl FnOnce(SubsystemHandle) -> F + Send + 'static,
    ) -> SubsystemResult
    where
        F: std::future::Future<Output = Result<(), E>> + Send + 'static,
        E: IntoGenericError + Display,
    {
        let global = CancellationToken::new();
        let local = global.child_token();

        if self.catch_signals {
            self.register_signal_handlers(&global);
        }

        let crash = CrashHolder {
            crash: Arc::new(Mutex::new(Ok(()))),
            cancel: global.clone(),
        };

        let (res_tx, res_rx) = oneshot::channel();
        let (join_tx, join_rx) = watch::channel(false);

        let cancel_clean_shutdown = CancellationToken::new();

        let subsystems = Arc::new(Mutex::new(HashMap::new()));

        let handle = SubsystemHandle {
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
                    let subsystems = subsystems.lock().expect("mutex is poisoned");
                    subsystems.clone()
                };
                let subsys_shutdown_future = wait_for_subsystems_shutdown(&subsystems);

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
                join_tx.send(true).ok();
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
                    let subsystems = subsystems.lock().expect("mutex is poisoned");
                    subsystems.clone()
                };
                let subsys_shutdown_future = wait_for_subsystems_shutdown(&subsystems);
                subsys_shutdown_future.await;
                info!("All subsystems have shut down.");

                res_tx.send(crash.take_crash()).ok();
                join_tx.send(true).ok();
            });
        }

        res_rx.await.unwrap_or(Err(SubsystemError::ForcedShutdown))
    }

    pub fn catch_signals(mut self) -> Self {
        self.catch_signals = true;
        self
    }

    pub fn with_timeout(mut self, shutdown_timeout: Duration) -> Self {
        self.shutdown_timeout = Some(shutdown_timeout);
        self
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    fn register_signal_handlers(&self, global: &CancellationToken) {
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
    fn register_signal_handlers(&self, global: &CancellationToken) {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(signal) = signal(SignalKind::hangup()) {
            handle_unix_signal(
                global,
                signal,
                "SIGHUP",
                SignalKind::hangup().as_raw_value(),
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
            );
        } else {
            error!("Failed to register SIGINT handler");
        }

        if let Ok(signal) = signal(SignalKind::quit()) {
            handle_unix_signal(global, signal, "SIGQUIT", SignalKind::quit().as_raw_value());
        } else {
            error!("Failed to register SIGQUIT handler");
        }

        if let Ok(signal) = signal(SignalKind::terminate()) {
            handle_unix_signal(
                global,
                signal,
                "SIGTERM",
                SignalKind::terminate().as_raw_value(),
            );
        } else {
            error!("Failed to register SIGTERM handler");
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn handle_unix_signal(
    global: &CancellationToken,
    mut signal: tokio::signal::unix::Signal,
    signal_name: &'static str,
    code: i32,
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
            global.cancel();
        }
        process::exit(128 + code);
    });
}

#[derive(Debug, Error, Diagnostic)]
pub enum SubsystemError {
    #[error("Subsystem '{0}' terminated with error: {1}")]
    Error(String, GenericError),
    #[error("Subsystem '{0}' panicked: {1}")]
    Panic(String, String),
    #[error("Subsystem shutdown timed out")]
    ForcedShutdown,
}

pub trait GenErr: Debug + Display + Send + Sync + 'static {}

impl<E> GenErr for E where E: Debug + Display + Send + Sync + 'static {}

pub struct GenericError(Box<dyn GenErr>);

pub trait IntoGenericError {
    fn into_generic_error(self) -> GenericError;
}

impl<E: GenErr> IntoGenericError for E {
    fn into_generic_error(self) -> GenericError {
        GenericError(Box::new(self))
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

pub type SubsystemResult = Result<(), SubsystemError>;

async fn wait_for_subsystems_shutdown(
    subsystems: &HashMap<String, (watch::Sender<bool>, watch::Receiver<bool>)>,
) {
    for subsystem in subsystems.values() {
        let mut rx = subsystem.1.clone();
        rx.wait_for(|it| *it).await.ok();
    }
}

pub struct SubsystemHandle {
    name: String,
    local: CancellationToken,
    global: CancellationToken,
    cancel_clean_shutdown: CancellationToken,
    subsystems: SubsystemMap,
    crash: CrashHolder,
    join_handle: (watch::Sender<bool>, watch::Receiver<bool>),
}

impl Clone for SubsystemHandle {
    fn clone(&self) -> Self {
        SubsystemHandle {
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

impl fmt::Debug for SubsystemHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubsystemHandle")
            .field("name", &self.name)
            .finish()
    }
}

fn convert_result<Err>(res: Result<(), Err>) -> Result<(), GenericError>
where
    Err: IntoGenericError,
{
    match res {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into_generic_error()),
    }
}

impl SubsystemHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn spawn<Err, F>(
        &self,
        name: impl AsRef<str>,
        subsys: impl FnOnce(SubsystemHandle) -> F + Send + 'static,
    ) -> SubsystemHandle
    where
        F: Future<Output = Result<(), Err>> + Send + 'static,
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
        info!("Spawning subsystem '{}' …", fname);
        tokio::spawn(async move {
            let name = fname.clone();
            let mut join_handle = tokio::spawn(async move {
                info!("Subsystem '{}' started.", name);
                let res = subsys(h).await;
                convert_result(res)
            });
            select! {
                res = &mut join_handle => Self::subsystem_joined(res, subsystems, &fname, &mut crash).await,
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

    pub fn is_shut_down(&self) -> bool {
        self.local.is_cancelled()
    }

    pub async fn join(&self) {
        let mut join_handle = self.join_handle.clone();
        join_handle.1.wait_for(|it| *it).await.ok();
    }

    fn create_child(
        &self,
        name: impl AsRef<str>,
        cancel_clean_shutdown: CancellationToken,
    ) -> SubsystemHandle {
        let (res_tx, res_rx) = watch::channel(false);
        let name = format!("{}/{}", self.name, name.as_ref());
        let global = self.global.clone();
        let local = self.local.child_token();
        let subsystems = self.subsystems.clone();
        let crash = self.crash.clone();

        let mut gc = self.subsystems.lock().expect("mutex is poisoned");
        gc.insert(name.clone(), (res_tx.clone(), res_rx.clone()));

        SubsystemHandle {
            name,
            global,
            local,
            cancel_clean_shutdown,
            subsystems,
            crash,
            join_handle: (res_tx, res_rx),
        }
    }

    async fn subsystem_joined(
        res: Result<Result<(), GenericError>, JoinError>,
        subsystems: SubsystemMap,
        subsystem_name: &str,
        crash: &mut CrashHolder,
    ) {
        let mut subsystems = subsystems.lock().expect("mutex is poisoned");
        let Some(subsys) = subsystems.remove(subsystem_name) else {
            warn!(
                "Subsystem '{}' already removed from tracking.",
                subsystem_name
            );
            return;
        };

        debug!(
            "Subsystem '{}' removed. Remaining subsystems: {:?}",
            subsystem_name,
            subsystems.keys()
        );

        match res {
            Ok(Ok(_)) => {
                info!("Subsystem '{}' terminated normally.", subsystem_name);
                subsys.0.send(true).ok();
            }
            Ok(Err(e)) => {
                error!(
                    "Subsystem '{}' terminated with error: {}",
                    subsystem_name, e
                );
                let err = SubsystemError::Error(subsystem_name.to_owned(), e);
                crash.set_crash(err);
                subsys.0.send(true).ok();
            }
            Err(e) => {
                if e.is_panic() {
                    error!("Subsystem '{}' panicked: {}", subsystem_name, e);
                    let err = SubsystemError::Panic(subsystem_name.to_owned(), e.to_string());
                    crash.set_crash(err);
                    subsys.0.send(true).ok();
                } else {
                    warn!("Subsystem '{}' was shut down forcefully.", subsystem_name);
                    let err = SubsystemError::ForcedShutdown;
                    crash.set_crash(err);
                    subsys.0.send(true).ok();
                }
            }
        };
    }

    async fn shutdown_timed_out<Err>(
        join_handle: tokio::task::JoinHandle<Result<(), Err>>,
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
    }
}
