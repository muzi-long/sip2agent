use anyhow::{Context, Result};
use sip2agent::admin::{self, AdminState, LogApplier, Reloader};
use sip2agent::config::{Config, SharedConfig};
use sip2agent::sip::SipServer;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, reload};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = match args.first().map(String::as_str) {
        Some(cmd @ ("reload" | "stop" | "status")) => {
            std::process::exit(admin::cli_main(cmd, &args[1..]).await);
        }
        Some("run") => &args[1..],
        _ => &args[..],
    };

    let mut config_path = PathBuf::from("config.toml");
    let mut check_only = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-c" | "--config" => {
                config_path = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!(admin::CLI_USAGE))?;
            }
            "--check" => check_only = true,
            "-h" | "--help" => {
                println!("{}", admin::CLI_USAGE);
                return Ok(());
            }
            "-V" | "--version" => {
                println!("sip2agent {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => anyhow::bail!("unknown argument '{other}'\n{}", admin::CLI_USAGE),
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
    let admin_addr = cfg.admin.addr()?;
    let shared = Arc::new(SharedConfig::new(cfg));
    let server = SipServer::bind_shared(shared.clone()).await?;

    let on_log: LogApplier = Arc::new(move |log| {
        if rust_log {
            tracing::info!(
                "log.level changed but RUST_LOG is set and takes precedence; log filter unchanged"
            );
            return;
        }
        match EnvFilter::try_new(log.directive()) {
            Ok(f) => match filter_handle.reload(f) {
                Ok(()) => tracing::info!(level = %log.level, "log level updated"),
                Err(e) => tracing::warn!(error = %e, "log filter reload failed"),
            },
            Err(e) => {
                tracing::warn!(error = %e, level = %log.level, "invalid log.level; log filter unchanged")
            }
        }
    });
    let path_abs = std::fs::canonicalize(&config_path).unwrap_or(config_path);
    let reloader = Arc::new(Reloader::new(shared, path_abs, Some(on_log)));
    tokio::spawn(reload_on_sighup(reloader.clone()));

    if let Some(addr) = admin_addr {
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
            format!("bind admin API {addr} (set [admin] listen = \"\" to disable)")
        })?;
        if !addr.ip().is_loopback() {
            tracing::warn!(%addr, "admin API is not on a loopback address: anyone who can reach it can reload / stop the server");
        }
        tracing::info!(%addr, "admin API listening (sip2agent reload | stop | status)");
        let state = Arc::new(AdminState {
            reloader,
            control: server.control(),
        });
        tokio::spawn(admin::serve(listener, state));
    }

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

/// SIGHUP → same reload as `sip2agent reload`. Invalid files are rejected
/// (running config kept); valid ones apply to new calls only.
#[cfg(unix)]
async fn reload_on_sighup(reloader: Arc<Reloader>) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut hup) = signal(SignalKind::hangup()) else {
        tracing::warn!("cannot install SIGHUP handler: config reload disabled");
        return;
    };
    while hup.recv().await.is_some() {
        reloader.reload("SIGHUP").await;
    }
}

#[cfg(not(unix))]
async fn reload_on_sighup(_reloader: Arc<Reloader>) {}
