use anyhow::Result;

use crate::commands::alias;
use crate::profile;

pub fn run(alias: &str) -> Result<()> {
    let alias = alias::required(alias)?;

    profile::delete_profile(alias)?;

    // Clear active if this was the active profile
    if let Some(active) = profile::get_active()?
        && active == alias
    {
        let active_file = crate::config::active_file()?;
        std::fs::remove_file(&active_file).ok();
    }

    println!("removed profile '{}'", alias);
    Ok(())
}
