use anyhow::Result;

use crate::commands::alias;
use crate::config;
use crate::profile;

/// Set or clear a profile's display label.
///
/// The label is display text only. It never selects a profile, so it carries no
/// uniqueness rule — two accounts may both read `team` without creating a name
/// that cannot be resolved.
pub fn run(alias_arg: &str, label: Option<&str>) -> Result<()> {
    let alias = alias::required(alias_arg)?;
    let paths = config::default_paths()?;
    profile::set_label_from(&paths, alias, label)?;

    match profile::get_profile_from(&paths, alias)?.meta.label {
        Some(label) => println!("labeled '{alias}' as {label}"),
        None => println!("cleared the label on '{alias}'"),
    }
    Ok(())
}
