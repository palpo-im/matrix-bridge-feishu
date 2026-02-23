use clap::Parser;
use matrix_appservice_feishu::{bridge::FeishuBridge, config::Config};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(name = "matrix-appservice-feishu")]
#[command(version)]
#[command(about = "A Matrix-Feishu puppeting bridge")]
struct Args {
    /// Path to config file
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,

    /// Generate example config and exit
    #[arg(long)]
    generate_config: bool,
}

const EXAMPLE_CONFIG: &str = include_str!("../example-config.yaml");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.generate_config {
        println!("{}", EXAMPLE_CONFIG);
        return Ok(());
    }

    FmtSubscriber::builder()
        .with_max_level(Level::DEBUG)
        .pretty()
        .init();

    info!(
        "Starting Matrix-Feishu bridge v{}",
        env!("CARGO_PKG_VERSION")
    );

    let config_path = args.config.to_string_lossy();
    info!("Loading config from {}", config_path);

    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load config: {}", e);
            return Err(e);
        }
    };

    let bridge = FeishuBridge::new(config).await?;
    let bridge = Arc::new(bridge);

    let bridge_clone = bridge.clone();
    tokio::select! {
        _ = bridge_clone.start() => {
            info!("Bridge started");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
        }
    }

    bridge.stop().await;
    info!("Bridge stopped");

    Ok(())
}
