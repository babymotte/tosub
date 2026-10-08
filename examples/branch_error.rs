use std::{io, process::ExitCode, time::Duration};
use tokio::{select, time::sleep};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> miette::Result<ExitCode> {
    tracing_subscriber::registry()
        .with(
            fmt::Layer::new().with_writer(io::stderr).with_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            ),
        )
        .init();

    tosub::build_root("root")
        .catch_signals()
        .with_timeout(Duration::from_secs(5))
        .start(|s| async move {
            let Some(subsys) = s.spawn("branch", |s| async move {
                let Some(tick) = s.spawn("tick", |s| async move {
                    for i in 0..10 {
                        select! {
                            _ = sleep(Duration::from_millis(950)) => println!("tick {i}"),
                            _ = s.shutdown_requested() => break,
                        }
                    }
                    Ok::<(), miette::Error>(())
                }) else {
                    return Ok(());
                };

                let Some(tock) = s.spawn("tock", |s| async move {
                    for i in 0..10 {
                        if i == 4 {
                            return Err(miette::miette!("Oh noes!"));
                        }
                        select! {
                            _ = sleep(Duration::from_millis(1000)) => println!("tock {i}"),
                            _ = s.shutdown_requested() => break,
                        }
                    }
                    Ok::<(), miette::Error>(())
                }) else {
                    return Ok(());
                };

                let _ = tick.join().await;
                let _ = tock.join().await;
                s.request_global_shutdown();

                Ok::<(), miette::Error>(())
            }) else {
                return Ok(());
            };
            let _ = subsys.join().await;

            Ok::<(), miette::Error>(())
        })
        .await
}
