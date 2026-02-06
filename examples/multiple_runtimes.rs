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
use tokio::runtime::Handle;
use tosub::SubsystemHandle;
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main(flavor = "current_thread")]
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

    info!("Root runtime created.");

    tosub::build_root("root")
        .catch_signals()
        .with_timeout(Duration::from_secs(1))
        .start(run)
        .await
        .into_diagnostic()?;

    info!("Root runtime completed.");

    Ok(())
}

async fn run(root: SubsystemHandle) -> miette::Result<()> {
    root.spawn("1", move |s| run_subsys(s));
    root.spawn("2", move |s| run_subsys(s));
    root.spawn("3", move |s| run_subsys(s));
    root.shutdown_requested().await;
    Ok(())
}

async fn run_subsys(subsys: SubsystemHandle) -> miette::Result<()> {
    let s = subsys.clone();
    Handle::current().spawn_blocking(move || run_subsys_on_other_runtime(s));
    subsys.shutdown_requested().await;
    Ok(())
}

fn run_subsys_on_other_runtime(s: SubsystemHandle) -> Result<(), miette::Error> {
    info!("Creating new runtime for subsystem '{}'", s.name());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .into_diagnostic()?;
    info!("Runtime for subsystem '{}' created.", s.name());
    rt.block_on(do_something_on_other_runtime(s.clone()));
    Ok(())
}

async fn do_something_on_other_runtime(subsys: SubsystemHandle) {
    info!(
        "Hello from subsystem '{}' on runtime '{}'",
        subsys.name(),
        Handle::current().id()
    );
    subsys.shutdown_requested().await;
    info!("Subsys '{}' stopped.", subsys.name());
}
