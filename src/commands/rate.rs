//! Read-only log aggregation. Account usage comes from the existing status command,
//! keeping its local/server account selection and credential handling in one place.
use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use codexctl::status_json::AccountStatus;
use comfy_table::{Table, presets::UTF8_FULL_CONDENSED};
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Default, Serialize)]
struct AccountRate {
    account: String,
    weekly_used_percent: Option<f64>,
    responses_ok: u64,
    responses_429: u64,
    rate_429: f64,
    processes: usize,
    pids: BTreeSet<u32>,
    #[serde(skip)]
    process_ids: BTreeSet<String>,
}

#[derive(Serialize)]
struct Report {
    host: String,
    window_minutes: u32,
    generated_at: String,
    accounts: Vec<AccountRate>,
}

fn accounts() -> Result<Vec<AccountStatus>> {
    #[derive(Deserialize)]
    struct Status {
        accounts: Vec<AccountStatus>,
    }
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(["status", "--json"])
        .output()
        .context("read account usage")?;
    if !status.status.success() {
        bail!("cannot read account usage; run codexctl status --json for details");
    }
    Ok(serde_json::from_slice::<Status>(&status.stdout)
        .context("invalid account status response")?
        .accounts)
}

pub fn run(json: bool, minutes: u32) -> Result<()> {
    let path = dirs::home_dir()
        .context("cannot locate home directory")?
        .join(".codex/logs_2.sqlite");
    // Do not use immutable=1: a running Codex process may have replies in the WAL.
    // READ_ONLY also refuses a missing database instead of creating an empty one.
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot read Codex logs at {}", path.display()))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let accounts = accounts()?;
    let now = Utc::now();
    let since = now.timestamp() - i64::from(minutes) * 60;
    let resets: Vec<_> = accounts
        .iter()
        .flat_map(|a| {
            let primary = a
                .primary_window_seconds
                .filter(|seconds| *seconds > 0)
                .and(a.primary_resets_at.as_deref());
            // Preserve the legacy weekly fallback, but never guess a short
            // window when status does not declare its duration.
            let weekly = a.secondary_resets_at.as_deref().filter(|_| {
                a.secondary_window_seconds
                    .is_none_or(|seconds| seconds == 604800)
            });
            [
                (primary, a.primary_window_seconds),
                (weekly, a.secondary_window_seconds),
            ]
            .into_iter()
            .filter_map(move |(reset, seconds)| {
                let reset = DateTime::parse_from_rfc3339(reset?).ok()?;
                Some((reset.timestamp(), seconds, a))
            })
        })
        .collect();
    let status_pattern = Regex::new(r"/codex/responses status=(\d{3})\b")?;
    let reset_pattern = Regex::new(r#""x-codex-primary-reset-at"\s*:\s*"(\d+)""#)?;
    let window_pattern = Regex::new(r#""x-codex-primary-window-minutes"\s*:\s*"(\d+)""#)?;
    let mut current: HashMap<String, Option<&AccountStatus>> = HashMap::new();
    // None is a separate key: an account literally called "unattributed" cannot
    // absorb traffic for which we have no evidence.
    let mut totals: BTreeMap<Option<String>, AccountRate> = BTreeMap::new();
    let mut query = db
        .prepare(
            "SELECT process_uuid, feedback_log_body FROM logs
         WHERE ts >= ?1 AND ts <= ?2 AND feedback_log_body LIKE '%/codex/responses status=%'
         ORDER BY ts, rowid",
        )
        .context("unsupported Codex log database schema")?;
    let rows = query.query_map([since, now.timestamp()], |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (process, body) = row?;
        let Some(status) = status_pattern
            .captures(&body)
            .and_then(|c| c[1].parse::<u16>().ok())
        else {
            continue;
        };
        let has_header = body.contains("\"x-codex-primary-reset-at\"");
        let has_window = body.contains("\"x-codex-primary-window-minutes\"");
        let window = window_pattern
            .captures(&body)
            .and_then(|c| c[1].parse::<u64>().ok())
            .and_then(|minutes| minutes.checked_mul(60))
            .filter(|seconds| *seconds > 0);
        let observed = reset_pattern
            .captures(&body)
            .and_then(|c| c[1].parse::<i64>().ok())
            .and_then(|reset| {
                if has_window && window.is_none() {
                    return None;
                }
                let mut matches = resets.iter().filter(|(candidate, seconds, _)| {
                    candidate.abs_diff(reset) <= 120
                        && window.is_none_or(|window| *seconds == Some(window))
                });
                let first = matches.next()?;
                // Both windows of one account can reset together. Ambiguity is
                // between accounts, not between that account's own windows.
                matches
                    .all(|(_, _, account)| account.alias == first.2.alias)
                    .then_some(first.2)
            });
        if has_header && let Some(process) = process.as_ref().filter(|p| !p.is_empty()) {
            // Unknown/malformed/ambiguous evidence clears a previous assignment.
            current.insert(process.clone(), observed);
        }
        if status != 429 && !(200..300).contains(&status) {
            continue;
        }
        let account = if has_header {
            observed
        } else {
            process
                .as_ref()
                .and_then(|p| current.get(p).copied().flatten())
        };
        let row = totals
            .entry(account.map(|a| a.alias.clone()))
            .or_insert_with(|| AccountRate {
                account: account.map_or(".unattributed", |a| &a.alias).to_owned(),
                weekly_used_percent: account.and_then(|a| a.secondary_used_percent),
                ..Default::default()
            });
        if status == 429 {
            row.responses_429 += 1;
        } else {
            row.responses_ok += 1;
        }
        if let Some(process) = process.filter(|p| !p.is_empty()) {
            if let Some(pid) = process
                .strip_prefix("pid:")
                .and_then(|rest| rest.split(':').next())
                .and_then(|pid| pid.parse::<u32>().ok())
                .filter(|pid| *pid > 0)
            {
                row.pids.insert(pid);
            }
            row.process_ids.insert(process);
        }
    }
    let mut rows: Vec<_> = totals.into_values().collect();
    for row in &mut rows {
        row.rate_429 = row.responses_429 as f64 / (row.responses_ok + row.responses_429) as f64;
        row.processes = row.process_ids.len();
    }
    rows.sort_by(|a, b| a.account.cmp(&b.account));
    let report = Report {
        host: hostname::get()?.to_string_lossy().into_owned(),
        window_minutes: minutes,
        generated_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        accounts: rows,
    };
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("window {} min, host {}", report.window_minutes, report.host);
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED).set_header([
            "Account",
            "Weekly",
            "OK",
            "429",
            "429 rate",
            "Processes",
            "PIDs",
        ]);
        for row in report.accounts {
            table.add_row([
                row.account,
                row.weekly_used_percent
                    .map_or_else(|| "-".into(), |v| format!("{v:.1}%")),
                row.responses_ok.to_string(),
                row.responses_429.to_string(),
                format!("{:.1}%", row.rate_429 * 100.0),
                row.processes.to_string(),
                row.pids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            ]);
        }
        println!("{table}");
    }
    Ok(())
}
