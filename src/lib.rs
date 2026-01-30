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
use tracing::{error, info, warn};

type ChildMap = Arc<Mutex<HashMap<String, (watch::Sender<bool>, watch::Receiver<bool>)>>>;

pub struct RootBuilder {
    name: String,
    catch_signals: bool,
    shutdown_timeout: Option<std::time::Duration>,
}

struct CrashHolder<E>
where
    E: Debug + Display,
{
    crash: Arc<Mutex<SubsystemResult<E>>>,
    cancel: CancellationToken,
}

impl<E> Clone for CrashHolder<E>
where
    E: Debug + Display,
{
    fn clone(&self) -> Self {
        CrashHolder {
            crash: self.crash.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

impl<E> CrashHolder<E>
where
    E: Debug + Display,
{
    fn set_crash(&self, err: SubsystemError<E>) {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        if guard.is_ok() {
            *guard = Err(err);
            self.cancel.cancel();
        }
    }

    fn take_crash(&self) -> SubsystemResult<E> {
        let mut guard = self.crash.lock().expect("mutex is poisoned");
        mem::replace(&mut *guard, Ok(()))
    }
}

impl RootBuilder {
    pub async fn start<E, F>(
        self,
        subsys: impl FnOnce(SubsystemHandle<E>) -> F + Send + 'static,
    ) -> SubsystemResult<E>
    where
        F: std::future::Future<Output = Result<(), E>> + Send + 'static,
        E: Debug + Display + Send + Sync + 'static,
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

        let children = Arc::new(Mutex::new(HashMap::new()));

        let handle = SubsystemHandle {
            name: self.name.clone(),
            global: global.clone(),
            local: local.clone(),
            cancel_clean_shutdown: cancel_clean_shutdown.clone(),
            children: children.clone(),
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
                        crash.set_crash(SubsystemError::Error(self.name.clone(), e));
                    }
                }

                glob.cancel();
                info!(
                    "Shutdown initiated, waiting up to {:?} for clean shutdown.",
                    to
                );

                let children = {
                    let children = children.lock().expect("mutex is poisoned");
                    children.clone()
                };
                let children_shutdown = wait_for_children_shutdown(&children);

                match timeout(to, children_shutdown).await {
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

                let children = {
                    let children = children.lock().expect("mutex is poisoned");
                    children.clone()
                };
                let children_shutdown = wait_for_children_shutdown(&children);
                children_shutdown.await;
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
pub enum SubsystemError<E>
where
    E: Debug + Display,
{
    #[error("Subsystem '{0}' terminated with error: {1}")]
    Error(String, E),
    #[error("Subsystem '{0}' panicked: {1}")]
    Panic(String, String),
    #[error("Subsystem shutdown timed out")]
    ForcedShutdown,
}

pub type SubsystemResult<E> = Result<(), SubsystemError<E>>;

async fn wait_for_children_shutdown(
    children: &HashMap<String, (watch::Sender<bool>, watch::Receiver<bool>)>,
) {
    for child in children.values() {
        let mut rx = child.1.clone();
        rx.wait_for(|it| *it).await.ok();
    }
}

pub struct SubsystemHandle<E>
where
    E: Debug + Display + Send + Sync + 'static,
{
    name: String,
    local: CancellationToken,
    global: CancellationToken,
    cancel_clean_shutdown: CancellationToken,
    children: ChildMap,
    crash: CrashHolder<E>,
    join_handle: (watch::Sender<bool>, watch::Receiver<bool>),
}

impl<E> Clone for SubsystemHandle<E>
where
    E: Debug + Display + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        SubsystemHandle {
            name: self.name.clone(),
            local: self.local.clone(),
            global: self.global.clone(),
            cancel_clean_shutdown: self.cancel_clean_shutdown.clone(),
            children: self.children.clone(),
            crash: self.crash.clone(),
            join_handle: (self.join_handle.0.clone(), self.join_handle.1.clone()),
        }
    }
}

impl<E> fmt::Debug for SubsystemHandle<E>
where
    E: Debug + Display + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubsystemHandle")
            .field("name", &self.name)
            .finish()
    }
}

fn convert_result<E, Err>(res: Result<(), Err>) -> Result<(), E>
where
    E: Send + Sync + 'static,
    Err: Send + Sync + 'static + Into<E>,
{
    match res {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

impl<E> SubsystemHandle<E>
where
    E: Debug + Display + Send + Sync + 'static,
{
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn spawn<Err, F>(
        &self,
        name: impl AsRef<str>,
        subsys: impl FnOnce(SubsystemHandle<E>) -> F + Send + 'static,
    ) -> SubsystemHandle<E>
    where
        F: Future<Output = Result<(), Err>> + Send + 'static,
        Err: Debug + Display + Send + Sync + 'static + Into<E>,
    {
        let cancel_clean_shutdown = self.cancel_clean_shutdown.clone();

        let handle = self.create_child(name, cancel_clean_shutdown.clone());
        let full_name = handle.name().to_owned();

        let fname = full_name.clone();
        let children = self.children.clone();
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
                res = &mut join_handle => Self::child_joined(res, children, &fname, &mut crash).await,
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
    ) -> SubsystemHandle<E> {
        let (res_tx, res_rx) = watch::channel(false);
        let name = format!("{}/{}", self.name, name.as_ref());
        let global = self.global.clone();
        let local = self.local.child_token();
        let children = self.children.clone();
        let crash = self.crash.clone();

        let mut gc = self.children.lock().expect("mutex is poisoned");
        gc.insert(name.clone(), (res_tx.clone(), res_rx.clone()));

        SubsystemHandle {
            name,
            global,
            local,
            cancel_clean_shutdown,
            children,
            crash,
            join_handle: (res_tx, res_rx),
        }
    }

    async fn child_joined(
        res: Result<Result<(), E>, JoinError>,
        children: ChildMap,
        child_name: &str,
        crash: &mut CrashHolder<E>,
    ) {
        let mut children = children.lock().expect("mutex is poisoned");
        let Some(child) = children.remove(child_name) else {
            warn!("Subsystem '{}' already removed from tracking.", child_name);
            return;
        };

        match res {
            Ok(Ok(_)) => {
                info!("Subsystem '{}' terminated normally.", child_name);
                child.0.send(true).ok();
            }
            Ok(Err(e)) => {
                error!("Subsystem '{}' terminated with error: {}", child_name, e);
                let err = SubsystemError::Error(child_name.to_owned(), e);
                crash.set_crash(err);
                child.0.send(true).ok();
            }
            Err(e) => {
                if e.is_panic() {
                    error!("Subsystem '{}' panicked: {}", child_name, e);
                    let err = SubsystemError::Panic(child_name.to_owned(), e.to_string());
                    crash.set_crash(err);
                    child.0.send(true).ok();
                } else {
                    warn!("Subsystem '{}' was shut down forcefully.", child_name);
                    let err = SubsystemError::ForcedShutdown;
                    crash.set_crash(err);
                    child.0.send(true).ok();
                }
            }
        };
    }

    async fn shutdown_timed_out<Err>(
        join_handle: tokio::task::JoinHandle<Result<(), Err>>,
        child_name: &str,
        global: &CancellationToken,
        crash: &mut CrashHolder<E>,
    ) where
        Err: Debug + Display + Send + Sync + 'static,
    {
        warn!("Subsystem '{}' is being shut down forcefully.", child_name);
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
