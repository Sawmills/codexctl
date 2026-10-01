use anyhow::Result;

use crate::commands::daemon_sync;
use crate::config;
use crate::profile;

pub fn run() -> Result<()> {
    #[cfg(feature = "central-prototype")]
    if codexctl::central::remote::whoami()? {
        return Ok(());
    }
    let active = profile::get_active()?;
    match active {
        Some(alias) => {
            let p = profile::get_profile(&alias)?;
            let email = p.meta.email.as_deref().unwrap_or("-");
            let plan = p.meta.plan.as_deref().unwrap_or("-");
            let label = p
                .meta
                .label
                .as_deref()
                .map(|label| format!(" — {label}"))
                .unwrap_or_default();
            println!("{alias}{label} ({email}) [{plan}]");
            daemon_sync::warn_if_stale(&alias, &config::codex_auth_json()?);
        }
        None => {
            println!("no active profile. Use 'codexctl save' or 'codexctl use <alias>'.");
        }
    }
    Ok(())
}
