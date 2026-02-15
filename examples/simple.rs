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

use std::{io, time::Duration};
use tokio::{select, time::sleep};
use tosub::SubsystemHandle;
use tracing::{info, level_filters::LevelFilter};
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
        .start(run)
        .await?;

    Ok(())
}

async fn run(subsys: SubsystemHandle) -> miette::Result<()> {
    println!("Hello from {}", subsys.name());

    select! {
        _ = async {
            info!("Doing some work, this should take about two seconds. Press Ctrl+C to stop...");
            sleep(Duration::from_secs(2)).await;
        } => (),
        _ = subsys.shutdown_requested() => (),
    }

    info!(
        "Stopped. Cleaning up, this should take about one second. Press Ctrl+C to exit immediately..."
    );

    // do some cleanup here
    sleep(Duration::from_secs(1)).await;

    Ok(())
}
