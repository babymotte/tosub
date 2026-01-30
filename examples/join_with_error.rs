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

use miette::IntoDiagnostic;
use std::{io, time::Duration};
use tokio::select;
use tokio::time::sleep;
use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::registry()
        .with(
            fmt::Layer::new().with_writer(io::stderr).with_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .with_env_var("WORTERBUCH_LOG")
                    .from_env_lossy(),
            ),
        )
        .init();

    tosub::build_root("hello_world")
        .catch_signals()
        .with_timeout(Duration::from_secs(5))
        .start(|root| async move {
            let child1 = root.spawn("child 1", child1);
            let child2 = root.spawn("child 2", child2);
            let child3 = root.spawn("child 3", child3);

            select! {
                _ = child3.join() => {
                    info!("Child 3 done");
                },
                _ = root.shutdown_requested() => (),
            }

            select! {
                _ = child2.join() => {
                    info!("Child 2 done");
                },
                _ = root.shutdown_requested() => (),
            }

            select! {
                _ = child1.join() => {
                    info!("Child 1 done");
                },
                _ = root.shutdown_requested() => (),
            }

            Ok::<(), miette::Report>(())
        })
        .await
        .into_diagnostic()?;
    Ok(())
}

async fn child1(subsys: tosub::SubsystemHandle) -> miette::Result<()> {
    info!("Hello from {}", subsys.name());
    select! {
        _ = sleep(Duration::from_secs(1)) => info!("Child 1 completed work"),
        _ = subsys.shutdown_requested() => info!("Child 1 received shutdown request"),
    }
    Ok(())
}

async fn child2(subsys: tosub::SubsystemHandle) -> miette::Result<()> {
    info!("Hello from {}", subsys.name());
    select! {
        _ = sleep(Duration::from_secs(2)) => info!("Child 2 completed work"),
        _ = subsys.shutdown_requested() => info!("Child 2 received shutdown request"),
    }
    Err(miette::miette!("Child 2 encountered an error"))
}

async fn child3(subsys: tosub::SubsystemHandle) -> miette::Result<()> {
    info!("Hello from {}", subsys.name());
    select! {
        _ = sleep(Duration::from_secs(3)) => info!("Child 3 completed work"),
        _ = subsys.shutdown_requested() => info!("Child 3 received shutdown request"),
    }
    Ok(())
}
