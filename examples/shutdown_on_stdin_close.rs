use std::io;
use tosub::{SubsystemHandle, SubsystemResult};
use tracing::{info, level_filters::LevelFilter};
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

    tosub::build_default_root("root")
        .shutdown_on_stdin_close()
        .with_stdin_consumer(|line| eprintln!("echo: {line}"))
        .start(run)
        .await
}

async fn run(subsys: SubsystemHandle) -> miette::Result<()> {
    info!("Hello, World! You can exit the app by pressing Ctrl+C or Ctrl+D (closing stdin).");

    subsys.shutdown_requested().await;
    Ok(())
}
