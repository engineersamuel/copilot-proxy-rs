use clap::Parser;
use copilot_proxy_rs::http::router;
use copilot_proxy_rs::{AppConfig, AppState};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

#[derive(Debug, Parser)]
#[command(
    name = "copilot-proxy-rs",
    version,
    about = "Rust API proxy for GitHub Copilot-backed OpenAI, Anthropic, and Responses clients"
)]
struct Cli {
    #[arg(long, env = "COPILOT_PROXY_RS_HOST")]
    host: Option<String>,

    #[arg(long, env = "COPILOT_PROXY_RS_PORT")]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut config = AppConfig::load()?;

    if let Some(host) = cli.host {
        config.host = host;
    }
    if let Some(port) = cli.port {
        config.port = port;
    }
    config.validate_bind_safety()?;

    copilot_proxy_rs::telemetry::init_logging()?;

    let address = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&address).await?;
    let state = AppState::new(config);
    let snapshot = state.backend.snapshot().await;
    copilot_proxy_rs::telemetry::log_startup(
        &format!("http://{address}"),
        env!("CARGO_PKG_VERSION"),
        snapshot.primary.as_str(),
        snapshot.fallback.map(|backend| backend.as_str()),
        std::process::id(),
    );
    let app = router(state).layer(TraceLayer::new_for_http());
    axum::serve(listener, app).await?;
    Ok(())
}
