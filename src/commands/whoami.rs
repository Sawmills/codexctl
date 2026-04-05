use anyhow::Result;

use crate::profile;

pub fn run() -> Result<()> {
    let active = profile::get_active()?;
    match active {
        Some(alias) => {
            let p = profile::get_profile(&alias)?;
            let email = p.meta.email.as_deref().unwrap_or("-");
            let plan = p.meta.plan.as_deref().unwrap_or("-");
            println!("{} ({}) [{}]", alias, email, plan);
        }
        None => {
            println!("no active profile. Use 'codexctl save' or 'codexctl use <alias>'.");
        }
    }
    Ok(())
}
