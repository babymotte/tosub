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
use tracing::level_filters::LevelFilter;
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

    let root = tosub::build_root("hello_world")
        .catch_signals()
        .with_timeout(Duration::from_secs(5));

    // By default, tosub root systems return an exit code matching the reason the process terminated.
    // In applications that perform an orderly shutdown when receiving a signal, it is however recommended
    // to return exit code 0 if the shutdown was successful to indicate a clean program exit.
    let exit_code = root.start(run).await?;
    eprintln!("Exit code: {:?}", exit_code);

    // Hence we discardit in this case and just return an empty result
    // Notice however that this code is never reached anyway since we
    // run into a timeout and thus do NOT perform an orderly shutdown.
    // The process will therefore exit with a code 1 anyway.
    Ok(())
}

async fn run(root: tosub::Subsystem) -> miette::Result<()> {
    root.spawn("child 1", child1);
    root.spawn("child 2", child2);
    root.spawn("child 3", child3);

    root.shutdown_requested().await;

    Ok(())
}

async fn child1(subsystem: tosub::Subsystem) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.spawn("grandchild 1", grandchild1);
    subsystem.spawn("grandchild 2", grandchild2);

    subsystem.shutdown_requested().await;

    println!("{} needs a second to shut down ...", subsystem.name());
    sleep(Duration::from_secs(1)).await;

    Ok(())
}

async fn grandchild2(subsystem: tosub::Subsystem) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.spawn("great grandchild 1", great_grandchild1);

    subsystem.shutdown_requested().await;

    println!("{} needs a second to shut down ...", subsystem.name());
    sleep(Duration::from_secs(1)).await;

    Ok(())
}

async fn great_grandchild1(subsystem: tosub::Subsystem) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.shutdown_requested().await;

    println!("{} needs ten seconds to shut down ...", subsystem.name());
    sleep(Duration::from_secs(10)).await;

    Ok(())
}

async fn grandchild1(subsystem: tosub::Subsystem) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.shutdown_requested().await;

    println!("{} shuts down immedaiately.", subsystem.name());

    Ok(())
}

async fn child2(subsystem: tosub::Subsystem) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.spawn("grandchild 3", grandchild3);

    subsystem.shutdown_requested().await;

    println!("{} needs two seconds to shut down ...", subsystem.name());
    sleep(Duration::from_secs(2)).await;

    Ok(())
}

async fn grandchild3(subsystem: tosub::Subsystem) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.shutdown_requested().await;

    println!("{} needs two second to shut down ...", subsystem.name());
    sleep(Duration::from_secs(2)).await;

    Ok(())
}

async fn child3(subsystem: tosub::Subsystem) -> miette::Result<()> {
    println!("Hello from {}", subsystem.name());

    subsystem.shutdown_requested().await;

    println!("{} needs thirty seconds to shut down ...", subsystem.name());
    sleep(Duration::from_secs(30)).await;

    Ok(())
}
