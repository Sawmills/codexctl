//! `codexctl relay` commands.

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result};

use super::{DEFAULT_LISTEN, RelayConfig};

#[derive(clap::Subcommand)]
pub enum RelayAction {
    /// Route every account-server launch on this host through the loopback relay.
    Enable {
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
    },
    /// Restore direct OpenAI routing for account-server launches on this host.
    Disable,
    /// Report seven-day request and capacity counts per model and UTC hour.
    Evidence {
        #[arg(long)]
        json: bool,
    },
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
    /// Exit 0 only when the relay proves its loopback listener identity.
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
        RelayAction::Evidence { json } => {
            let directory = crate::config::default_paths()?
                .codexctl_dir()
                .join("relay/evidence");
            print!("{}", super::evidence::render(&directory, json)?);
            if json {
                println!();
            }
            Ok(())
        }
        RelayAction::Enable { listen } => {
            crate::central::native::configure_relay(Some(listen))?;
            println!("account-server launches use the relay at {listen}");
            Ok(())
        }
        RelayAction::Disable => {
            crate::central::native::configure_relay(None)?;
            println!("account-server launches use direct OpenAI routing");
            Ok(())
        }
        RelayAction::Serve {
            listen,
            upstream,
            rate_budget_secs,
            overloaded_budget_secs,
        } => {
            let config = RelayConfig::new(&upstream)?
                .with_evidence_dir(
                    crate::config::default_paths()?
                        .codexctl_dir()
                        .join("relay/evidence"),
                )
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
            super::identity::require_listener(
                &crate::config::default_paths()?.codexctl_dir(),
                listen,
            )?;
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
