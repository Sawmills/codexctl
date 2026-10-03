use clap::{Parser, Subcommand};
use codexctl::central;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(about = "Server-owned Codex credentials with company SSO and user isolation")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize an empty multi-user server. No account credentials are required.
    Setup {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        key_file: PathBuf,
    },
    /// List users, or enable or disable an SSO user. Requires server filesystem access.
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
    /// Restore the Codex provider that was selected before remote use.
    Disconnect,
    /// Register a remote account alias for ordinary codexctl use.
    Connect {
        #[arg(long)]
        alias: String,
        #[arg(long)]
        server: String,
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Import credentials into a new encrypted server vault. Transfer refresh ownership first.
    Init {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long)]
        auth: PathBuf,
        #[arg(long)]
        alias: String,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
    },
    /// Register a device and write its new secret to a private file.
    Register {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        device: String,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Revoke future broker access. Already-issued access tokens remain valid until expiry.
    Revoke {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        device: String,
    },
    /// Apply central PostgreSQL schema migrations when PostgreSQL storage is enabled.
    Migrate {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        key_file: PathBuf,
    },
    /// Serve the account API behind a private HTTPS ingress.
    Serve {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: SocketAddr,
        #[arg(long, default_value = "codex")]
        codex_bin: PathBuf,
        #[arg(long)]
        read_only: bool,
        /// HTTPS origin served by the private ingress. Required for managed mode.
        #[arg(long)]
        public_url: Option<String>,
        /// Company OIDC configuration. Required for a network listener.
        #[arg(long)]
        sso_config: Option<PathBuf>,
        /// A separate private bearer credential for monitoring only.
        #[arg(long)]
        metrics_token_file: Option<PathBuf>,
    },
    /// Run one prompt through a local App Server with centrally supplied authentication.
    Run {
        #[arg(long)]
        server: String,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long, default_value = "codex")]
        codex_bin: PathBuf,
        #[arg(long, default_value = "gpt-6.1-sol")]
        model: String,
        prompt: String,
    },
}

// Native children use Linux PR_SET_PDEATHSIG, which follows the spawning thread.
// Spawn directly on this long-lived main thread, never in spawn_blocking.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Commands::Connect {
        alias,
        server,
        token_file,
    } = &cli.command
    {
        let result = std::thread::scope(|scope| {
            scope
                .spawn(|| central::native::connect(alias, server, token_file))
                .join()
                .expect("connection worker panicked")
        });
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    let result = execute(cli).await;
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn execute(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
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
                    println!(
                        "{} {} {}",
                        user.id,
                        user.email,
                        if user.enabled { "enabled" } else { "disabled" }
                    );
                }
            }
        }
        Commands::Disconnect => central::native::deactivate()?,
        Commands::Connect { .. } => unreachable!("handled before async dispatch"),
        Commands::Init {
            state,
            key_file,
            auth,
            alias,
            tenant,
            user,
        } => central::init(&state, &key_file, &auth, &alias, &tenant, &user)?,
        Commands::Register {
            state,
            device,
            tenant,
            user,
            token_file,
        } => central::register(&state, &device, &tenant, &user, &token_file)?,
        Commands::Revoke { state, device } => central::revoke(&state, &device)?,
        Commands::Migrate { state, key_file } => {
            central::storage::maybe_migrate(&state, &key_file).await?
        }
        Commands::Serve {
            state,
            key_file,
            listen,
            codex_bin,
            read_only,
            public_url,
            sso_config,
            metrics_token_file,
        } => {
            if state.join("users.json").exists() {
                central::managed::serve(
                    &state,
                    &key_file,
                    listen,
                    &codex_bin,
                    read_only,
                    public_url
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("managed server requires --public-url"))?,
                    sso_config.as_deref(),
                    metrics_token_file.as_deref(),
                )
                .await?;
            } else {
                if public_url.is_some() || sso_config.is_some() || metrics_token_file.is_some() {
                    anyhow::bail!("run setup before managed server use");
                }
                central::serve(&state, &key_file, listen, &codex_bin, read_only).await?;
            }
        }
        Commands::Run {
            server,
            token_file,
            codex_bin,
            model,
            prompt,
        } => {
            let response = central::run_client(
                &server,
                &token_file,
                &codex_bin,
                &std::env::current_dir()?,
                &model,
                &prompt,
            )
            .await?;
            println!("{response}");
        }
    }
    Ok(())
}
