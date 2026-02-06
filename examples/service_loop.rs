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

use miette::{IntoDiagnostic, miette};
use std::{io, time::Duration};
use tokio::select;
use tokio::time::sleep;
use tosub::SubsystemHandle;
use tracing::level_filters::LevelFilter;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> miette::Result<()> {
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
        .with_timeout(Duration::from_secs(5))
        .start(|root| async move {
            root.spawn("service 1", |s| service_loop(s, Duration::from_secs(3)));
            root.spawn("service 2", |s| service_loop(s, Duration::from_secs(7)));
            root.spawn("service 3", |s| service_loop(s, Duration::from_secs(11)));

            root.shutdown_requested().await;

            Ok::<(), miette::Report>(())
        })
        .await
        .into_diagnostic()?;
    Ok(())
}

async fn service_loop(subsys: tosub::SubsystemHandle, duration: Duration) -> miette::Result<()> {
    info!("Service {} started.", subsys.name());

    loop {
        let d = duration.clone();
        let runner = subsys.spawn("runner", move |s| service_runner(s, d.clone()));
        select! {
            _ = runner.join() => {
                info!("Service {} runner stopped. Restarting...", subsys.name());
            },
            _ = subsys.shutdown_requested() => {
                info!("Service {} received shutdown request.", subsys.name());
                break;
            },
        }
    }

    Ok(())
}

async fn service_runner(s: SubsystemHandle, duration: Duration) -> miette::Result<()> {
    info!("Service {} started.\n", s.name());

    let simulated_work = async {
        sleep(duration).await;
        Err::<(), miette::Report>(miette!("Simulated error in service {}", s.name()))
    };

    select! {
        res = simulated_work => if let Err(e) = res {
            // catch error and return normally to prevent shutting down the whole system
            error!("Service {} encountered an error: {e}", s.name());
            return Ok(());
        },
        _ = s.shutdown_requested() => {
            info!("Service {} received shutdown request.", s.name());
            return Ok(());
        },
    }

    Ok(())
}
