use miette::IntoDiagnostic;
use std::{io, time::Duration};
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

    tosub::build_root("root")
        .catch_signals()
        .with_timeout(Duration::from_secs(5))
        .start(|root| async move {
            root.spawn("tick", |s| async move {
                for i in 0..10 {
                    println!("tick {i}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                s.request_global_shutdown();
                Ok::<(), miette::Report>(())
            });

            root.shutdown_requested().await;
            Ok::<(), miette::Report>(())
        })
        .await
        .into_diagnostic()?;
    Ok(())
}
