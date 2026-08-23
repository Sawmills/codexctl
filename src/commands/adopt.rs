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

/// Ask before replacing `alias` with an account that cannot be matched to it.
///
/// `stored` is what the profile is understood to hold, when it declares
/// anything at all.
pub fn approve_adoption(
    alias: &str,
    stored: Option<&str>,
    arriving: Option<&str>,
    assume_yes: bool,
    out: &mut impl std::io::Write,
) -> bool {
    use std::io::IsTerminal;

    if assume_yes {
        let _ = writeln!(out, "codexctl: replacing the profile saved as {alias}");
        return true;
    }
    if !std::io::stdin().is_terminal() {
        let _ = writeln!(
            out,
            "codexctl: not replacing the profile saved as {alias} \
             (no terminal to approve; pass --allow-adopt to allow)"
        );
        return false;
    }

    let held = match stored {
        Some(stored) => format!("holds {}", profile::short_workspace(stored)),
        None => "does not record which account it holds".to_string(),
    };
    let arriving = arriving
        .map(|arriving| format!(" this login is {}.", profile::short_workspace(arriving)))
        .unwrap_or_default();
    let _ = write!(
        out,
        "codexctl: profile '{alias}' {held}, so this login cannot be matched to it.{arriving} \
         Replace it? Its saved credentials are overwritten. [y/N] "
    );
    let _ = out.flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The error for an adoption nobody approved.
///
/// It says only what is known — that the profile could not be matched — rather
/// than claiming a different account, which is exactly what the store could not
/// establish.
pub fn refusal(alias: &str, stored: Option<&str>) -> anyhow::Error {
    let held = match stored {
        Some(stored) => format!(
            "holds {} but cannot be matched to this account",
            profile::short_workspace(stored)
        ),
        None => "does not record which account it holds".to_string(),
    };
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
    #[test]
    fn the_flag_answers_without_a_terminal() {
        let mut out = Vec::new();
        assert!(approve_adoption(
            "work",
            None,
            Some("acct-team"),
            true,
            &mut out
        ));
        assert!(String::from_utf8(out).unwrap().contains("work"));
    }

    /// Tests have no terminal, which is exactly the condition being checked:
    /// with no way to ask, the answer is no, and the message says what supplies
    /// one instead.
    #[test]
    fn no_terminal_declines_and_names_the_flag() {
        let mut out = Vec::new();
        assert!(!approve_adoption(
            "work",
            Some("acct-team"),
            None,
            false,
            &mut out
        ));
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("--allow-adopt"), "{printed}");
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
    }
}
