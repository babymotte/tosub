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
use tokio::time::sleep;
use tosub::SubsystemResult;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> SubsystemResult {
    tracing_subscriber::registry()
        .with(
            fmt::Layer::new().with_writer(io::stderr).with_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            ),
        )
        .init();

    let root = tosub::build_root("hello_world")
        .catch_signals()
        .with_timeout(Duration::from_secs(5));

    // tosub lets you recover the exit code matching the signal that stopped the process.
    // it is for the application developer to decide if on a clean shutdown an exit code of 0 or
    // the one matching the signal should be returned.
    let exit_code = root.start(run).await?;

    // in this case we return the exit code matching the signal
    Ok(exit_code)

    // alternatively, discard the exit code and return 0 on clean shutdown
    // Ok(ExitCode::SUCCESS)
}

async fn run(root: tosub::SubsystemHandle) -> miette::Result<()> {
    root.spawn("child 1", child1);
    root.spawn("child 2", child2);
    root.spawn("child 3", child3);

    root.shutdown_requested().await;

    Ok(())
}

async fn child1(subsystem: tosub::SubsystemHandle) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.spawn("grandchild 1", grandchild1);
    subsystem.spawn("grandchild 2", grandchild2);

    subsystem.shutdown_requested().await;

    println!("{} needs a second to shut down ...", subsystem.name());
    sleep(Duration::from_secs(1)).await;

    Ok(())
}

async fn grandchild2(subsystem: tosub::SubsystemHandle) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.spawn("great grandchild 1", great_grandchild1);

    subsystem.shutdown_requested().await;

    println!("{} needs a second to shut down ...", subsystem.name());
    sleep(Duration::from_secs(1)).await;

    Ok(())
}

async fn great_grandchild1(subsystem: tosub::SubsystemHandle) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.shutdown_requested().await;

    println!("{} needs a second to shut down ...", subsystem.name());
    sleep(Duration::from_secs(1)).await;

    Ok(())
}

async fn grandchild1(subsystem: tosub::SubsystemHandle) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.shutdown_requested().await;

    println!("{} shuts down immedaiately.", subsystem.name());

    Ok(())
}

async fn child2(subsystem: tosub::SubsystemHandle) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.spawn("grandchild 3", grandchild3);

    subsystem.shutdown_requested().await;

    println!("{} needs two seconds to shut down ...", subsystem.name());
    sleep(Duration::from_secs(2)).await;

    Ok(())
}

async fn grandchild3(subsystem: tosub::SubsystemHandle) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.shutdown_requested().await;

    println!("{} needs two second to shut down ...", subsystem.name());
    sleep(Duration::from_secs(2)).await;

    Ok(())
}

async fn child3(subsystem: tosub::SubsystemHandle) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.shutdown_requested().await;

    println!("{} needs three seconds to shut down ...", subsystem.name());
    sleep(Duration::from_secs(3)).await;

    Ok(())
}
