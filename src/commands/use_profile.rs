use anyhow::Result;

use crate::profile;

pub fn run(alias: &str) -> Result<()> {
    let email = profile::switch_to(alias)?;
    println!("switched to {} ({})", alias, email);
    Ok(())
}
