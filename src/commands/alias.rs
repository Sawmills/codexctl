use anyhow::Result;

use crate::store;

pub(super) fn optional(alias: Option<&str>) -> Result<Option<&str>> {
    alias.map(store::validate_alias).transpose()
}

pub(super) fn required(alias: &str) -> Result<&str> {
    store::validate_alias(alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_trims_alias() {
        assert_eq!(
            optional(Some("  amir+8@sawmills.ai  ")).unwrap(),
            Some("amir+8@sawmills.ai")
        );
    }

    #[test]
    fn optional_rejects_blank_alias() {
        assert!(optional(Some("   ")).is_err());
        assert_eq!(optional(None).unwrap(), None);
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

    #[test]
    fn aliases_reject_path_traversal() {
        for alias in ["../escape", "/tmp/escape", "a/b", "a\\b"] {
            assert!(required(alias).is_err(), "accepted {alias:?}");
        }
    }
}
