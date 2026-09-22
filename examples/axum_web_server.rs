use axum::{Router, response::Html, routing::get};
use miette::{Context, IntoDiagnostic};
use std::time::Duration;
use tosub::Subsystem;
use tracing::info;

#[tokio::main(flavor = "current_thread")]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt::init();

    tosub::build_root("simple_axum_node_api")
        .catch_signals()
        .with_timeout(Duration::from_secs(1))
        .start(run)
        .await?;

    Ok(())
}

async fn run(subsys: Subsystem) -> miette::Result<()> {
    info!("building router …");
    let app = Router::new().route("/", get(handler));

    let bind_addr = "127.0.0.1";
    let port = 3000;

    let addr = format!("{bind_addr}:{port}");

    info!("creating socket …");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .into_diagnostic()
        .wrap_err("failed to bind to 127.0.0.1:3000")?;

    info!("Hello World endpoint running at {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(subsys.into_shutdown_requested())
        .await
        .into_diagnostic()
        .wrap_err("failed to start axum server")
}

async fn handler() -> Html<&'static str> {
    Html("<h1>Hello, World!</h1>")
}
