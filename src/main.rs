//! coban —— Codex 授权代理。
//!
//! 用 ChatGPT 订阅账号登录（Codex CLI 的 OAuth 流程），把多个账号的 token 存进 SQLite，
//! 再把 codex 客户端的请求转发到 `chatgpt.com/backend-api/codex`，按优先级与轮换选号。

mod admin_ui;
mod auth;
mod chat;
mod clients;
mod config;
mod credentials;
mod oauth;
mod pricing;
mod proxy;
mod quota_reset;
mod store;
mod web;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use store::CredentialStore;

#[derive(Parser)]
#[command(name = "coban", version, about = "Codex authorization proxy")]
struct Cli {
    /// Web service bind address (0.0.0.0 is reachable from the network; use 127.0.0.1 for local-only).
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    /// Web service port (used when running without a subcommand).
    #[arg(long, default_value_t = 4700)]
    port: u16,
    /// API key that callers such as the Codex CLI must present; also available through COBAN_API_KEY.
    /// If unset, the proxy does not authenticate callers; use it only on a trusted local network.
    #[arg(long, env = "COBAN_API_KEY")]
    api_key: Option<String>,
    /// Admin console password; also available through COBAN_ADMIN_PASSWORD.
    /// Once set, admin APIs require authentication. A CLI or environment value takes precedence
    /// and makes the web setting read-only.
    #[arg(long, env = "COBAN_ADMIN_PASSWORD")]
    admin_password: Option<String>,
    /// Open a browser after startup (off by default).
    #[arg(long)]
    open: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List all saved credentials.
    Status,
    /// Remove all saved credentials.
    Logout,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    let store = Arc::new(CredentialStore::open_default()?);

    match cli.command {
        // 不带子命令：直接启动网页服务 + 转发代理。
        None => {
            // 空串按「没设」处理：`COBAN_API_KEY=` 这种写法在 compose 里很常见，
            // 按字面收下会得到一个「谁都猜得到」的空 key。
            let api_key = cli.api_key.filter(|k| !k.trim().is_empty());
            let admin_password = cli.admin_password.filter(|k| !k.trim().is_empty());
            web::run(&cli.host, cli.port, cli.open, store, api_key, admin_password).await
        }
        Some(Command::Status) => status(&store),
        Some(Command::Logout) => logout(&store),
    }
}

/// 初始化日志：本地时间、干净格式、非终端自动关 ANSI 颜色。
/// 默认 info 级，`RUST_LOG` 可覆盖（如 `RUST_LOG=coban=debug`）。
fn init_logging() {
    use std::io::IsTerminal;
    use tracing_subscriber::{EnvFilter, fmt::time::ChronoLocal};
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".to_owned()))
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}

/// 列出所有凭证。
fn status(store: &CredentialStore) -> Result<()> {
    let list = store.list()?;
    if list.is_empty() {
        println!(
            "No credentials saved. Run `coban` without a subcommand to open the web UI and add an account."
        );
        return Ok(());
    }
    println!(
        "Saved credentials ({}; database: {}):",
        list.len(),
        CredentialStore::db_path()?.display()
    );
    for c in &list {
        let state = if let Some(reason) = &c.ban_reason {
            format!("disabled ({reason})")
        } else if c.disabled {
            match c.resume_at {
                Some(t) => format!("paused until {t} (rate limited)"),
                None => "disabled".to_string(),
            }
        } else if c.expires_in_secs() == 0 {
            "active; token expired (refreshes automatically)".to_string()
        } else {
            format!("active; token valid for {} min", c.expires_in_secs() / 60)
        };
        println!(
            "  #{:<3} [P{}] {:<28} {:<6} {}",
            c.id,
            c.priority,
            c.label,
            c.plan_type.as_deref().unwrap_or("-"),
            state
        );
    }
    Ok(())
}

/// 清空所有凭证。
fn logout(store: &CredentialStore) -> Result<()> {
    let n = store.clear()?;
    if n > 0 {
        let noun = if n == 1 { "credential" } else { "credentials" };
        println!("Cleared {n} {noun}, including the associated usage history.");
    } else {
        println!("No credentials to clear.");
    }
    Ok(())
}
