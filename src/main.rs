mod api;
mod commands;
mod config;
mod profile;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(name = "codexctl", about = "Manage multiple Codex CLI accounts")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show rate limit status for all accounts
    Status,
    /// Save current ~/.codex/auth.json as a profile
    Save {
        /// Custom alias (defaults to email)
        alias: Option<String>,
    },
    /// Switch to a profile by alias (or most available if omitted)
    Use {
        /// Profile alias to switch to (auto-selects most available if omitted)
        alias: Option<String>,
    },
    /// Interactive fuzzy picker to switch accounts
    Switch,
    /// List saved profiles
    List,
    /// Remove a saved profile
    Remove {
        /// Profile alias to remove
        alias: String,
    },
    /// Show current active account
    Whoami,
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

fn main() {
    if let Err(e) = config::ensure_dirs() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Status => commands::status::run(),
        Commands::Save { ref alias } => commands::save::run(alias.as_deref()),
        Commands::Use { ref alias } => commands::use_profile::run(alias.as_deref()),
        Commands::Switch => commands::switch::run(),
        Commands::List => commands::list::run(),
        Commands::Remove { ref alias } => commands::remove::run(alias),
        Commands::Whoami => commands::whoami::run(),
        Commands::Completions { shell } => commands::completions::run(shell),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
