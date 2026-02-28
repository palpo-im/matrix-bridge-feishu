use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use clap::{Args as ClapArgs, Parser, Subcommand};
use matrix_bridge_feishu::bridge::FeishuBridge;
use matrix_bridge_feishu::config::Config;
use reqwest::Client;
use serde_json::{Value, json};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(name = "matrix-appservice-feishu")]
#[command(version)]
#[command(about = "A Matrix-Feishu puppeting bridge")]
struct CliArgs {
    /// Path to config file
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,

    /// Generate example config and exit
    #[arg(long)]
    generate_config: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show provisioning/admin runtime status
    Status(StatusCommand),
    /// List current Matrix <-> Feishu mappings
    Mappings(MappingsCommand),
    /// Replay dead-letters (single id or a batch)
    Replay(ReplayCommand),
    /// Cleanup dead-letters by status/time window
    DeadLetterCleanup(DeadLetterCleanupCommand),
}

#[derive(ClapArgs, Debug, Clone)]
struct AdminApiTarget {
    /// Admin API base URL, default: http://<hostname>:<port>/admin from config
    #[arg(long)]
    admin_api: Option<String>,
    /// Bearer token, defaults to scope-specific provisioning token env vars
    #[arg(long)]
    token: Option<String>,
}

#[derive(ClapArgs, Debug)]
struct StatusCommand {
    #[command(flatten)]
    target: AdminApiTarget,
}

#[derive(ClapArgs, Debug)]
struct MappingsCommand {
    #[arg(long, default_value_t = 100)]
    limit: i64,
    #[arg(long, default_value_t = 0)]
    offset: i64,
    #[command(flatten)]
    target: AdminApiTarget,
}

#[derive(ClapArgs, Debug)]
struct ReplayCommand {
    /// Replay a specific dead-letter id
    #[arg(long)]
    id: Option<i64>,
    /// Batch replay filter status when --id is absent
    #[arg(long)]
    status: Option<String>,
    /// Batch replay size when --id is absent
    #[arg(long, default_value_t = 20)]
    limit: i64,
    #[command(flatten)]
    target: AdminApiTarget,
}

#[derive(ClapArgs, Debug)]
struct DeadLetterCleanupCommand {
    #[arg(long)]
    status: Option<String>,
    #[arg(long)]
    older_than_hours: Option<i64>,
    #[arg(long, default_value_t = 200)]
    limit: i64,
    #[arg(long)]
    dry_run: bool,
    #[command(flatten)]
    target: AdminApiTarget,
}

#[derive(Debug, Clone, Copy)]
enum TokenScope {
    Read,
    Write,
    Delete,
}

const EXAMPLE_CONFIG: &str = include_str!("../example-config.yaml");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();

    if args.generate_config {
        println!("{}", EXAMPLE_CONFIG);
        return Ok(());
    }

    let config_path = args.config.to_string_lossy().to_string();
    let config = Config::load(&config_path).with_context(|| {
        format!(
            "Failed to load config at '{}'; use --generate-config to print a template",
            config_path
        )
    })?;

    if let Some(command) = args.command {
        run_management_command(command, &config).await?;
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

async fn run_management_command(command: Command, config: &Config) -> anyhow::Result<()> {
    let client = Client::builder()
        .build()
        .context("failed to create HTTP client")?;

    match command {
        Command::Status(cmd) => {
            let (base, token) = resolve_admin_access(config, &cmd.target, TokenScope::Read);
            let response = api_get(&client, &format!("{base}/status"), &token).await?;
            print_json(&response)?;
        }
        Command::Mappings(cmd) => {
            let (base, token) = resolve_admin_access(config, &cmd.target, TokenScope::Read);
            let url = format!(
                "{base}/mappings?limit={}&offset={}",
                cmd.limit.max(1),
                cmd.offset.max(0)
            );
            let response = api_get(&client, &url, &token).await?;
            print_json(&response)?;
        }
        Command::Replay(cmd) => {
            let (base, token) = resolve_admin_access(config, &cmd.target, TokenScope::Write);
            let response = if let Some(id) = cmd.id {
                api_post_json(
                    &client,
                    &format!("{base}/dead-letters/{id}/replay"),
                    &token,
                    json!({}),
                )
                .await?
            } else {
                api_post_json(
                    &client,
                    &format!("{base}/dead-letters/replay"),
                    &token,
                    json!({
                        "status": cmd.status,
                        "limit": cmd.limit.max(1),
                    }),
                )
                .await?
            };
            print_json(&response)?;
        }
        Command::DeadLetterCleanup(cmd) => {
            let (base, token) = resolve_admin_access(config, &cmd.target, TokenScope::Delete);
            let response = api_post_json(
                &client,
                &format!("{base}/dead-letters/cleanup"),
                &token,
                json!({
                    "status": cmd.status,
                    "older_than_hours": cmd.older_than_hours,
                    "limit": cmd.limit.max(1),
                    "dry_run": cmd.dry_run,
                }),
            )
            .await?;
            print_json(&response)?;
        }
    }

    Ok(())
}

fn resolve_admin_access(
    config: &Config,
    target: &AdminApiTarget,
    required_scope: TokenScope,
) -> (String, String) {
    let base = target.admin_api.clone().unwrap_or_else(|| {
        format!(
            "http://{}:{}/admin",
            config.appservice.hostname, config.appservice.port
        )
    });

    let token = target
        .token
        .clone()
        .or_else(|| env_token_for_scope(required_scope))
        .unwrap_or_else(|| config.appservice.as_token.clone());

    (base.trim_end_matches('/').to_string(), token)
}

fn env_token_for_scope(scope: TokenScope) -> Option<String> {
    match scope {
        TokenScope::Read => std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_READ_TOKEN")
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_WRITE_TOKEN"))
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_DELETE_TOKEN"))
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN"))
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN"))
            .ok(),
        TokenScope::Write => std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_WRITE_TOKEN")
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_DELETE_TOKEN"))
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN"))
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN"))
            .ok(),
        TokenScope::Delete => std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_DELETE_TOKEN")
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN"))
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_WRITE_TOKEN"))
            .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN"))
            .ok(),
    }
}

async fn api_get(client: &Client, url: &str, token: &str) -> anyhow::Result<Value> {
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("GET request failed: {url}"))?;
    decode_api_response(response).await
}

async fn api_post_json(
    client: &Client,
    url: &str,
    token: &str,
    body: Value,
) -> anyhow::Result<Value> {
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST request failed: {url}"))?;
    decode_api_response(response).await
}

async fn decode_api_response(response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read API response body")?;
    let payload = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }))
    };

    if !status.is_success() {
        return Err(anyhow!(
            "API request failed: status={} payload={}",
            status,
            payload
        ));
    }

    Ok(payload)
}

fn print_json(value: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
