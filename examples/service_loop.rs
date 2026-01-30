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
            root.spawn("service 1", |s| service_loop(s, Duration::from_secs(2)));
            root.spawn("service 2", |s| service_loop(s, Duration::from_secs(5)));
            root.spawn("service 3", |s| service_loop(s, Duration::from_secs(10)));

            root.shutdown_requested().await;

            Ok(())
        })
        .await
        .into_diagnostic()?;
    Ok(())
}

async fn service_loop(
    subsys: tosub::SubsystemHandle<miette::Report>,
    duration: Duration,
) -> miette::Result<()> {
    info!("Service {} started.", subsys.name());

    loop {
        select! {
            res = service_runner(duration) => {
                match res {
                    Ok(_) => break,
                    Err(e) => info!("Service {} runner finished with error: {}, restarting...", subsys.name(), e),
                }
            },
            _ = subsys.shutdown_requested() => {
                info!("Service {} received shutdown request", subsys.name());
                break;
            },
        }
    }

    Ok(())
}

async fn service_runner(duration: Duration) -> miette::Result<()> {
    info!("Service started.");
    sleep(duration).await;
    Err(miette::miette!("Simulated error"))
}
