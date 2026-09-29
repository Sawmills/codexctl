use clap::{Parser, Subcommand};
use codexctl::central;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(about = "Experimental server-owned Codex credentials (one account, loopback only)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
    /// Serve access tokens. Remote devices connect through an SSH tunnel.
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
    },
    /// Run one prompt through a local App Server with centrally supplied authentication.
    Run {
        #[arg(long, default_value = "http://127.0.0.1:8787")]
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

#[tokio::main]
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
        Commands::Serve {
            state,
            key_file,
            listen,
            codex_bin,
            read_only,
        } => central::serve(&state, &key_file, listen, &codex_bin, read_only).await?,
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
