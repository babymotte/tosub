/*
 *  Copyright (C) 2025 Michael Bachmann
 *
 *  This program is free software: you can redistribute it and/or modify
 *  it under the terms of the GNU Affero General Public License as published by
 *  the Free Software Foundation, either version 3 of the License, or
 *  (at your option) any later version.
 *
 *  This program is distributed in the hope that it will be useful,
 *  but WITHOUT ANY WARRANTY; without even the implied warranty of
 *  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 *  GNU Affero General Public License for more details.
 *
 *  You should have received a copy of the GNU Affero General Public License
 *  along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use std::{future::pending, io, process::ExitCode, time::Duration};
use tokio::select;
use tokio::time::sleep;
use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> miette::Result<ExitCode> {
    tracing_subscriber::registry()
        .with(
            fmt::Layer::new().with_writer(io::stderr).with_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            ),
        )
        .init();

    tosub::build_root("hello_world")
        .catch_signals()
        .with_timeout(Duration::from_secs(1))
        .start(|root| async move {
            let child1 = root.spawn("child 1", child1);
            sleep(Duration::from_millis(10)).await;
            let child2 = root.spawn("child 2", child2);
            sleep(Duration::from_millis(10)).await;
            let child3 = root.spawn("child 3", child3);
            sleep(Duration::from_millis(10)).await;
            let child4 = root.spawn("child 4", child4);
            sleep(Duration::from_millis(10)).await;
            let child5 = root.spawn("child 5", child5);

            sleep(Duration::from_millis(10)).await;

            // this will shut down subsystem 5, however it will keep running because it does not handle the shutdown request gracefully and it will eventually be killed forcefully
            child5.request_local_shutdown();

            sleep(Duration::from_millis(10)).await;

            // this will shut down subsystem 4, however it will still return an ok result because it gracefully handles the shutdown request
            child4.request_local_shutdown();

            // the remaining systems will complete normally and will not be requested to shut down

            let res = child5.join().await;
            info!("Child 5 done with result: {:?}", res);

            let res = child4.join().await;
            info!("Child 4 done with result: {:?}", res);

            let res = child3.join().await;
            info!("Child 3 done with result: {:?}", res);

            let res = child2.join().await;
            info!("Child 2 done with result: {:?}", res);

            let res = child1.join().await;
            info!("Child 1 done with result: {:?}", res);

            Ok::<(), miette::Report>(())
        })
        .await
}

async fn child1(subsys: tosub::Subsystem<u64>) -> miette::Result<u64> {
    info!("Hello from {}", subsys.name());
    select! {
        _ = sleep(Duration::from_secs(1)) => {
            info!("{} completed work", subsys.name());
            Ok(1)
        },
        _ = subsys.shutdown_requested() => Err(miette::miette!("Child 1 received shutdown request")),
    }
}

async fn child2(subsys: tosub::Subsystem<u64>) -> miette::Result<u64> {
    info!("Hello from {}", subsys.name());
    select! {
        _ = sleep(Duration::from_secs(2)) => {
            info!("{} completed work", subsys.name());
            Ok(2)
        },
        _ = subsys.shutdown_requested() => Err(miette::miette!("Child 2 received shutdown request")),
    }
}

async fn child3(subsys: tosub::Subsystem<u64>) -> miette::Result<u64> {
    info!("Hello from {}", subsys.name());
    select! {
        _ = sleep(Duration::from_secs(3)) => {
            info!("{} completed work", subsys.name());
            Ok(3)
        },
        _ = subsys.shutdown_requested() => Err(miette::miette!("Child 3 received shutdown request")),
    }
}

async fn child4(subsys: tosub::Subsystem<u64>) -> miette::Result<u64> {
    info!("Hello from {}", subsys.name());
    select! {
        _ = pending() => info!("{} completed work", subsys.name()),
        _ = subsys.shutdown_requested() => info!("Child 4 received shutdown request"),
    }
    Ok(4)
}

async fn child5(subsys: tosub::Subsystem<u64>) -> miette::Result<u64> {
    info!("Hello from {}", subsys.name());
    loop {
        sleep(Duration::from_millis(100)).await;
    }
}
