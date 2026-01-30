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

use std::{
    collections::HashMap,
    fmt::{self, Debug, Display},
    process,
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    select,
    signal::unix::{Signal, SignalKind, signal},
    spawn,
    sync::{oneshot, watch},
    task::JoinError,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub struct RootBuilder {
    name: String,
    catch_signals: bool,
    shutdown_timeout: Option<std::time::Duration>,
}

impl RootBuilder {
    pub fn start<E, F>(
        self,
        subsys: impl FnOnce(SubsystemHandle<E>) -> F + Send + 'static,
    ) -> SubsystemJoinHandle<E>
    where
        F: std::future::Future<Output = Result<(), E>> + Send + 'static,
        E: Debug + Display + Send + Sync + 'static,
    {
        let global = CancellationToken::new();
        let local = global.child_token();

        if self.catch_signals {
            self.register_signal_handlers(&global);
        }

        let (res_tx, res_rx) = oneshot::channel();

        let cancel_clean_shutdown = CancellationToken::new();

        let children = Arc::new(Mutex::new(Some(HashMap::new())));

        let handle = SubsystemHandle {
            name: self.name.clone(),
            global: global.clone(),
            local: local.clone(),

            cancel_clean_shutdown: cancel_clean_shutdown.clone(),
            children: children.clone(),
        };

        let join_handle = SubsystemJoinHandle {
            name: self.name,
            global: global.clone(),
            local: local.clone(),
            res_rx: Some(res_rx),
        };

        let glob = global.clone();
        if let Some(to) = self.shutdown_timeout {
            let cancel_clean_shutdown = cancel_clean_shutdown.clone();

            spawn(async move {
                match subsys(handle).await {
                    Ok(_) => info!("Root system terminated normally."),
                    Err(e) => error!("Root system terminated with error: {e}"),
                }

                if !global.is_cancelled() {
                    glob.cancel();
                }
                info!(
                    "Shutdown initiated, waiting up to {:?} for clean shutdown.",
                    to
                );

                let Some(children) = children.lock().expect("mutex is poisoned").take() else {
                    error!("Root system has already been shut down.");
                    return;
                };

                let children_shutdown =
                    SubsystemJoinHandle::wait_for_children_shutdown(children, Ok(()));

                match timeout(to, children_shutdown).await {
                    Ok(res) => {
                        info!("All subsystems have shut down in time.");
                        res_tx.send(res).ok();
                    }
                    Err(_) => {
                        error!("Shutdown timeout reached, forcing shutdown …");
                        cancel_clean_shutdown.cancel();
                        res_tx.send(Err(SubSystemErr::ForcedShutdown)).ok();
                    }
                }
            });
        } else {
            spawn(async move {
                match subsys(handle).await {
                    Ok(_) => info!("Root system terminated normally."),
                    Err(e) => error!("Root system terminated with error: {e}"),
                }

                if !global.is_cancelled() {
                    glob.cancel();
                }
                info!("Shutdown initiated, waiting for clean shutdown.");

                let Some(children) = children.lock().expect("mutex is poisoned").take() else {
                    error!("Root system has already been shut down.");
                    return;
                };

                let res = SubsystemJoinHandle::wait_for_children_shutdown(children, Ok(())).await;
                info!("All subsystems have shut down in time.");
                res_tx.send(res).ok();
            });
        }

        join_handle
    }

    pub fn catch_signals(mut self) -> Self {
        self.catch_signals = true;
        self
    }

    pub fn with_timeout(mut self, shutdown_timeout: Duration) -> Self {
        self.shutdown_timeout = Some(shutdown_timeout);
        self
    }

    fn register_signal_handlers(&self, global: &CancellationToken) {
        if let Ok(signal) = signal(SignalKind::hangup()) {
            handle_signal(
                global,
                signal,
                "SIGHUP",
                SignalKind::hangup().as_raw_value(),
            );
        } else {
            error!("Failed to register SIGHUP handler");
        }

        if let Ok(signal) = signal(SignalKind::interrupt()) {
            handle_signal(
                global,
                signal,
                "SIGINT",
                SignalKind::interrupt().as_raw_value(),
            );
        } else {
            error!("Failed to register SIGINT handler");
        }

        if let Ok(signal) = signal(SignalKind::quit()) {
            handle_signal(global, signal, "SIGQUIT", SignalKind::quit().as_raw_value());
        } else {
            error!("Failed to register SIGQUIT handler");
        }

        if let Ok(signal) = signal(SignalKind::terminate()) {
            handle_signal(
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

fn handle_signal(
    global: &CancellationToken,
    mut signal: Signal,
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

#[derive(Debug, Error)]
pub enum SubSystemErr<E> {
    #[error("Subsystem terminated with error: {0}")]
    Error(E),
    #[error("Subsystem panicked: {0}")]
    Panic(String),
    #[error("Subsystem shutdown timed out")]
    ForcedShutdown,
}

pub type SubsystemResult<E> = Result<(), SubSystemErr<E>>;

pub struct Child<E> {
    res_rx: oneshot::Receiver<SubsystemResult<E>>,
    res_tx: oneshot::Sender<SubsystemResult<E>>,
}

impl<E> Child<E>
where
    E: Send + Sync + 'static,
{
    async fn join(self) -> SubsystemResult<E> {
        let res = self.res_rx.await.unwrap_or_else(|_| Ok(()));
        self.res_tx.send(Ok(())).ok();
        res
    }
}

#[derive(Clone)]
pub struct SubsystemHandle<E>
where
    E: Send + Sync + 'static,
{
    name: String,
    local: CancellationToken,
    global: CancellationToken,
    cancel_clean_shutdown: CancellationToken,
    children: Arc<Mutex<Option<HashMap<String, Child<E>>>>>,
}

impl<E> fmt::Debug for SubsystemHandle<E>
where
    E: Send + Sync + 'static,
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
    ) -> SubsystemJoinHandle<E>
    where
        F: Future<Output = Result<(), Err>> + Send + 'static,
        Err: Debug + Display + Send + Sync + 'static + Into<E>,
    {
        let join = watch::channel(false);
        let join_tx = join.0.clone();
        let cancel_clean_shutdown = self.cancel_clean_shutdown.clone();

        let (res_tx, res_rx) = oneshot::channel();
        let (res_tx2, res_rx2) = oneshot::channel();
        let (join_handle, handle) = self.create_child(name, res_rx2, cancel_clean_shutdown.clone());
        let full_name = join_handle.name().to_owned();
        let child: Child<E> = Child {
            res_rx,
            res_tx: res_tx2,
        };

        {
            let mut children = self.children.lock().expect("mutex is poisoned");
            if let Some(children) = children.as_mut() {
                children.insert(full_name.clone(), child);
            }
        }

        let fname = full_name.clone();
        let global = self.global.clone();
        let children = self.children.clone();
        info!("Spawning subsystem '{}' …", fname);
        tokio::spawn(async move {
            let name = fname.clone();
            let mut join_handle: tokio::task::JoinHandle<Result<(), E>> =
                tokio::spawn(async move {
                    info!("Subsystem '{}' started.", name);
                    let res = subsys(handle).await;
                    convert_result(res)
                });
            let res = select! {
                res = &mut join_handle => Self::child_joined(res, children, &fname, global, join_tx),
                _ = cancel_clean_shutdown.cancelled() => Self::shutdown_timed_out(join_handle, &fname),
            };
            res_tx.send(res).ok();
        });

        join_handle
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

    fn create_child(
        &self,
        name: impl AsRef<str>,
        res_rx: oneshot::Receiver<SubsystemResult<E>>,
        cancel_clean_shutdown: CancellationToken,
    ) -> (SubsystemJoinHandle<E>, SubsystemHandle<E>) {
        let name = format!("{}/{}", self.name, name.as_ref());
        let global = self.global.clone();
        let local = self.local.child_token();
        let children = Arc::new(Mutex::new(Some(HashMap::new())));

        let join_handle = SubsystemJoinHandle {
            name: name.clone(),
            global: global.clone(),
            local: local.clone(),
            res_rx: Some(res_rx),
        };

        let handle = SubsystemHandle {
            name,
            global,
            local,
            cancel_clean_shutdown,
            children,
        };

        (join_handle, handle)
    }

    fn child_joined(
        res: Result<Result<(), E>, JoinError>,
        children: Arc<Mutex<Option<HashMap<String, Child<E>>>>>,
        child_name: &str,
        global: CancellationToken,
        join_tx: watch::Sender<bool>,
    ) -> Result<(), SubSystemErr<E>> {
        let res = match res {
            Ok(Ok(_)) => {
                info!("Subsystem '{}' terminated normally.", child_name);
                if let Some(children) = children.lock().expect("mutex is poisoned").as_mut() {
                    if let Some(child) = children.remove(child_name) {
                        child.res_tx.send(Ok(())).ok();
                    }
                }
                Ok(())
            }
            Ok(Err(e)) => {
                error!("Subsystem '{}' terminated with error: {}", child_name, e);
                global.cancel();
                Err(SubSystemErr::Error(e.into()))
            }
            Err(e) => {
                if e.is_panic() {
                    error!("Subsystem '{}' panicked: {}", child_name, e);
                    global.cancel();
                    Err(SubSystemErr::Panic(e.to_string()))
                } else {
                    warn!("Subsystem '{}' was shut down forcefully.", child_name);
                    Err(SubSystemErr::ForcedShutdown)
                }
            }
        };
        join_tx.send(true).ok();
        res
    }

    fn shutdown_timed_out<Err>(
        join_handle: tokio::task::JoinHandle<Result<(), Err>>,
        child_name: &str,
    ) -> Result<(), SubSystemErr<Err>>
    where
        Err: Debug + Display + Send + Sync + 'static,
    {
        warn!("Subsystem '{}' is being shut down forcefully.", child_name);
        join_handle.abort();
        Err(SubSystemErr::ForcedShutdown)
    }
}

pub struct SubsystemJoinHandle<E>
where
    E: Debug + Display + Send + Sync + 'static,
{
    name: String,
    local: CancellationToken,
    global: CancellationToken,
    res_rx: Option<oneshot::Receiver<SubsystemResult<E>>>,
}

impl<E> fmt::Debug for SubsystemJoinHandle<E>
where
    E: Debug + Display + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubsystemJoinHandle")
            .field("name", &self.name)
            .finish()
    }
}

impl<E> SubsystemJoinHandle<E>
where
    E: Debug + Display + Send + Sync + 'static,
{
    pub fn name(&self) -> &str {
        &self.name
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

    pub async fn join(&mut self) -> SubsystemResult<E> {
        let Some(rx) = self.res_rx.take() else {
            panic!("already joined on this subsystem");
        };
        let res = rx.await.unwrap_or_else(|_| Ok(()));
        res
    }

    async fn wait_for_children_shutdown(
        children: HashMap<String, Child<E>>,
        res: SubsystemResult<E>,
    ) -> SubsystemResult<E> {
        let mut res = res;
        for (_, child) in children {
            let r = child.join().await;
            if let (r, &Ok(_)) = (r, &res) {
                res = r;
            }
        }
        res
    }
}

pub fn build_root(name: impl Into<String>) -> RootBuilder {
    RootBuilder {
        name: name.into(),
        catch_signals: false,
        shutdown_timeout: None,
    }
}
