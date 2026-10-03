//! Standalone company account server. Provider runtimes are explicitly enabled.
use clap::{Parser, Subcommand};
use codexctl::central::{self, providers::Provider};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(version, about = "Private company account server for Claude and Codex")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Check a local server's readiness without loading credentials or contacting providers.
    HealthCheck {
        #[arg(long, default_value = "127.0.0.1:8787")]
        address: SocketAddr,
    },
    Setup {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        key_file: PathBuf,
    },
    Users {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        user: Option<String>,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
    },
    GrantProvider {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        machine: String,
        #[arg(long, value_enum, value_delimiter = ',', required = true)]
        providers: Vec<Provider>,
    },
    Revoke {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        machine: String,
    },
    Serve {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: SocketAddr,
        #[arg(long)]
        public_url: String,
        #[arg(long)]
        sso_config: Option<PathBuf>,
        #[arg(long)]
        metrics_token_file: Option<PathBuf>,
        #[arg(long, value_enum, value_delimiter = ',', default_value = "anthropic")]
        providers: Vec<Provider>,
        #[arg(long, default_value = "codex")]
        codex_bin: PathBuf,
        #[arg(long)]
        read_only: bool,
    },
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Commands::HealthCheck { address } => {
            anyhow::ensure!(address.ip().is_loopback(), "health checks require loopback");
            let response = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(3))
                .build()?
                .get(format!("http://{address}/ready"))
                .send()
                .await?;
            anyhow::ensure!(
                response.status() == reqwest::StatusCode::OK,
                "account server is not ready"
            );
        }
        Commands::Setup { state, key_file } => central::managed::setup(&state, &key_file)?,
        Commands::Users {
            state,
            user,
            enable,
            disable,
        } => {
            if enable || disable {
                central::managed::set_user(
                    &state,
                    user.as_deref()
                        .ok_or_else(|| anyhow::anyhow!("--user required"))?,
                    enable,
                )?;
            } else {
                for user in central::managed::users(&state)? {
                    println!("{} {} {}", user.id, user.email, user.enabled);
                }
            }
        }
        Commands::GrantProvider {
            state,
            machine,
            providers,
        } => central::providers::grant(&state, &machine, providers)?,
        Commands::Revoke { state, machine } => central::revoke(&state, &machine)?,
        Commands::Serve {
            state,
            key_file,
            listen,
            public_url,
            sso_config,
            metrics_token_file,
            providers,
            codex_bin,
            read_only,
        } => {
            central::managed::serve_providers(
                &state,
                &key_file,
                listen,
                &codex_bin,
                read_only,
                &public_url,
                sso_config.as_deref(),
                metrics_token_file.as_deref(),
                providers,
            )
            .await?;
        }
    }
    Ok(())
}
