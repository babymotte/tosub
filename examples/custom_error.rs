use std::{env, io};
use tosub::SubsystemError;
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

    if env::args().count() > 1 {
        Err(SubsystemError::Custom("Arguments were provided".to_string()).into())
    } else {
        Err(SubsystemError::Custom("No arguments were provided".to_string()).into())
    }
}
