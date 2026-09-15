use miette::IntoDiagnostic;
use std::{io, thread};
use tokio::{select, sync::mpsc};
use tosub::Subsystem;
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

    tosub::build_default_root("root")
        .start(run)
        .await
        .into_diagnostic()?;
    Ok(())
}

async fn run(subsys: Subsystem) -> miette::Result<()> {
    let (tx, mut rx) = mpsc::channel(100);

    thread::spawn(move || {
        let stdin = std::io::stdin().lines();
        for line in stdin {
            if let Ok(line) = line {
                if tx.blocking_send(line).is_err() {
                    break;
                }
            } else {
                break;
            }
        }
    });

    loop {
        select! {
            recv = rx.recv() => {
                if let Some(line) = recv {
                    println!("got line: {line}");
                } else {
                    break;
                }
            }
            _ = subsys.shutdown_requested() => break,
        }
    }

    Ok(())
}
