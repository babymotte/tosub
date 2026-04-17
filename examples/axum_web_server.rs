use axum::{Router, response::Html, routing::get};
use miette::{Context, IntoDiagnostic};
use std::time::Duration;
use tosub::{SubsystemHandle, SubsystemResult};
use tracing::info;

#[tokio::main(flavor = "current_thread")]
async fn main() -> SubsystemResult {
    tracing_subscriber::fmt::init();

    tosub::build_root("simple_axum_node_api")
        .catch_signals()
        .with_timeout(Duration::from_secs(1))
        .start(run)
        .await
}

async fn run(subsys: SubsystemHandle) -> miette::Result<()> {
    info!("building router …");
    let app = Router::new().route("/", get(handler));

    info!("creating socket …");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .into_diagnostic()
        .wrap_err("failed to bind to 127.0.0.1:3000")?;

    info!("Hello Worl endpoint running at http://127.0.0.1:3000");

    axum::serve(listener, app)
        .with_graceful_shutdown(subsys.into_shutdown_requested())
        .await
        .into_diagnostic()
        .wrap_err("failed to start axum server")?;

    Ok(())
}

async fn handler() -> Html<&'static str> {
    Html("<h1>Hello, World!</h1>")
}
