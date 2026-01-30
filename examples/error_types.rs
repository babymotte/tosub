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
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("Error type 0: {0}")]
struct Error0(&'static str);

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("Error type 1: {0}")]
struct Error1(&'static str);

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("Error type 2: {0}")]
struct Error2(&'static str);

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
        .start(run)
        .await
        .into_diagnostic()?;
    Ok(())
}

async fn run(root: tosub::SubsystemHandle) -> Result<(), Error0> {
    root.spawn("child 1", |_| async { Err::<(), Error1>(Error1("some")) });

    root.spawn("child 2", |_| async { Err::<(), Error2>(Error2("thing")) });

    Ok(())
}
