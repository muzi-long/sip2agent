use anyhow::Result;
use sip2agent::config::Config;
use sip2agent::sip::SipServer;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: sip2agent [-c|--config <config.toml>] [--check]";

#[tokio::main]
async fn main() -> Result<()> {
    let mut config_path = PathBuf::from("config.toml");
    let mut check_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => {
                config_path = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!(USAGE))?;
            }
            "--check" => check_only = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("sip2agent {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => anyhow::bail!("unknown argument '{other}'\n{USAGE}"),
        }
    }

    let cfg = Config::load(&config_path)?;
    // rsipstack logs every raw SIP message at INFO; keep it at WARN unless
    // the directive mentions it explicitly.
    let directive = if cfg.log.level.contains("rsipstack") {
        cfg.log.level.clone()
    } else {
        format!("{},rsipstack=warn", cfg.log.level)
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directive));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if check_only {
        println!("config OK: {} agent(s)", cfg.agents.len());
        return Ok(());
    }
    if cfg.agents.is_empty() {
        tracing::warn!("no [[agent]] configured: every call will get 404");
    }
    for a in &cfg.agents {
        tracing::info!(agent = %a.name, r#match = %a.match_user, provider = a.provider.as_str(), "agent loaded");
    }

    sip2agent::install_crypto_provider();
    let server = SipServer::bind(Arc::new(cfg)).await?;
    let shutdown = CancellationToken::new();
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            wait_for_signal().await;
            shutdown.cancel();
        });
    }
    server.run(shutdown).await
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
