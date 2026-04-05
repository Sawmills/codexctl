use anyhow::Result;

use crate::profile;

pub fn run() -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;

    for p in &profiles {
        let marker = if active.as_deref() == Some(&p.meta.alias) {
            " *"
        } else {
            ""
        };
        let email = p.meta.email.as_deref().unwrap_or("-");
        println!("  {}{} ({})", p.meta.alias, marker, email);
    }
    Ok(())
}
