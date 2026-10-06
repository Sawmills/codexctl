//! `codexctl loans`: lend a server account, list, end, and read the audit.
use super::{AuditEvent, Grant};
use crate::central::{remote, transport};
use anyhow::{Context, Result, bail};
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};
use serde_json::{Value, json};

fn connection() -> Result<remote::Connection> {
    remote::connection()?.context("not connected to an account server; run codexctl connect")
}

fn url(connection: &remote::Connection, path: &str) -> String {
    format!("{}{path}", connection.server.trim_end_matches('/'))
}

/// Send a request and return its JSON. A refusal names the server's reason.
fn send(request: reqwest::blocking::RequestBuilder) -> Result<Value> {
    let response = request.send()?;
    let status = response.status();
    let body: Value = response.json().unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = body["error"].as_str().unwrap_or("unknown");
        bail!("loan request refused: {reason} (HTTP {})", status.as_u16());
    }
    Ok(body)
}

fn get(path: &str) -> Result<Value> {
    let connection = connection()?;
    send(
        transport::blocking()?
            .get(url(&connection, path))
            .bearer_auth(remote::secret(&connection)?),
    )
}

fn post(path: &str, body: Value) -> Result<Value> {
    let connection = connection()?;
    send(
        transport::blocking()?
            .post(url(&connection, path))
            .bearer_auth(remote::secret(&connection)?)
            .json(&body),
    )
}

fn time(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string()
        })
        .unwrap_or_else(|| at.to_string())
}

/// Lend one of the caller's server accounts. `until` is RFC 3339.
pub fn lend(alias: &str, to: &str, until: Option<&str>) -> Result<()> {
    let until = until
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|at| at.timestamp())
                .context("--until must be an RFC 3339 time, for example 2026-10-09T18:00:00-07:00")
        })
        .transpose()?;
    let grant: Grant = serde_json::from_value(post(
        "/v1/loans",
        json!({"alias": alias, "borrowerEmail": to, "until": until}),
    )?)?;
    println!(
        "Lent {} to {} until {}.",
        grant.alias,
        grant.borrower_email,
        time(grant.ends_at)
    );
    println!("The borrower uses it as {}.", grant.reference);
    println!("End it with: codexctl loans end {}", grant.id);
    Ok(())
}

pub fn list(json: bool) -> Result<()> {
    let value = get("/v1/loans")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let grants: Vec<Grant> = serde_json::from_value(value)?;
    if grants.is_empty() {
        println!("No loans.");
        return Ok(());
    }
    let me = connection()?.user_id;
    let now = chrono::Utc::now().timestamp();
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(["Account", "Role", "With", "Ends", "State", "ID"]);
    for grant in grants {
        let (role, with) = if grant.lender == me {
            ("lent", grant.borrower_email.as_str())
        } else {
            ("borrowed", grant.lender_email.as_str())
        };
        let state = match grant.end_reason {
            Some(reason) => Cell::new(reason.as_str()).fg(Color::DarkGrey),
            None if grant.active(now) => Cell::new("active").fg(Color::Green),
            None => Cell::new("expired").fg(Color::DarkGrey),
        };
        table.add_row(vec![
            Cell::new(&grant.reference),
            Cell::new(role),
            Cell::new(with),
            Cell::new(time(grant.ended_at.unwrap_or(grant.ends_at))),
            state,
            Cell::new(&grant.id[..12.min(grant.id.len())]),
        ]);
    }
    println!("{table}");
    Ok(())
}

/// End a loan by ID or by an unambiguous ID prefix from `codexctl loans list`.
pub fn end(id: &str) -> Result<()> {
    let grants: Vec<Grant> = serde_json::from_value(get("/v1/loans")?)?;
    let matches: Vec<_> = grants.iter().filter(|g| g.id.starts_with(id)).collect();
    let grant = match matches.as_slice() {
        [grant] => grant,
        [] => bail!("no loan with ID {id}"),
        _ => bail!("loan ID {id} is ambiguous; use more characters"),
    };
    let ended: Grant = serde_json::from_value(post("/v1/loans/end", json!({"id": grant.id}))?)?;
    println!(
        "Loan of {} ended ({}).",
        ended.reference,
        ended.end_reason.map_or("ended", |reason| reason.as_str())
    );
    println!(
        "The server issues no new tokens. A token already issued stays valid until it expires; a login renewal is the only hard cut."
    );
    Ok(())
}

pub fn audit(id: Option<&str>, json: bool) -> Result<()> {
    let path = match id {
        Some(id) => format!("/v1/loans/audit?id={id}"),
        None => "/v1/loans/audit".into(),
    };
    let value = get(&path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let events: Vec<AuditEvent> = serde_json::from_value(value)?;
    if events.is_empty() {
        println!("No loan events.");
        return Ok(());
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(["Time", "Loan", "Event", "Detail"]);
    for event in events {
        let kind = serde_json::to_value(event.kind)?;
        let detail = event
            .reason
            .or(event.machine.map(|machine| format!("machine {machine}")))
            .unwrap_or_default();
        table.add_row(vec![
            time(event.at),
            event.grant_id[..12.min(event.grant_id.len())].to_owned(),
            kind.as_str().unwrap_or_default().to_owned(),
            detail,
        ]);
    }
    println!("{table}");
    Ok(())
}

#[derive(clap::Subcommand)]
pub enum LoansAction {
    /// Lend one of your server accounts until its weekly reset or --until.
    Lend {
        alias: String,
        /// The borrower's company email.
        #[arg(long)]
        to: String,
        /// An earlier end, as an RFC 3339 time.
        #[arg(long)]
        until: Option<String>,
    },
    /// List the loans you lent or borrowed.
    List {
        #[arg(long)]
        json: bool,
    },
    /// End a loan, as the lender or the borrower.
    End { id: String },
    /// Show the audit events of your loans.
    Audit {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(action: LoansAction) -> Result<()> {
    match action {
        LoansAction::Lend { alias, to, until } => lend(&alias, &to, until.as_deref()),
        LoansAction::List { json } => list(json),
        LoansAction::End { id } => end(&id),
        LoansAction::Audit { id, json } => audit(id.as_deref(), json),
    }
}
