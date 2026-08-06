use anyhow::Result;
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};

use crate::profile;

pub fn run() -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;
    // Carry the label column only when something fills it, so a store with no
    // labels renders exactly as it did before the column existed.
    let show_labels = profiles.iter().any(|p| p.meta.label.is_some());

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(headers(show_labels));
    for p in &profiles {
        let is_active = active.as_deref() == Some(&p.meta.alias);
        table.add_row(row(p, show_labels, is_active));
    }
    println!("{table}");
    Ok(())
}

fn headers(show_labels: bool) -> Vec<&'static str> {
    let mut headers = vec!["Account"];
    if show_labels {
        headers.push("Label");
    }
    headers.extend(["Plan", "Email", "Active"]);
    headers
}

fn row(p: &profile::Profile, show_labels: bool, is_active: bool) -> Vec<Cell> {
    let mut row = vec![Cell::new(&p.meta.alias)];
    if show_labels {
        row.push(label_cell(p.meta.label.as_deref()));
    }
    row.push(Cell::new(p.meta.plan.as_deref().unwrap_or("-")));
    row.push(Cell::new(p.meta.email.as_deref().unwrap_or("-")));
    row.push(active_cell(is_active));
    row
}

/// Cyan marks the two cells that answer "which account is this" — the label the
/// operator chose, and which row is live. Everything else stays uncolored so
/// color keeps meaning something.
fn label_cell(label: Option<&str>) -> Cell {
    match label {
        Some(label) => Cell::new(label).fg(Color::Cyan),
        None => Cell::new("-"),
    }
}

fn active_cell(is_active: bool) -> Cell {
    if is_active {
        Cell::new("*").fg(Color::Cyan)
    } else {
        Cell::new("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_gain_a_label_column_only_when_labels_exist() {
        assert_eq!(headers(false), vec!["Account", "Plan", "Email", "Active"]);
        assert_eq!(
            headers(true),
            vec!["Account", "Label", "Plan", "Email", "Active"]
        );
    }

    fn profile_with(alias: &str, label: Option<&str>) -> profile::Profile {
        profile::Profile {
            meta: profile::Meta {
                alias: alias.to_string(),
                label: label.map(str::to_string),
                email: Some("amir@sawmills.ai".to_string()),
                plan: Some("pro".to_string()),
                saved_at: "2026-01-01T00:00:00Z".to_string(),
                ..profile::Meta::default()
            },
            dir: std::path::PathBuf::from("/tmp").join(alias),
        }
    }

    /// Every row must have the same width as the header or the table breaks.
    #[test]
    fn rows_match_the_header_width_in_both_shapes() {
        for show_labels in [false, true] {
            let labeled = profile_with("amir-team", Some("team"));
            let bare = profile_with("amir@sawmills.ai", None);
            assert_eq!(
                row(&labeled, show_labels, true).len(),
                headers(show_labels).len()
            );
            assert_eq!(
                row(&bare, show_labels, false).len(),
                headers(show_labels).len()
            );
        }
    }

    #[test]
    fn an_unlabeled_profile_renders_a_placeholder_in_a_labeled_table() {
        let bare = profile_with("amir@sawmills.ai", None);

        assert_eq!(row(&bare, true, false)[1].content(), "-");
    }
}
