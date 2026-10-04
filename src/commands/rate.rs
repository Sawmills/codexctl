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
mod ownership;

#[derive(Serialize)]
struct OwnedPid {
    pid: u32,
    source: ownership::Source,
}

#[derive(Default, Serialize)]
struct AccountRate {
    account: String,
    weekly_used_percent: Option<f64>,
    responses_ok: u64,
    responses_429: u64,
    rate_429: f64,
    processes: usize,
    pids: BTreeSet<u32>,
    owned_pids: Vec<OwnedPid>,
    #[serde(skip)]
    process_ids: BTreeSet<String>,
}

#[derive(Serialize)]
struct Report {
    host: String,
    window_minutes: u32,
    generated_at: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
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
    // A machine without logs still has an account catalog. Never create a DB.
    let db = if path.try_exists()? {
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("cannot read Codex logs at {}", path.display()))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        Some(db)
    } else {
        None
    };
    let accounts = accounts()?;
    let host_account = accounts
        .iter()
        .find(|account| {
            matches!(account.source, codexctl::status_json::Source::Server)
                && matches!(account.state, codexctl::status_json::State::Active)
        })
        .map(|account| account.alias.as_str());
    let snapshot = ownership::snapshot(host_account);
    let mut report = collect_report(db.as_ref(), &accounts, minutes, &snapshot.processes)?;
    report.warnings = snapshot.warnings;
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
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
            "Owned PIDs",
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
                row.owned_pids
                    .iter()
                    .map(|owned| format!("{} ({:?})", owned.pid, owned.source))
                    .collect::<Vec<_>>()
                    .join(", "),
            ]);
        }
        println!("{table}");
    }
    Ok(())
}

fn collect_report(
    db: Option<&Connection>,
    accounts: &[AccountStatus],
    minutes: u32,
    live: &[ownership::LiveProcess],
) -> Result<Report> {
    let now = Utc::now();
    let since = now.timestamp() - i64::from(minutes) * 60;
    let live_by_pid: HashMap<_, _> = live.iter().map(|process| (process.pid, process)).collect();
    let scan_since = live
        .iter()
        .filter(|process| process.account.is_none())
        .map(|process| process.started_at)
        .fold(since, i64::min);
    let mut log_owners = HashMap::new();
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
                // An undeclared duration is compatible only with a weekly header
                // for this legacy secondary slot; status metadata stays unknown.
                (weekly, Some(a.secondary_window_seconds.unwrap_or(604800))),
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
    for account in accounts
        .iter()
        .filter(|account| matches!(account.source, codexctl::status_json::Source::Server))
    {
        totals.insert(
            Some(account.alias.clone()),
            AccountRate {
                account: account.alias.clone(),
                weekly_used_percent: account.secondary_used_percent,
                ..Default::default()
            },
        );
    }
    let mut query = db
        .map(|db| {
            db.prepare(
                "SELECT process_uuid, feedback_log_body, ts FROM logs
         WHERE ts >= ?1 AND ts <= ?2 AND feedback_log_body LIKE '%/codex/responses status=%'
         ORDER BY ts, rowid",
            )
        })
        .transpose()
        .context("unsupported Codex log database schema")?;
    let rows = query
        .as_mut()
        .map(|query| {
            query.query_map([scan_since, now.timestamp()], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
        })
        .transpose()?;
    for row in rows.into_iter().flatten() {
        let (process, body, timestamp) = row?;
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
                // Both windows of one status row can reset together. Distinct
                // rows stay ambiguous, including local/server alias collisions.
                matches
                    .all(|(_, _, account)| std::ptr::eq(*account, first.2))
                    .then_some(first.2)
            });
        if has_header
            && let Some(pid) = process.as_deref().and_then(process_pid)
            && live_by_pid
                .get(&pid)
                .is_some_and(|process| timestamp >= process.started_at)
        {
            log_owners.insert(pid, observed);
        }
        // Ownership may use older evidence; response counts retain their
        // original window-scoped header inheritance.
        if timestamp < since {
            continue;
        }
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
            if let Some(pid) = process_pid(&process) {
                row.pids.insert(pid);
            }
            row.process_ids.insert(process);
        }
    }
    for process in live {
        let owner = process
            .account
            .as_ref()
            .map(|(alias, source)| (alias.as_str(), *source))
            .or_else(|| {
                log_owners
                    .get(&process.pid)
                    .copied()
                    .flatten()
                    .map(|account| (account.alias.as_str(), ownership::Source::Log))
            });
        let Some((alias, source)) = owner else {
            continue;
        };
        let Some(account) = accounts.iter().find(|account| account.alias == alias) else {
            continue;
        };
        totals
            .entry(Some(alias.to_owned()))
            .or_insert_with(|| AccountRate {
                account: alias.to_owned(),
                weekly_used_percent: account.secondary_used_percent,
                ..Default::default()
            })
            .owned_pids
            .push(OwnedPid {
                pid: process.pid,
                source,
            });
    }
    let mut rows: Vec<_> = totals.into_values().collect();
    for row in &mut rows {
        let responses = row.responses_ok + row.responses_429;
        row.rate_429 = if responses == 0 {
            0.0
        } else {
            row.responses_429 as f64 / responses as f64
        };
        row.processes = row.process_ids.len();
        row.owned_pids.sort_by_key(|owned| owned.pid);
    }
    rows.sort_by(|a, b| a.account.cmp(&b.account));
    Ok(Report {
        host: hostname::get()?.to_string_lossy().into_owned(),
        window_minutes: minutes,
        generated_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        warnings: Vec::new(),
        accounts: rows,
    })
}

