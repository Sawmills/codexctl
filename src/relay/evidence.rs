//! Bounded, payload-free observations and seven-day model/hour reports.
use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::mpsc;

const DAILY_BYTES: u64 = 4 * 1024 * 1024;
const REQUEST_BYTES: u64 = 3 * 1024 * 1024;
const RETAIN_DAYS: i64 = 7;
const DROP_REASONS: [&str; 3] = ["queue_full", "io", "daily_limit"];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    utc_hour: String,
    status: u16,
    /// The HTTP policy decision; absent for streams and older records.
    #[serde(default)]
    rate_429: Option<bool>,
    error_code: Option<String>,
    retry_headers: BTreeMap<String, String>,
    model: String,
    account_alias: Option<String>,
    account_class: String,
    request_id: Option<String>,
    observation: String,
}

#[derive(Clone)]
pub(super) struct Evidence {
    sender: Option<mpsc::Sender<Record>>,
    dropped: Arc<[AtomicU64; 3]>,
    maintenance_failed: Arc<AtomicU64>,
}

impl Evidence {
    pub(super) fn start(
        directory: Option<PathBuf>,
    ) -> Result<(Self, Option<tokio::task::JoinHandle<()>>)> {
        let dropped = Arc::new([const { AtomicU64::new(0) }; 3]);
        let maintenance_failed = Arc::new(AtomicU64::new(0));
        let Some(directory) = directory else {
            return Ok((
                Self {
                    sender: None,
                    dropped,
                    maintenance_failed,
                },
                None,
            ));
        };
        crate::store::ensure_private_dir(&directory)?;
        let (sender, mut receiver) = mpsc::channel::<Record>(256);
        let errors = dropped.clone();
        let maintenance_errors = maintenance_failed.clone();
        let worker = tokio::spawn(async move {
            let mut maintenance = tokio::time::interval(std::time::Duration::from_secs(60));
            maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let record = tokio::select! {
                    // The first tick is immediate; later ticks prune even when
                    // no response arrives. The worker owns both write paths.
                    _ = maintenance.tick() => {
                        let directory = directory.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            let _lock = writer_lock(&directory)?;
                            prune(&directory, Utc::now().date_naive())
                        }).await;
                        if !matches!(result, Ok(Ok(()))) {
                            maintenance_errors.fetch_add(1, Ordering::Relaxed);
                            eprintln!("relay evidence maintenance failed reason=io");
                        }
                        continue;
                    }
                    record = receiver.recv() => record,
                };
                let Some(record) = record else { break };
                let directory = directory.clone();
                let result = tokio::task::spawn_blocking(move || append(&directory, &record)).await;
                let reason = match result {
                    Ok(Ok(true)) => continue,
                    Ok(Ok(false)) => 2,
                    _ => 1,
                };
                errors[reason].fetch_add(1, Ordering::Relaxed);
                eprintln!("relay evidence dropped reason={}", DROP_REASONS[reason]);
            }
        });
        Ok((
            Self {
                sender: Some(sender),
                dropped,
                maintenance_failed,
            },
            Some(worker),
        ))
    }

    /// Reconstruct only the allowlist. The caller's diagnostics may have more
    /// detail, but messages, bodies, thread ids and credentials never persist.
    pub(super) fn observe(&self, line: &Value) {
        let Some(sender) = &self.sender else {
            return;
        };
        let error_code = line
            .pointer("/failure/response/error/code")
            .or_else(|| line.pointer("/failure/error/code"))
            .or_else(|| line.pointer("/failure/error/type"))
            .and_then(Value::as_str)
            .map(error_class)
            .map(str::to_owned);
        let retry_headers = line
            .get("retry_headers")
            .and_then(Value::as_object)
            .map(|headers| {
                headers
                    .iter()
                    .filter_map(|(name, value)| {
                        retry_header(name, value.as_str()?).map(|value| (name.clone(), value))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let record = Record {
            utc_hour: Utc::now().format("%Y-%m-%dT%H:00:00Z").to_string(),
            rate_429: line.get("rate_429").and_then(Value::as_bool),
            status: line
                .get("http_status")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(200),
            error_code,
            retry_headers,
            model: model(line.get("model").and_then(Value::as_str).unwrap_or("other")).into(),
            account_alias: line
                .get("account_alias")
                .and_then(Value::as_str)
                .and_then(alias),
            account_class: match line.get("account_class").and_then(Value::as_str) {
                Some("included") => "included",
                Some("credit") => "credit",
                _ => "unknown",
            }
            .into(),
            request_id: line
                .get("request_id")
                .and_then(Value::as_str)
                .and_then(request_id),
            observation: if line.get("outcome").and_then(Value::as_str) == Some("headers") {
                "headers"
            } else {
                "failure"
            }
            .into(),
        };
        if sender.try_send(record).is_err() {
            self.dropped[0].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn metrics(&self) -> String {
        let mut text = String::from("# TYPE codexctl_relay_evidence_dropped_total counter\n");
        for (reason, count) in DROP_REASONS.into_iter().zip(self.dropped.iter()) {
            text.push_str(&format!(
                "codexctl_relay_evidence_dropped_total{{reason=\"{reason}\"}} {}\n",
                count.load(Ordering::Relaxed)
            ));
        }
        text.push_str(&format!(
            "# TYPE codexctl_relay_evidence_maintenance_failures_total counter\ncodexctl_relay_evidence_maintenance_failures_total {}\n",
            self.maintenance_failed.load(Ordering::Relaxed)
        ));
        text
    }
}

pub(super) fn retry_header(name: &str, value: &str) -> Option<String> {
    match name {
        "retry-after" | "retry-after-ms" if value.len() <= 20 && value.parse::<u64>().is_ok() => {
            Some(value.into())
        }
        "retry-after"
            if value.len() <= 40 && chrono::DateTime::parse_from_rfc2822(value).is_ok() =>
        {
            Some(value.into())
        }
        "x-should-retry" if matches!(value, "true" | "false") => Some(value.into()),
        _ => None,
    }
}

pub(super) fn alias(value: &str) -> Option<String> {
    crate::store::validate_alias(value)
        .ok()
        .filter(|value| {
            !value.starts_with("sk-") && !value.starts_with("rt-") && !value.starts_with("eyJ")
        })
        .map(str::to_owned)
}

fn request_id(value: &str) -> Option<String> {
    let uuid = value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    let named =
        (value.starts_with("req-") || value.starts_with("req_") || value.starts_with("request-"))
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    (uuid || named).then(|| value.to_owned())
}

fn model(value: &str) -> &str {
    if crate::RELAY_KNOWN_MODELS.contains(&value) {
        value
    } else {
        "other"
    }
}

fn error_class(value: &str) -> &'static str {
    match value {
        "server_is_overloaded" => "server_is_overloaded",
        "rate_limit_exceeded" => "rate_limit_exceeded",
        "slow_down" => "slow_down",
        "usage_limit_reached" => "usage_limit_reached",
        "usage_not_included" => "usage_not_included",
        "insufficient_quota" => "insufficient_quota",
        "credit_balance_exhausted" => "credit_balance_exhausted",
        "organization_spend_limit_exceeded" => "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded" => "project_spend_limit_exceeded",
        "organization_usage_limit_exceeded" => "organization_usage_limit_exceeded",
        "flex_unavailable" => "flex_unavailable",
        _ => "other",
    }
}

fn day(path: &Path) -> Option<NaiveDate> {
    if path.extension()?.to_str()? != "jsonl" {
        return None;
    }
    NaiveDate::parse_from_str(path.file_stem()?.to_str()?, "%Y-%m-%d").ok()
}

fn writer_lock(directory: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(".write.lock"))?;
    if !lock.metadata()?.is_file() {
        bail!("relay evidence lock must be a regular file");
    }
    lock.try_lock().context("relay evidence is busy")?;
    Ok(lock)
}

fn prune(directory: &Path, today: NaiveDate) -> Result<()> {
    let earliest = today - chrono::Duration::days(RETAIN_DAYS - 1);
    let mut removed = false;
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if day(&path).is_some_and(|day| day < earliest) {
            std::fs::remove_file(path)?;
            removed = true;
        }
    }
    if removed {
        std::fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn append(directory: &Path, record: &Record) -> Result<bool> {
    use std::os::unix::fs::OpenOptionsExt;
    let _lock = writer_lock(directory)?;
    let today = Utc::now().date_naive();
    prune(directory, today)?;
    let path = directory.join(format!("{today}.jsonl"));
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        bail!("relay evidence must be a regular file");
    }
    let length = file.metadata()?.len();
    let mut last = *b"\n";
    if length != 0 {
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
    }
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    if last[0] != b'\n' {
        // Separate a crash-left fragment so the next observation survives.
        bytes.insert(0, b'\n');
    }
    let limit = if record.observation == "headers" {
        REQUEST_BYTES
    } else {
        DAILY_BYTES
    };
    if length.saturating_add(bytes.len() as u64) > limit {
        return Ok(false);
    }
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(true)
}

#[derive(Default, Serialize)]
struct Counts {
    utc_hour: String,
    model: String,
    requests: u64,
    overloaded: u64,
    rate_429: u64,
    other_failures: u64,
}

fn report(directory: &Path) -> Result<Vec<Counts>> {
    use std::os::unix::fs::OpenOptionsExt;
    if !directory.try_exists()? {
        return Ok(Vec::new());
    }
    let today = Utc::now().date_naive();
    let earliest = today - chrono::Duration::days(RETAIN_DAYS - 1);
    let mut counts: BTreeMap<(String, String), Counts> = BTreeMap::new();
    let mut skipped = 0_u64;
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if !day(&path).is_some_and(|day| (earliest..=today).contains(&day)) {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        if !file.metadata()?.is_file() {
            bail!("relay evidence must be a regular file");
        }
        if file.metadata()?.len() > DAILY_BYTES {
            bail!("relay evidence exceeds its daily bound");
        }
        let mut contents = Vec::new();
        file.take(DAILY_BYTES + 1).read_to_end(&mut contents)?;
        if contents.len() as u64 > DAILY_BYTES {
            bail!("relay evidence exceeds its daily bound");
        }
        for line in contents
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        {
            // A failed append can leave a fragment joined to the next record.
            // Keep valid observations and disclose loss without printing data.
            let Ok(record) = serde_json::from_slice::<Record>(line) else {
                skipped += 1;
                continue;
            };
            let Ok(hour) = chrono::DateTime::parse_from_rfc3339(&record.utc_hour) else {
                skipped += 1;
                continue;
            };
            let hour = hour
                .with_timezone(&Utc)
                .format("%Y-%m-%dT%H:00:00Z")
                .to_string();
            let label = model(&record.model).to_owned();
            let row = counts
                .entry((hour.clone(), label.clone()))
                .or_insert_with(|| Counts {
                    utc_hour: hour,
                    model: label,
                    ..Counts::default()
                });
            if record.observation == "headers" {
                row.requests += 1;
            } else {
                match record.rate_429 {
                    Some(true) => row.rate_429 += 1,
                    Some(false) => row.other_failures += 1,
                    None => match record.error_code.as_deref() {
                        Some("server_is_overloaded") => row.overloaded += 1,
                        Some("rate_limit_exceeded" | "slow_down") => row.rate_429 += 1,
                        _ => row.other_failures += 1,
                    },
                }
            }
        }
    }
    if skipped != 0 {
        eprintln!(
            "relay evidence: skipped {skipped} invalid record line(s); counts exclude damaged lines"
        );
    }
    Ok(counts.into_values().collect())
}

pub(super) fn render(directory: &Path, json: bool) -> Result<String> {
    let rows = report(directory)?;
    if json {
        return Ok(serde_json::to_string_pretty(&rows)?);
    }
    let mut text = String::from(
        "UTC hour              Model            Requests Overloads Rate429 Other failures\n",
    );
    for row in rows {
        text.push_str(&format!(
            "{:<21} {:<16} {:>8} {:>9} {:>7} {:>14}\n",
            row.utc_hour, row.model, row.requests, row.overloaded, row.rate_429, row.other_failures
        ));
    }
    Ok(text)
}
