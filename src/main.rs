use anyhow::Result;
use sip2agent::config::{Config, SharedConfig};
use sip2agent::sip::SipServer;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, reload};

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
    // RUST_LOG, when set, wins over `log.level` — at startup and on reload.
    let (filter, rust_log) = match EnvFilter::try_from_default_env() {
        Ok(f) => (f, true),
        Err(_) => (EnvFilter::new(cfg.log.directive()), false),
    };
    let (filter, filter_handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();

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
    let shared = Arc::new(SharedConfig::new(cfg));
    let server = SipServer::bind_shared(shared.clone()).await?;
    tokio::spawn(reload_on_sighup(
        shared,
        config_path,
        filter_handle,
        rust_log,
    ));
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

/// SIGHUP → re-read the config file. Invalid files are rejected (running
/// config kept); valid ones apply to new calls only.
#[cfg(unix)]
async fn reload_on_sighup(
    shared: Arc<SharedConfig>,
    path: PathBuf,
    filter: reload::Handle<EnvFilter, Registry>,
    rust_log: bool,
) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut hup) = signal(SignalKind::hangup()) else {
        tracing::warn!("cannot install SIGHUP handler: config reload disabled");
        return;
    };
    while hup.recv().await.is_some() {
        tracing::info!(path = %path.display(), "SIGHUP: reloading config");
        let report = match shared.reload_file(&path) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    error = format!("{e:#}"),
                    "config reload failed; keeping the running config"
                );
                continue;
            }
        };
        let Some(log) = report.log_level else {
            continue;
        };
        if rust_log {
            tracing::info!(
                "log.level changed but RUST_LOG is set and takes precedence; log filter unchanged"
            );
            continue;
        }
        match EnvFilter::try_new(log.directive()) {
            Ok(f) => {
                if let Err(e) = filter.reload(f) {
                    tracing::warn!(error = %e, "log filter reload failed");
                } else {
                    tracing::info!(level = %log.level, "log level updated");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, level = %log.level, "invalid log.level; log filter unchanged")
            }
        }
    }
}

#[cfg(not(unix))]
async fn reload_on_sighup(
    _shared: Arc<SharedConfig>,
    _path: PathBuf,
    _filter: reload::Handle<EnvFilter, Registry>,
    _rust_log: bool,
) {
}
