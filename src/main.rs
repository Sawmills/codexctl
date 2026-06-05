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
    Status {
        /// Show only rate-limited accounts
        #[arg(long, conflicts_with = "usage_based")]
        rate_limited: bool,
        /// Show only usage-based accounts
        #[arg(long, conflicts_with = "rate_limited")]
        usage_based: bool,
    },
    /// Log into a Codex account in an isolated auth home and save it
    Login {
        /// Profile alias to save the login as
        alias: String,
    },
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
    /// Run Codex with automatic spend-cap account recovery
    Codex {
        /// Prompt sent when the wrapper resumes after switching profiles
        #[arg(long, default_value = commands::codex::DEFAULT_RECOVERY_PROMPT)]
        recovery_prompt: String,
        /// Arguments forwarded to codex
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
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
        Commands::Status {
            rate_limited,
            usage_based,
        } => {
            let filter = if rate_limited {
                commands::status::Filter::RateLimited
            } else if usage_based {
                commands::status::Filter::UsageBased
            } else {
                commands::status::Filter::All
            };
            commands::status::run(filter)
        }
        Commands::Login { ref alias } => commands::login::run(alias),
        Commands::Save { ref alias } => commands::save::run(alias.as_deref()),
        Commands::Use { ref alias } => commands::use_profile::run(alias.as_deref()),
        Commands::Switch => commands::switch::run(),
        Commands::List => commands::list::run(),
        Commands::Remove { ref alias } => commands::remove::run(alias),
        Commands::Whoami => commands::whoami::run(),
        Commands::Codex {
            ref args,
            ref recovery_prompt,
        } => codex_command_outcome(commands::codex::run(args, recovery_prompt)).into_result(),
        Commands::Completions { shell } => commands::completions::run(shell),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

enum CommandOutcome {
    Continue(anyhow::Result<()>),
    Exit(i32),
}

impl CommandOutcome {
    fn into_result(self) -> anyhow::Result<()> {
        match self {
            Self::Continue(result) => result,
            Self::Exit(code) => std::process::exit(code),
        }
    }
}

fn codex_command_outcome(result: anyhow::Result<i32>) -> CommandOutcome {
    match result {
        Ok(code) => CommandOutcome::Exit(code),
        Err(e) => CommandOutcome::Continue(Err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_subcommand_accepts_resume_without_separator() {
        let cli = Cli::parse_from([
            "codexctl",
            "codex",
            "resume",
            "019e9507-1bdc-7fd1-ac72-5705ee5cd793",
        ]);

        match cli.command {
            Commands::Codex { args, .. } => {
                assert_eq!(
                    args,
                    vec![
                        "resume".to_string(),
                        "019e9507-1bdc-7fd1-ac72-5705ee5cd793".to_string()
                    ]
                );
            }
            _ => panic!("expected codex command"),
        }
    }

    #[test]
    fn codex_command_outcome_preserves_child_exit_status() {
        match codex_command_outcome(Ok(130)) {
            CommandOutcome::Exit(code) => assert_eq!(code, 130),
            CommandOutcome::Continue(_) => panic!("expected process exit"),
        }
    }
}
