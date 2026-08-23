//! Operator consent for replacing a profile the store cannot identify.
//!
//! Two claims that positively disagree are never the same account, and no
//! answer changes that. But a profile that declares *nothing* is a different
//! situation: the store cannot tell whether this is the same account returning
//! or a stranger arriving, while the operator who just logged in usually can.
//!
//! Refusing that case outright reads as safety and is not. The only remedy left
//! is `codexctl remove` followed by a fresh login — which destroys the very
//! metadata that could have identified the profile, and then performs the same
//! replacement with nothing checked at all. This module asks the question
//! instead, following the consent idiom `use` already applies to billing and to
//! banked resets: prompt on a terminal, refuse without one unless a flag says
//! the operator already decided.

use crate::profile;

/// How the adoption question came out.
///
/// Declining and being unable to ask are both "do not replace it", but they are
/// not the same outcome: one is a decision, the other is a command that could
/// not run. Collapsing them lets an unattended `save` report success having
/// saved nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum Approval {
    Granted,
    Declined,
    NoTerminal,
}

/// Ask before replacing `alias` with an account that cannot be matched to it.
///
/// `stored` is what the profile is understood to hold, when it declares
/// anything at all.
///
/// `interactive` and `input` are supplied rather than read from the process, so
/// that a test can exercise the prompt without depending on whether the test
/// runner happened to be started from a terminal — which decides, for a global
/// `stdin().is_terminal()`, between passing and blocking the whole suite.
pub fn approve_adoption(
    alias: &str,
    stored: Option<&str>,
    arriving: Option<&str>,
    assume_yes: bool,
    interactive: bool,
    input: &mut impl std::io::BufRead,
    out: &mut impl std::io::Write,
) -> Approval {
    if assume_yes {
        let _ = writeln!(out, "codexctl: replacing the profile saved as {alias}");
        return Approval::Granted;
    }
    if !interactive {
        return Approval::NoTerminal;
    }

    let held = describes(stored);
    // `arriving` is the incoming workspace, not a login. Calling it one names
    // the wrong half of the identity in the one message that has to be exact.
    let arriving = arriving
        .map(|arriving| {
            format!(
                " That account is in workspace {}.",
                profile::short_workspace(arriving)
            )
        })
        .unwrap_or_default();
    let _ = write!(
        out,
        "codexctl: profile '{alias}' {held} cannot be matched to the account signing in.{arriving} \
         Replace it? Its saved credentials are overwritten. [y/N] "
    );
    let _ = out.flush();

    let mut answer = String::new();
    if input.read_line(&mut answer).is_err() {
        return Approval::Declined;
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Approval::Granted,
        _ => Approval::Declined,
    }
}

/// Exactly what a profile directory currently stores, if anything readable.
///
/// The raw bytes rather than one parsed field: a refresh can rotate the refresh
/// token while the access token stays put, and an approval given for the old
/// credential must not be honoured against the new one. Two distinct tokens can
/// also describe themselves identically — same login, no workspace — so
/// comparing descriptions is not enough to tell them apart.
pub fn stored_credentials(dir: &std::path::Path) -> Option<Vec<u8>> {
    std::fs::read(dir.join("auth.json")).ok()
}

/// What a profile is known to hold, phrased for an operator.
///
/// A login identifier is not a workspace, so it must not be rendered as one:
/// `short_workspace` would print a namespaced login as `sub:seat…`, which reads
/// like a truncated account id and names the wrong thing entirely.
fn describes(stored: Option<&str>) -> String {
    match stored {
        Some(stored) if stored.starts_with("uid:") || stored.starts_with("sub:") => {
            "records a login but no workspace, so it".to_string()
        }
        Some(stored) => format!("holds {}, which", profile::describe_claim(stored)),
        None => "does not record which account it holds, so it".to_string(),
    }
}

/// The error for an adoption nobody approved.
///
/// It says only what is known — that the profile could not be matched — rather
/// than claiming a different account, which is exactly what the store could not
/// establish.
pub fn refusal(alias: &str, stored: Option<&str>) -> anyhow::Error {
    let held = format!("{} cannot be matched to this account", describes(stored));
    anyhow::anyhow!(
        "profile '{alias}' {held}, so replacing it needs approval and none was given. \
         Re-run on a terminal to confirm, pass --allow-adopt, or choose another alias."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag is the operator's answer given ahead of time, so it needs no
    /// terminal — that is the whole point of having one.
    /// Drive the prompt with a fixed answer and no terminal question begged.
    fn ask(assume_yes: bool, interactive: bool, answer: &str) -> (Approval, String) {
        let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
        let mut out = Vec::new();
        let approval = approve_adoption(
            "work",
            None,
            Some("acct-team"),
            assume_yes,
            interactive,
            &mut input,
            &mut out,
        );
        (approval, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_flag_answers_without_a_terminal() {
        let (approval, printed) = ask(true, false, "");
        assert_eq!(approval, Approval::Granted);
        assert!(printed.contains("work"), "{printed}");
    }

    /// Only an explicit yes is a yes. Anything else, empty input included,
    /// leaves the profile alone.
    #[test]
    fn only_an_explicit_yes_approves() {
        for answer in ["y\n", "Y\n", "yes\n"] {
            assert_eq!(ask(false, true, answer).0, Approval::Granted, "{answer:?}");
        }
        for answer in ["n\n", "\n", "", "no\n", "sure\n"] {
            assert_eq!(ask(false, true, answer).0, Approval::Declined, "{answer:?}");
        }
    }

    /// Tests have no terminal, which is exactly the condition being checked.
    /// It must report *why* it could not ask, so the caller can fail rather than
    /// treat it as the operator saying no.
    #[test]
    fn no_terminal_is_distinct_from_a_decline() {
        assert_eq!(ask(false, false, "").0, Approval::NoTerminal);
        assert_eq!(ask(false, true, "n\n").0, Approval::Declined);
    }

    /// The refusal reports only what is known. Claiming a different account
    /// would state the very thing the store could not establish.
    #[test]
    fn the_refusal_does_not_claim_a_different_account() {
        let message = refusal("work", None).to_string();
        assert!(
            message.contains("does not record which account"),
            "{message}"
        );
        assert!(!message.contains("different account"), "{message}");
        assert!(
            refusal("work", Some("acct-team"))
                .to_string()
                .contains("cannot be matched"),
        );
        // A login identifier must not be dressed up as a workspace.
        let login_only = refusal("work", Some("sub:seatA")).to_string();
        assert!(
            login_only.contains("records a login but no workspace"),
            "{login_only}"
        );
        assert!(!login_only.contains("sub:seat"), "{login_only}");
    }
}
