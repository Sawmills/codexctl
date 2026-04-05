use anyhow::Result;

use crate::profile;

pub fn run() -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;

    let max_len = profiles
        .iter()
        .map(|p| p.meta.alias.len())
        .max()
        .unwrap_or(0);

    for p in &profiles {
        let marker = if active.as_deref() == Some(&p.meta.alias) {
            "  *"
        } else {
            ""
        };
        let plan = p.meta.plan.as_deref().unwrap_or("-");
        println!(
            "  {:<width$}  [{}]{}",
            p.meta.alias,
            plan,
            marker,
            width = max_len
        );
    }
    Ok(())
}
