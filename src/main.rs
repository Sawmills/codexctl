mod commands;

use codexctl::{api, config, profile, store};

use clap::{Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(
    name = "codexctl",
    about = "Manage multiple Codex CLI accounts",
    version
)]
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
        /// Allow automatic selection of a credit-billing account without
        /// prompting (use for unattended runs; it may spend credits)
        #[arg(long)]
        allow_billing: bool,
        /// When auto-selecting and no account has headroom left, redeem a
        /// banked reset without prompting (resets are scarce and expire)
        #[arg(long)]
        allow_resets: bool,
    },
    /// Interactive fuzzy picker to switch accounts
    Switch,
    /// List banked rate-limit resets across all accounts
    Resets {
        /// Redeem every banked reset that is about to lapse on an account
        /// that is already rate-limited, instead of just listing them
        #[arg(long)]
        claim: bool,
        /// How soon a credit must lapse to be claimed, in days
        #[arg(long, default_value_t = 3, requires = "claim")]
        within_days: i64,
        /// Claim without confirming (for unattended runs)
        #[arg(long, short = 'y', requires = "claim")]
        yes: bool,
    },
    /// Redeem a banked rate-limit reset to clear an exhausted window
    Reset {
        /// Profile alias to redeem for (defaults to the active account)
        alias: Option<String>,
        /// Redeem without confirming (for unattended runs)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Redeem a specific credit id (defaults to the soonest-expiring one)
        #[arg(long)]
        credit: Option<String>,
    },
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
        /// Allow recovery to switch to a credit-billing account without
        /// prompting (use for unattended runs; it may spend credits)
        #[arg(long)]
        allow_billing: bool,
        /// Allow recovery to redeem a banked rate-limit reset without
        /// prompting (use for unattended runs; resets are scarce and expire).
        /// A reset that would lapse before its window resets anyway is always
        /// redeemed without prompting, flag or not.
        #[arg(long)]
        allow_resets: bool,
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
    let cli = Cli::parse();

    // Informational commands must work in a read-only or empty home. Clap
    // exits while parsing help, version, and invalid commands, and shell
    // completion generation does not need the profile store.
    let needs_store = !matches!(&cli.command, Commands::Completions { .. });
    if needs_store && let Err(e) = config::ensure_dirs() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }

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
        Commands::Use {
            ref alias,
            allow_billing,
            allow_resets,
        } => commands::use_profile::run(alias.as_deref(), allow_billing, allow_resets),
        Commands::Switch => commands::switch::run(),
        Commands::Resets {
            claim,
            within_days,
            yes,
        } => {
            if claim {
                commands::resets::run_claim(within_days, yes)
            } else {
                commands::resets::run_list()
            }
        }
        Commands::Reset {
            ref alias,
            yes,
            ref credit,
        } => commands::resets::run_redeem(alias.as_deref(), yes, credit.as_deref()),
        Commands::List => commands::list::run(),
        Commands::Remove { ref alias } => commands::remove::run(alias),
        Commands::Whoami => commands::whoami::run(),
        Commands::Codex {
            ref args,
            ref recovery_prompt,
            allow_billing,
            allow_resets,
        } => codex_command_outcome(commands::codex::run(
            args,
            recovery_prompt,
            allow_billing,
            allow_resets,
        ))
        .into_result(),
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
