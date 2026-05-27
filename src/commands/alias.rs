use anyhow::{Result, bail};

pub(super) fn optional(alias: Option<&str>) -> Option<&str> {
    alias.map(str::trim).filter(|alias| !alias.is_empty())
}

pub(super) fn required(alias: &str) -> Result<&str> {
    let alias = alias.trim();
    if alias.is_empty() {
        bail!("profile alias cannot be empty");
    }
    Ok(alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_trims_alias() {
        assert_eq!(
            optional(Some("  amir+8@sawmills.ai  ")),
            Some("amir+8@sawmills.ai")
        );
    }

    #[test]
    fn optional_drops_blank_alias() {
        assert_eq!(optional(Some("   ")), None);
        assert_eq!(optional(None), None);
    }

    #[test]
    fn required_trims_alias() {
        assert_eq!(
            required("  amir+8@sawmills.ai  ").unwrap(),
            "amir+8@sawmills.ai"
        );
    }

    #[test]
    fn required_rejects_blank_alias() {
        assert!(required("   ").is_err());
    }
}
