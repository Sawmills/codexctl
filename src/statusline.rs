//! Credential-free usage cache for prompt renderers. The read path never contacts a service.
use crate::{api, config, profile, store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::Path,
    time::Duration,
};

const TTL_SECONDS: i64 = 120;
const READ_BUDGET: Duration = Duration::from_millis(150);
const MAX_BYTES: u64 = 65_536;

#[derive(Clone, Serialize, Deserialize)]
pub struct Usage {
    pub weekly_used_percent: Option<f64>,
    pub weekly_resets_at: Option<i64>,
    pub five_hour_used_percent: Option<f64>,
    #[serde(default)]
    pub five_hour_resets_at: Option<i64>,
}
impl Usage {
    pub(crate) fn from_usage(usage: &api::RateLimitResponse) -> Self {
        let window = |seconds| {
            usage
                .rate_limit
                .as_ref()?
                .windows()
                .map(|(_, w)| w)
                .find(|w| w.duration_seconds() == Some(seconds))
        };
        let weekly = window(604_800);
        let short = window(18_000);
        Self {
            weekly_used_percent: weekly.map(|w| w.used_percent),
            weekly_resets_at: weekly.and_then(api::RateLimitWindow::reset_timestamp),
            five_hour_used_percent: short.map(|w| w.used_percent),
            five_hour_resets_at: short.and_then(api::RateLimitWindow::reset_timestamp),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    Local {
        account_id: Option<String>,
        user_id: Option<String>,
        saved_at: String,
    },
    Server {
        server: String,
        user_id: Option<String>,
        account_id: String,
    },
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Selection {
    pub alias: String,
    pub source: Source,
}
impl Selection {
    pub(crate) fn local(meta: &profile::Meta) -> Self {
        Self {
            alias: meta.alias.clone(),
            source: Source::Local {
                account_id: meta.account_id.clone(),
                user_id: meta.user_id.clone(),
                saved_at: meta.saved_at.clone(),
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    sampled_at: i64,
    selection: Selection,
    account: Option<crate::status_json::AccountStatus>,
    five_hour_resets_at: Option<i64>,
}

fn read(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        bail!("not a bounded regular file");
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("file grew beyond limit");
    }
    Ok(bytes)
}

fn active(paths: &config::Paths) -> Result<Selection> {
    #[cfg(feature = "central-prototype")]
    if let Some(selection) = crate::central::native::statusline_selection(paths)? {
        return Ok(selection);
    }
    let marker = read(&paths.active_file())?;
    let alias = store::validate_alias(std::str::from_utf8(&marker)?)?;
    let directory = store::profile_dir(paths, alias)?;
    if directory.join(".central-transfer.json").try_exists()? {
        bail!("profile is migrated");
    }
    let meta: profile::Meta = serde_json::from_slice(&read(&directory.join("meta.json"))?)?;
    if meta.alias != alias {
        bail!("profile changed");
    }
    Ok(Selection::local(&meta))
}

/// Cache failure must not fail status or token delivery. Never store credentials here.
pub(crate) fn record(
    paths: &config::Paths,
    selection: Selection,
    label: Option<&str>,
    usage: Option<Usage>,
) {
    let _ = (|| -> Result<()> {
        if active(paths)? != selection {
            return Ok(());
        }
        let destination = paths.codexctl_dir().join("statusline.json");
        let five_hour_resets_at = usage.as_ref().and_then(|u| u.five_hour_resets_at);
        let account = usage.map(|usage| crate::status_json::AccountStatus {
            alias: selection.alias.clone(),
            label: label.map(str::to_owned),
            plan: None,
            source: match selection.source {
                Source::Local { .. } => crate::status_json::Source::Local,
                Source::Server { .. } => crate::status_json::Source::Server,
            },
            state: crate::status_json::State::Active,
            primary_used_percent: usage.five_hour_used_percent,
            secondary_used_percent: usage.weekly_used_percent,
            resets_at: crate::status_json::timestamp(usage.weekly_resets_at),
            billing_class: api::BillingClass::Unknown,
            error: None,
        });
        let cache = Cache {
            version: 1,
            sampled_at: chrono::Utc::now().timestamp(),
            selection,
            account,
            five_hour_resets_at,
        };
        store::atomic_write(&destination, &serde_json::to_vec(&cache)?)
    })();
}

/// Reuse the usage fetched for the normal local status command.
pub fn record_local(
    paths: &config::Paths,
    meta: &profile::Meta,
    usage: Option<&api::RateLimitResponse>,
) {
    record(
        paths,
        Selection::local(meta),
        meta.label.as_deref(),
        usage.map(Usage::from_usage),
    );
}

fn short_name(alias: &str, label: Option<&str>) -> String {
    let local = alias.split('@').next().unwrap_or(alias);
    let alias = local
        .rsplit('+')
        .next()
        .filter(|v| !v.is_empty())
        .unwrap_or(local);
    let clean = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
            .take(20)
            .collect::<String>()
            .trim()
            .to_owned()
    };
    label
        .map(clean)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| clean(alias))
}
fn remaining(used: f64) -> Option<u8> {
    (used.is_finite() && (0.0..=100.0).contains(&used)).then(|| (100.0 - used).floor() as u8)
}
fn duration(seconds: i64) -> String {
    if seconds >= 86400 {
        format!("{}d{}h", seconds / 86400, seconds % 86400 / 3600)
    } else if seconds >= 3600 {
        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
    } else {
        format!("{}m", seconds / 60)
    }
}
fn render(paths: &config::Paths) -> Result<String> {
    let cache: Cache =
        serde_json::from_slice(&read(&paths.codexctl_dir().join("statusline.json"))?)?;
    let now = chrono::Utc::now().timestamp();
    if cache.version != 1
        || !(0..=TTL_SECONDS).contains(&now.saturating_sub(cache.sampled_at))
        || active(paths)? != cache.selection
    {
        bail!("cache is not current");
    }
    let account = cache.account.context("no usage")?;
    if account.error.is_some() || account.alias != cache.selection.alias {
        bail!("invalid status");
    }
    let weekly = remaining(account.secondary_used_percent.context("no weekly window")?)
        .context("invalid usage")?;
    let reset = chrono::DateTime::parse_from_rfc3339(
        account.resets_at.as_deref().context("no reset time")?,
    )?
    .timestamp();
    if reset <= now {
        bail!("window ended");
    }
    let name = short_name(&cache.selection.alias, account.label.as_deref());
    if name.is_empty() {
        bail!("no display name");
    }
    let mut line = format!(
        "{name} {weekly}% wk · {}",
        duration(reset.saturating_sub(now))
    );
    if let Some(short) = account.primary_used_percent.and_then(remaining)
        && cache.five_hour_resets_at.is_none_or(|reset| reset > now)
    {
        line.push_str(&format!(" · {short}% 5h"));
    }
    Ok(line)
}

/// No store initialization, locks, refresh, or diagnostics on the prompt path.
/// The process exits without joining the reader if a filesystem call stalls.
pub fn run() {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::Builder::new()
        .name("statusline".into())
        .spawn(move || {
            let result = config::default_paths().and_then(|paths| render(&paths));
            let _ = tx.send(result);
        });
    if reader.is_ok()
        && let Ok(Ok(line)) = rx.recv_timeout(READ_BUDGET)
    {
        let _ = writeln!(std::io::stdout().lock(), "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, config::Paths, profile::Meta) {
        let home = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_home(home.path().into());
        let meta = profile::Meta {
            alias: "work".into(),
            label: Some("team".into()),
            saved_at: "today".into(),
            ..Default::default()
        };
        store::atomic_write(&paths.active_file(), b"work").unwrap();
        store::atomic_write(
            &paths.profiles_dir().join("work/meta.json"),
            &serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        (home, paths, meta)
    }
    fn usage(seconds: u64) -> api::RateLimitResponse {
        serde_json::from_value(json!({"rate_limit":{"primary_window":{"used_percent":38,"limit_window_seconds":seconds,"resets_at":chrono::Utc::now().timestamp()+600000}}})).unwrap()
    }
    #[test]
    fn statusline_local_usage_writer_feeds_renderer_without_credentials() {
        let (_home, paths, meta) = setup();
        let data = usage(604800);

        record_local(&paths, &meta, Some(&data));

        assert_eq!(render(&paths).unwrap(), "team 62% wk · 6d22h");
        assert!(!paths.codex_auth_json().exists());
    }
    #[test]
    fn statusline_usage_failure_invalidates_previous_sample() {
        let (_home, paths, meta) = setup();
        record_local(&paths, &meta, Some(&usage(604800)));

        record_local(&paths, &meta, None);

        assert!(render(&paths).is_err());
    }
    #[test]
    fn statusline_nonweekly_window_does_not_invent_weekly_usage() {
        let (_home, paths, meta) = setup();

        record_local(&paths, &meta, Some(&usage(3600)));

        assert!(render(&paths).is_err());
    }
    #[test]
    fn statusline_cache_write_failure_does_not_fail_usage_fetch() {
        let (_home, paths, meta) = setup();
        std::fs::create_dir(paths.codexctl_dir().join("statusline.json")).unwrap();

        record_local(&paths, &meta, Some(&usage(604800)));

        assert!(render(&paths).is_err());
    }
}
