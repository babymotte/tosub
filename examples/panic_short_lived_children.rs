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
                    .with_env_var("WORTERBUCH_LOG")
                    .from_env_lossy(),
            ),
        )
        .init();

    tosub::build_root("hello_world")
        .catch_signals()
        .with_timeout(Duration::from_secs(1))
        .start(|root| async move {
            root.spawn("child 1", |subsystem| async move {
                println!("Hello from {}", subsystem.name());
                println!("Bye from {}", subsystem.name());
                Ok::<(), miette::ErrReport>(())
            });
            root.spawn("child 2", |subsystem| async move {
                println!("Hello from {}", subsystem.name());
                subsystem.spawn("child 1", |subsubsystem| async move {
                    println!("Hello from {}", subsubsystem.name());
                    sleep(Duration::from_millis(50)).await;
                    subsubsystem.shutdown_requested().await;
                    println!("Bye from {}", subsubsystem.name());
                    Ok::<(), miette::ErrReport>(())
                });
                sleep(Duration::from_millis(100)).await;
                println!("Bye from {}", subsystem.name());
                Ok::<(), miette::ErrReport>(())
            });
            root.spawn("child 3", |subsystem| async move {
                println!("Hello from {}", subsystem.name());
                sleep(Duration::from_millis(200)).await;
                println!("Bye from {}", subsystem.name());
                Ok::<(), miette::ErrReport>(())
            });

            root.spawn("child 4", |subsystem| async move {
                println!("Hello from {}", subsystem.name());
                sleep(Duration::from_secs(1)).await;
                panic!("Oopsie whoopsie!");
                #[allow(unreachable_code)]
                Ok::<(), miette::ErrReport>(())
            });

            root.shutdown_requested().await;
            Ok::<(), miette::ErrReport>(())
        })
        .await
        .into_diagnostic()?;
    Ok(())
}
