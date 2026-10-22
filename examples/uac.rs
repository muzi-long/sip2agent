//! Test UAC: place one call to sip2agent, stream a 440 Hz tone, report RTP.
//!
//! cargo run --example uac -- --server 127.0.0.1:5060 --to 9000 --secs 5 [--codec pcmu] [--wait]
//!
//! `--wait` keeps the call up until the far end hangs up (or --secs elapses).

use sip2agent::audio::g711::G711;
use sip2agent::uac::{UacOptions, run};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let mut opts = UacOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{a} needs a value"))
        };
        match a.as_str() {
            "--server" => opts.server = val()?.parse()?,
            "--to" => opts.to = val()?,
            "--from" => opts.from = val()?,
            "--secs" => opts.duration = Duration::from_secs_f64(val()?.parse()?),
            "--codec" => {
                let c = val()?;
                opts.codecs =
                    vec![G711::from_name(&c).ok_or_else(|| anyhow::anyhow!("unknown codec {c}"))?];
            }
            "--local-ip" => opts.local_ip = val()?.parse()?,
            "--silence" => opts.tone_hz = 0.0,
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let report = run(opts).await?;
    println!("{report:#?}");
    if report.status != 200 {
        std::process::exit(2);
    }
    Ok(())
}
