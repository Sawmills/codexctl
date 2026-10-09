//! `codexctl relay` commands.

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, bail};

use super::{DEFAULT_LISTEN, RelayConfig};

#[derive(clap::Subcommand)]
pub enum RelayAction {
    /// Run the relay in the foreground; launchd or systemd keeps it running.
    Serve {
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
        #[arg(long, hide = true, default_value = super::CHATGPT)]
        upstream: String,
        /// How long a 429 streak may keep advising before Codex stops.
        #[arg(long, default_value_t = super::DEFAULT_RATE_BUDGET.as_secs())]
        rate_budget_secs: u64,
        /// How long a model-capacity streak may keep advising before Codex stops.
        #[arg(long, default_value_t = super::DEFAULT_OVERLOADED_BUDGET.as_secs())]
        overloaded_budget_secs: u64,
    },
    /// Exit 0 only when the relay answers on its loopback address.
    Status {
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
    },
    /// Print a service definition that restarts the relay when it exits.
    Unit {
        #[arg(value_enum)]
        kind: UnitKind,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum UnitKind {
    Launchd,
    Systemd,
}

pub fn run(action: RelayAction) -> Result<()> {
    match action {
        RelayAction::Serve {
            listen,
            upstream,
            rate_budget_secs,
            overloaded_budget_secs,
        } => {
            let config = RelayConfig::new(&upstream)?
                .with_budgets(
                    Duration::from_secs(rate_budget_secs),
                    Duration::from_secs(overloaded_budget_secs),
                )
                .with_active_central_reporter();
            tokio::runtime::Runtime::new()?.block_on(async move {
                // A bind failure is fatal so the service manager reports it.
                let listener = tokio::net::TcpListener::bind(listen)
                    .await
                    .with_context(|| format!("relay could not bind {listen}"))?;
                super::serve(listener, config, shutdown_signal()).await
            })
        }
        RelayAction::Status { listen } => {
            let url = format!("http://{listen}/healthz");
            let response = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()?
                .get(&url)
                .send()
                .with_context(|| format!("relay is not answering at {listen}"))?;
            if !response.status().is_success() {
                bail!("relay at {listen} answered {}", response.status());
            }
            println!("relay ok at {listen}");
            Ok(())
        }
        RelayAction::Unit { kind } => {
            let exe = std::env::current_exe()?;
            let exe = exe.to_str().context("codexctl path must be UTF-8")?;
            print!("{}", unit(kind, exe));
            Ok(())
        }
    }
}

/// Stops accepting on SIGTERM or Ctrl-C; open streams then drain until the
/// service manager's stop timeout.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

fn unit(kind: UnitKind, exe: &str) -> String {
    match kind {
        UnitKind::Launchd => format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>ai.sawmills.codexctl-relay</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>relay</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ExitTimeOut</key><integer>30</integer>
  <key>StandardErrorPath</key><string>/tmp/codexctl-relay.log</string>
</dict>
</plist>
"#
        ),
        UnitKind::Systemd => format!(
            "[Unit]\nDescription=codexctl relay (retry advice for Codex lanes)\n\n[Service]\nExecStart={exe} relay serve\nRestart=always\nRestartSec=2\nTimeoutStopSec=30\n\n[Install]\nWantedBy=default.target\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_restart_the_relay_and_bound_the_drain() {
        let launchd = unit(UnitKind::Launchd, "/bin/codexctl");
        assert!(launchd.contains("<key>KeepAlive</key><true/>"));
        assert!(launchd.contains(
            "<string>/bin/codexctl</string><string>relay</string><string>serve</string>"
        ));
        let systemd = unit(UnitKind::Systemd, "/bin/codexctl");
        assert!(systemd.contains("ExecStart=/bin/codexctl relay serve\nRestart=always"));
        assert!(systemd.contains("TimeoutStopSec=30"));
    }
}