fn process_pid(process: &str) -> Option<u32> {
    process
        .strip_prefix("pid:")?
        .split(':')
        .next()?
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codexctl::{profile::Meta, status_json::Source};
    use serde_json::json;

    fn weekly_account() -> AccountStatus {
        let mut account = AccountStatus::local(
            &Meta {
                alias: "shared".into(),
                ..Meta::default()
            },
            false,
        );
        account.secondary_resets_at = Some("2100-01-01T00:00:00Z".into());
        account.secondary_used_percent = Some(10.0);
        account
    }

    fn fixture(bodies: &[&str]) -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE logs (ts INTEGER, process_uuid TEXT, feedback_log_body TEXT)",
        )
        .unwrap();
        for body in bodies {
            db.execute(
                "INSERT INTO logs VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    Utc::now().timestamp(),
                    "pid:301:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    body
                ],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn b21b_legacy_weekly_duration_accepts_only_a_weekly_header() {
        let mut account = weekly_account();
        account.primary_resets_at = account.secondary_resets_at.clone();
        assert!(account.primary_window_seconds.is_none());
        assert!(account.secondary_window_seconds.is_none());
        let db = fixture(&[
            r#"/codex/responses status=200 headers={"x-codex-primary-reset-at": "4102444800", "x-codex-primary-window-minutes": "10080"}"#,
            "/codex/responses status=429",
            r#"/codex/responses status=429 headers={"x-codex-primary-reset-at": "4102444800", "x-codex-primary-window-minutes": "300"}"#,
        ]);
        let report = collect_report(Some(&db), &[account], 10, &[]).unwrap();
        assert_eq!(
            serde_json::to_value(report.accounts).unwrap(),
            json!([
                {"account":".unattributed", "weekly_used_percent":null, "responses_ok":0, "responses_429":1, "rate_429":1.0, "processes":1, "pids":[301], "owned_pids":[]},
                {"account":"shared", "weekly_used_percent":10.0, "responses_ok":1, "responses_429":1, "rate_429":0.5, "processes":1, "pids":[301], "owned_pids":[]}
            ])
        );
    }

    #[test]
    fn b21b_matching_windows_in_distinct_same_alias_status_rows_stay_ambiguous() {
        let mut local = weekly_account();
        local.secondary_window_seconds = Some(604800);
        local.primary_resets_at = local.secondary_resets_at.clone();
        local.primary_window_seconds = Some(18000);
        let mut remote = weekly_account();
        remote.source = Source::Server;
        remote.secondary_window_seconds = Some(604800);
        remote.secondary_used_percent = Some(70.0);
        let db = fixture(&[
            r#"/codex/responses status=200 headers={"x-codex-primary-reset-at": "4102444800"}"#,
        ]);
        let mut rows = [local, remote];
        // Two windows from the same status row do not create ambiguity.
        assert_eq!(
            collect_report(Some(&db), &rows[..1], 10, &[])
                .unwrap()
                .accounts[0]
                .account,
            "shared"
        );
        for _ in 0..2 {
            let report = collect_report(Some(&db), &rows, 10, &[]).unwrap();
            assert_eq!(
                serde_json::to_value(report.accounts).unwrap(),
                json!([
                    {"account":".unattributed", "weekly_used_percent":null, "responses_ok":1, "responses_429":0, "rate_429":0.0, "processes":1, "pids":[301], "owned_pids":[]},
                    {"account":"shared", "weekly_used_percent":70.0, "responses_ok":0, "responses_429":0, "rate_429":0.0, "processes":0, "pids":[], "owned_pids":[]}
                ])
            );
            rows.reverse();
        }
    }
}
