mod commands;

use codexctl::{api, config, daemon, profile, store};

#[cfg(feature = "central-prototype")]
use clap::ArgGroup;
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
    #[cfg(feature = "central-prototype")]
    /// Connect this machine to an account server with company SSO.
    Connect {
        #[arg(long)]
        server: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        no_browser: bool,
    },
    #[cfg(feature = "central-prototype")]
    /// Restore the previous provider. Optionally forget this machine's registration.
    Disconnect {
        #[arg(long)]
        forget: bool,
    },
    #[cfg(feature = "central-prototype")]
    /// Transfer local profiles to the server after stopping all credential owners.
    Migrate {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        exclusive_owner: bool,
    },
    #[cfg(feature = "central-prototype")]
    /// Repair or restore saved session providers in the active Codex home.
    SessionProvider {
        #[command(subcommand)]
        action: codexctl::central::native::SessionProviderAction,
    },
    #[cfg(feature = "central-prototype")]
    /// List your registered devices, or revoke one.
    Devices {
        #[arg(long)]
        revoke: Option<String>,
    },
    #[cfg(feature = "central-prototype")]
    /// Pin the Codex desktop app to a server account with ChatGPT features.
    #[cfg(feature = "central-prototype")]
    AppAuth {
        #[command(subcommand)]
        action: codexctl::central::native::AppAuthAction,
    },
    /// Lend a server account to a teammate, or manage your loans.
    Loans {
        #[command(subcommand)]
        action: codexctl::central::loans::client::LoansAction,
    },
    #[cfg(feature = "central-prototype")]
    #[command(hide = true)]
    #[command(group(ArgGroup::new("token_source").required(true).args(["connection", "active"])))]
    CentralToken {
        #[arg(long)]
        connection: Option<std::path::PathBuf>,
        #[arg(long)]
        active: bool,
    },
    /// Show rate limit status for all accounts
    Status {
        /// Print a versioned JSON document
        #[arg(long)]
        json: bool,
        /// Show only rate-limited accounts
        #[arg(long, conflicts_with = "usage_based")]
        rate_limited: bool,
        /// Show only usage-based accounts
        #[arg(long, conflicts_with = "rate_limited")]
        usage_based: bool,
    },
    /// Read per-account response and 429 counts from this host's Codex logs
    Rate {
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=525600))]
        minutes: u32,
    },
    /// Print cached active-account usage for prompts. Silent on unavailable data.
    Statusline,
    /// Log into a local Codex account or renew an existing server account
    Login {
        /// Profile alias to save the login as
        alias: String,
        /// Display label for the account, e.g. "personal" or "team"
        #[arg(long)]
        label: Option<String>,
        /// Replace a profile that records no account of its own, without
        /// prompting (needed on a terminal that cannot answer)
        #[arg(long)]
        allow_adopt: bool,
        /// Print the server-managed OpenAI sign-in link without opening a browser.
        #[arg(long)]
        no_browser: bool,
        /// Stop a pending server-managed login and wait for its terminal status.
        #[arg(long)]
        cancel: bool,
        /// On a connected machine, save a local profile instead of adding a
        /// server account. Server aliases still renew on the server.
        #[arg(long, conflicts_with_all = ["no_browser", "cancel"])]
        local: bool,
    },
    /// Save current ~/.codex/auth.json as a profile
    Save {
        /// Custom alias (defaults to email)
        alias: Option<String>,
        /// Display label for the account, e.g. "personal" or "team"
        #[arg(long)]
        label: Option<String>,
        /// Replace a profile that records no account of its own, without
        /// prompting (needed on a terminal that cannot answer)
        #[arg(long)]
        allow_adopt: bool,
    },
    /// Rename a server account alias; credentials and label stay
    #[cfg(feature = "central-prototype")]
    Rename {
        /// Current server account alias
        old: String,
        /// New alias
        new: String,
    },
    /// Set or clear a profile's display label
    Label {
        /// Profile alias to label
        alias: String,
        /// Label text (omit to clear the label)
        text: Option<String>,
    },
    /// Switch to a profile by alias (or most available if omitted)
    Use {
        /// Profile alias to switch to (auto-selects most available if omitted)
        #[arg(conflicts_with = "history")]
        alias: Option<String>,
        /// Print recent active-account pointer changes without changing accounts
        #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "20", conflicts_with_all = ["allow_billing", "allow_resets", "restart_daemon"])]
        history: Option<usize>,
        /// Print history as a JSON array
        #[arg(long, requires = "history")]
        json: bool,
        /// Deprecated compatibility flag. Automatic selection now warns and
        /// continues without prompting.
        #[arg(long, hide = true)]
        allow_billing: bool,
        /// When auto-selecting and no account has headroom left, redeem a
        /// banked reset without prompting (resets are scarce and expire)
        #[arg(long)]
        allow_resets: bool,
        /// Restart the Codex app-server daemon without prompting when it
        /// still runs on another account, then resume the sessions it stops
        /// with no sandbox and no approval prompts
        #[arg(long)]
        restart_daemon: bool,
    },
    /// Interactive fuzzy picker to switch accounts
    Switch,
    /// List banked rate-limit resets across all accounts
    Resets {
        /// Redeem one banked reset for an exhausted account
        #[arg(long, conflicts_with = "claim", group = "redemption")]
        redeem: Option<String>,
        /// Redeem every banked reset that is about to lapse on an account
        /// that is already rate-limited, instead of just listing them
        #[arg(long, group = "redemption")]
        claim: bool,
        /// How soon a credit must lapse to be claimed, in days
        #[arg(long, default_value_t = 3, requires = "claim")]
        within_days: i64,
        /// Redeem without confirming (for unattended runs)
        #[arg(long, short = 'y', requires = "redemption")]
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
    List {
        /// Print a versioned JSON document
        #[arg(long)]
        json: bool,
    },
    /// Remove a saved profile
    Remove {
        /// Profile alias to remove
        alias: String,
    },
    /// Show current active account
    Whoami,
    /// Run Codex with recovery, or pin one session to a server account
    Codex {
        /// Pin this launch to one server account without switching other sessions.
        #[arg(long, conflicts_with = "allow_resets")]
        account: Option<String>,
        /// Prompt sent when the wrapper resumes after switching profiles
        #[arg(long, default_value = commands::codex::DEFAULT_RECOVERY_PROMPT)]
        recovery_prompt: String,
        /// Allow a pinned launch or recovery to use a credit-billing account without
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
    /// Run a command with its Codex credentials pinned to one saved local profile,
    /// without switching the active profile. Server accounts are not supported.
    Exec {
        /// Profile alias the command runs as
        #[arg(long)]
        account: String,
        /// Command to run, followed by its arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
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

    if matches!(cli.command, Commands::Statusline) {
        codexctl::statusline::run();
        return;
    }
    // Informational commands must work in a read-only or empty home. Clap
    // exits while parsing help, version, and invalid commands, and shell
    // completion generation does not need the profile store.
    let needs_store = !matches!(&cli.command, Commands::Completions { .. });
    if needs_store && let Err(e) = config::ensure_dirs() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }

    #[cfg(feature = "central-prototype")]
    if matches!(&cli.command, Commands::Save { .. } | Commands::Switch)
        && let Err(error) = codexctl::central::native::require_local_mode()
    {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
    let result = match cli.command {
        Commands::Statusline => Ok(()),
        Commands::Rate { json, minutes } => commands::rate::run(json, minutes),
        #[cfg(feature = "central-prototype")]
        Commands::Connect {
            server,
            name,
            no_browser,
        } => codexctl::central::remote::connect(&server, name.as_deref(), no_browser),
        #[cfg(feature = "central-prototype")]
        Commands::Disconnect { forget } => codexctl::central::remote::disconnect(forget),
        #[cfg(feature = "central-prototype")]
        Commands::Migrate {
            all,
            exclusive_owner,
        } => codexctl::central::remote::migrate(all, exclusive_owner),
        #[cfg(feature = "central-prototype")]
        Commands::SessionProvider { action } => codexctl::central::native::session_provider(action),
        #[cfg(feature = "central-prototype")]
        Commands::Devices { revoke } => codexctl::central::remote::devices(revoke.as_deref()),
        #[cfg(feature = "central-prototype")]
        #[cfg(feature = "central-prototype")]
        Commands::AppAuth { action } => codexctl::central::native::run_app_auth(action),
        Commands::Loans { action } => codexctl::central::loans::client::run(action),
        Commands::Status {
            json,
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
            commands::status::run(filter, json)
        }
        Commands::Login {
            ref alias,
            ref label,
            allow_adopt,
            no_browser,
            cancel,
            local,
        } => (|| {
            #[cfg(feature = "central-prototype")]
            if codexctl::central::remote::login(
                alias,
                label.as_deref(),
                allow_adopt,
                no_browser,
                cancel,
                local,
            )? {
                return Ok(());
            }
            // Without server support every login is local.
            #[cfg(not(feature = "central-prototype"))]
            let _ = local;
            #[cfg(feature = "central-prototype")]
            codexctl::central::native::require_local_mode()?;
            if no_browser || cancel {
                anyhow::bail!("--no-browser and --cancel require an existing server account");
            }
            commands::login::run(alias, label.as_deref(), allow_adopt)
        })(),
        Commands::Save {
            ref alias,
            ref label,
            allow_adopt,
        } => commands::save::run(alias.as_deref(), label.as_deref(), allow_adopt),
        Commands::Label {
            ref alias,
            ref text,
        } => commands::label::run(alias, text.as_deref()),
        #[cfg(feature = "central-prototype")]
        Commands::Rename { ref old, ref new } => codexctl::central::remote::rename(old, new),
        #[cfg(feature = "central-prototype")]
        Commands::CentralToken {
            ref connection,
            active,
        } => match (connection, active) {
            (Some(connection), false) => codexctl::central::native::print_token(connection),
            (None, true) => codexctl::central::native::print_active_token(),
            _ => Err(anyhow::anyhow!(
                "pass exactly one of --connection <path> or --active"
            )),
        },
        Commands::Use {
            ref alias,
            history,
            json,
            allow_billing,
            allow_resets,
            restart_daemon,
        } => {
            if let Some(limit) = history {
                #[cfg(feature = "central-prototype")]
                {
                    codexctl::central::native::print_history(limit, json)
                }
                #[cfg(not(feature = "central-prototype"))]
                {
                    let _ = (limit, json);
                    Err(anyhow::anyhow!(
                        "--history requires the central provider feature"
                    ))
                }
            } else {
                commands::use_profile::run(
                    alias.as_deref(),
                    allow_billing,
                    allow_resets,
                    restart_daemon,
                )
            }
        }
        Commands::Switch => commands::switch::run(),
        Commands::Resets {
            ref redeem,
            claim,
            within_days,
            yes,
        } => {
            if let Some(alias) = redeem {
                commands::resets::run_redeem(Some(alias), yes, None)
            } else if claim {
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
        Commands::List { json } => commands::list::run(json),
        Commands::Remove { ref alias } => commands::remove::run(alias),
        Commands::Whoami => commands::whoami::run(),
        Commands::Codex {
            ref account,
            ref args,
            ref recovery_prompt,
            allow_billing,
            allow_resets,
        } => codex_command_outcome(commands::codex::run(
            account.as_deref(),
            args,
            recovery_prompt,
            allow_billing,
            allow_resets,
        ))
        .into_result(),
        Commands::Exec {
            ref account,
            ref args,
        } => codex_command_outcome(commands::exec::run(account, args)).into_result(),
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

    /// A pinned child owns everything after `--`, including words that look
    /// like codexctl's own flags.
    #[test]
    fn exec_subcommand_forwards_the_whole_child_command() {
        let cli = Cli::parse_from([
            "codexctl",
            "exec",
            "--account",
            "amir@example.com",
            "--",
            "codex",
            "--allow-billing",
        ]);

        match cli.command {
            Commands::Exec { account, args } => {
                assert_eq!(account, "amir@example.com");
                assert_eq!(
                    args,
                    vec!["codex".to_string(), "--allow-billing".to_string()]
                );
            }
            _ => panic!("expected exec command"),
        }
    }

    #[test]
    fn codex_command_outcome_preserves_child_exit_status() {
        match codex_command_outcome(Ok(130)) {
            CommandOutcome::Exit(code) => assert_eq!(code, 130),
            CommandOutcome::Continue(_) => panic!("expected process exit"),
        }
    }

    #[test]
    fn use_history_is_optional_and_read_only() {
        let cli = Cli::parse_from(["codexctl", "use", "--history", "--json"]);
        match cli.command {
            Commands::Use {
                alias,
                history,
                json,
                allow_billing,
                allow_resets,
                restart_daemon,
            } => {
                assert_eq!(alias, None);
                assert_eq!(history, Some(20));
                assert!(json);
                assert!(!allow_billing);
                assert!(!allow_resets);
                assert!(!restart_daemon);
            }
            _ => panic!("expected use command"),
        }
    }
}
