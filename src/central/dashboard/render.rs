//! The browser projection owns presentation and its conservative recommendation.
//! It receives only the secret-free snapshot; neither rendering nor polling mutates accounts.
use super::enrollment::escape;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub version: u8,
    pub identity: Identity,
    pub server_time: i64,
    pub accounts: Vec<Account>,
    pub machines: Vec<Machine>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Identity {
    pub email: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Account {
    pub alias: String,
    pub label: Option<String>,
    pub plan: Option<String>,
    pub state: String,
    pub routing_refused: bool,
    pub billing_class: crate::api::BillingClass,
    pub primary: Window,
    pub secondary: Window,
    pub usage_age_seconds: Option<u64>,
    pub usage_stale: bool,
    pub usage_error: Option<String>,
    pub banked_resets: Resets,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Window {
    pub used_percent: Option<f64>,
    pub left_percent: Option<f64>,
    pub window_seconds: Option<u64>,
    pub resets_at: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Resets {
    pub count: Option<i64>,
    pub redeemable_now: Option<i64>,
    pub nearest_expiry: Option<i64>,
    pub stale: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Machine {
    pub name: String,
    pub status: String,
    pub last_seen_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_alias: Option<String>,
}
impl Account {
    fn name(&self) -> &str {
        self.label
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.alias)
    }
    fn stale(&self) -> bool {
        self.usage_stale || self.usage_age_seconds.is_none_or(|age| age >= 60)
    }
    fn used(&self) -> impl Iterator<Item = f64> + '_ {
        [self.primary.used_percent, self.secondary.used_percent]
            .into_iter()
            .flatten()
    }
    fn exhausted(&self) -> bool {
        self.used().any(|n| n >= 100.0)
    }
    fn included(&self) -> bool {
        self.billing_class == crate::api::BillingClass::RateLimited
    }
    fn ready(&self) -> bool {
        self.state == "available" && !self.routing_refused && !self.stale()
    }
    fn redeemable(&self, now: i64) -> bool {
        self.ready()
            && self.included()
            && self.exhausted()
            && !self.banked_resets.stale
            && self.banked_resets.redeemable_now.is_some_and(|n| n > 0)
            && self.banked_resets.nearest_expiry.is_none_or(|at| at > now)
    }
    fn state(&self) -> (&'static str, &'static str) {
        if self.state == "renewal_pending" {
            ("pending", "Renewal pending")
        } else if self.routing_refused {
            ("bad", "Routing refused")
        } else if self.state == "unavailable" {
            ("bad", "Login needs attention")
        } else if self.stale() {
            ("warn", "Stale usage")
        } else if self.exhausted() {
            ("bad", "Exhausted")
        } else if self.used().any(|n| n > 80.0) {
            ("warn", "Nearly exhausted")
        } else {
            ("ok", "Available")
        }
    }
}

fn recommendation(accounts: &[Account]) -> Option<&Account> {
    let eligible: Vec<_> = accounts
        .iter()
        .filter(|a| {
            a.ready()
                && a.included()
                && a.used().next().is_some()
                && [&a.primary, &a.secondary].iter().all(|w| {
                    w.used_percent.is_some()
                        || (w.window_seconds.is_none() && w.resets_at.is_none())
                })
                && a.used().all(|n| n.is_finite() && (0.0..100.0).contains(&n))
        })
        .collect();
    let below = eligible.iter().any(|a| a.used().all(|n| n < 95.0));
    eligible
        .into_iter()
        .filter(|a| !below || a.used().all(|n| n < 95.0))
        .min_by(|a, b| {
            let score = |a: &Account| {
                a.primary.used_percent.unwrap_or(0.0) * 2.0
                    + a.secondary.used_percent.unwrap_or(0.0)
            };
            a.secondary
                .resets_at
                .unwrap_or(i64::MAX)
                .cmp(&b.secondary.resets_at.unwrap_or(i64::MAX))
                .then_with(|| score(a).total_cmp(&score(b)))
                .then_with(|| a.alias.cmp(&b.alias))
        })
}
fn shell(alias: &str) -> String {
    let quoted = if !alias.is_empty()
        && alias
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        alias.to_owned()
    } else {
        format!("'{}'", alias.replace('\'', "'\\''"))
    };
    if alias.starts_with('-') {
        format!("-- {quoted}")
    } else {
        quoted
    }
}

fn command(id: &str, text: &str, primary: bool) -> String {
    let class = if primary { "primary" } else { "small" };
    let text = escape(text);
    format!(
        r#"<div class="command"><pre tabindex="0"><code id="{id}" translate="no">{text}</code></pre><button class="button {class} copy" type="button" data-copy="{id}" aria-label="Copy {text}">Copy</button></div><p class="command-note">POSIX shell syntax.</p>"#
    )
}
fn date(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| t.format("%b %-d, %H:%M UTC").to_string())
        .unwrap_or_else(|| "Unknown time".into())
}
fn reset(at: Option<i64>, now: i64) -> String {
    let Some(at) = at else {
        return "Reset time unknown".into();
    };
    let minutes = (at.saturating_sub(now).max(0) as u64).div_ceil(60);
    let text = if minutes == 0 {
        "Reset due · awaiting observation".into()
    } else if minutes >= 1440 {
        format!("Resets in {}d {}h", minutes / 1440, minutes % 1440 / 60)
    } else if minutes >= 60 {
        format!("Resets in {}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("Resets in {minutes}m")
    };
    format!(r#"<time title="{}">{text}</time>"#, date(at))
}
fn age(a: &Account) -> String {
    match a.usage_age_seconds {
        Some(n) if n < 60 => format!("{n} s ago"),
        Some(n) => format!("{} min ago", n / 60),
        None => "time unknown".into(),
    }
}
fn window(w: &Window, a: &Account, fallback: &str, figure: bool, now: i64) -> String {
    // An entirely absent window is a missing limit only when telemetry exists.
    let absent = w.used_percent.is_none()
        && w.window_seconds.is_none()
        && w.resets_at.is_none()
        && a.used().next().is_some();
    let label = match w.window_seconds {
        Some(n) if n % 86400 == 0 => format!("{}-day", n / 86400),
        Some(n) if n % 3600 == 0 => format!("{}-hour", n / 3600),
        Some(n) => format!("{n}-second"),
        None => fallback.into(),
    };
    let left = w
        .used_percent
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| (100.0 - n).clamp(0.0, 100.0));
    let value = left
        .map(|n| {
            format!(
                "{n:.0}<small>%{}</small>",
                if figure { "" } else { " left" }
            )
        })
        .unwrap_or_else(|| if absent { "No limit" } else { "Unknown" }.into());
    let severity = if left.is_some_and(|n| n < 20.0) {
        "warn"
    } else {
        ""
    };
    let meter = match left {
        Some(n) => format!(
            r#"<progress class="meter {severity}" aria-hidden="true" max="100" value="{n}"></progress>"#
        ),
        None if absent => String::new(),
        None => r#"<div class="meter unknown" aria-hidden="true"></div>"#.into(),
    };
    let time = if absent {
        String::new()
    } else {
        reset(w.resets_at, now)
    };
    let unknown = if left.is_none() { "none" } else { "" };
    if figure {
        format!(
            r#"<div class="figure"><span class="figure-value {unknown}">{value}</span>{meter}<span class="figure-label">{label} left · <span>{time}</span></span></div>"#
        )
    } else {
        format!(
            r#"<span class="cell-label" aria-hidden="true">{label}</span><div class="usage {severity} {unknown}"><div class="usage-top"><span class="usage-value">{value}</span><span class="usage-reset">{time}</span></div>{meter}</div>"#
        )
    }
}
fn fallback(id: &str) -> String {
    format!(
        r#"<h1>Cannot confirm headroom</h1><p class="context">Check live usage before choosing an account.</p>{}<p class="effect">Checks live usage before switching this machine.</p>"#,
        command(id, "codexctl use", true)
    )
}
fn answer(accounts: &[Account], best: Option<&Account>, now: i64) -> String {
    if let Some(a) = best {
        return format!(
            r#"<h1 id="answer-title">Use {} <span class="alias" translate="no">{}</span></h1><div class="figures">{}{}</div>{}<p class="effect">Run it on the machine you want to switch. Its running Codex sessions move to this account within 60 seconds with codexctl v0.1.37 or later.</p><p class="rule">Chosen like <code>codexctl use</code>: included usage only, no account at 95% used or more while another is below, soonest 7-day reset first. Updated {}.</p>"#,
            escape(a.name()),
            escape(&a.alias),
            window(&a.primary, a, "5-hour", true, now),
            window(&a.secondary, a, "7-day", true, now),
            command(
                "cmd-use",
                &format!("codexctl use {}", shell(&a.alias)),
                true
            ),
            age(a)
        );
    }
    if accounts.is_empty() {
        return format!(
            r#"<h1 id="answer-title">No server accounts yet</h1><p>Connect a machine and migrate your profiles to get started.</p>{}"#,
            command("cmd-migrate", "codexctl migrate", true)
        );
    }
    if let Some(a) = accounts
        .iter()
        .filter(|a| a.redeemable(now))
        .min_by_key(|a| a.banked_resets.nearest_expiry.unwrap_or(i64::MAX))
    {
        return format!(
            r#"<h1 id="answer-title">No included usage left</h1><p class="next-step">Redeem a banked reset for <b>{}</b>, then switch.</p>{}<p class="rule">Spends the qualifying reset closest to expiry, only while a window is exhausted.</p>"#,
            escape(a.name()),
            command(
                "cmd-reset-use",
                &format!(
                    "codexctl reset {}\ncodexctl use {}",
                    shell(&a.alias),
                    shell(&a.alias)
                ),
                true
            )
        );
    }
    if accounts
        .iter()
        .any(|a| a.state == "available" && a.included() && a.stale())
    {
        return fallback("cmd-live-current");
    }
    let mut html = String::from(r#"<h1 id="answer-title">No included usage available</h1>"#);
    // All exhausted windows must reset before this account has headroom again.
    let next = accounts
        .iter()
        .filter(|a| a.ready() && a.included() && a.exhausted())
        .filter_map(|a| {
            let exhausted: Vec<_> = [&a.primary, &a.secondary]
                .into_iter()
                .filter(|w| w.used_percent.is_some_and(|n| n >= 100.0))
                .collect();
            if exhausted.iter().any(|w| w.resets_at.is_none()) {
                return None;
            }
            exhausted
                .iter()
                .filter_map(|w| w.resets_at)
                .max()
                .map(|at| (at, a))
        })
        .min_by_key(|(at, _)| *at);
    if let Some((at, a)) = next {
        html += &format!(
            "<p class=\"next-step\">{} · {}</p>",
            escape(a.name()),
            reset(Some(at), now)
        );
    }
    html += "<p>Check the accounts that need attention below.</p>";
    for (i, a) in accounts
        .iter()
        .filter(|a| a.ready() && a.billing_class == crate::api::BillingClass::UsageBased)
        .enumerate()
    {
        html += &format!(
            "<div class=\"alternatives\"><p>{} bills credits. This command asks before it bills.</p>{}</div>",
            escape(a.name()),
            command(
                &format!("cmd-billing-{i}"),
                &format!("codexctl use {}", shell(&a.alias)),
                false
            )
        );
    }
    html
}
fn attention(accounts: &[Account], all_stale: bool, now: i64) -> String {
    let mut items = Vec::new();
    for (i, a) in accounts.iter().enumerate() {
        let (class, state) = a.state();
        let mut note = String::new();
        let mut fix = None;
        match state {
            "Routing refused" => note = "The account server refused token routing. Run codexctl status and contact your account server operator if it persists.".into(),
            "Login needs attention" => fix = Some(format!("codexctl login {}", shell(&a.alias))),
            "Renewal pending" => { note = "Finish the OpenAI sign-in on the machine that started it, or cancel:".into(); fix = Some(if a.alias.starts_with('-') { format!("codexctl login --cancel {}", shell(&a.alias)) } else { format!("codexctl login {} --cancel", shell(&a.alias)) }); }
            "Stale usage" if !all_stale => note = format!("Last observed {}. The server refreshes every 60 seconds.", age(a)),
            "Exhausted" | "Nearly exhausted" => {
                note = [&a.primary, &a.secondary].into_iter().filter(|w| w.used_percent.is_some_and(|n| n > 80.0)).map(|w| reset(w.resets_at, now)).collect::<Vec<_>>().join(" · ");
                if a.redeemable(now) { fix = Some(format!("codexctl reset {}", shell(&a.alias))); }
            }
            _ => {}
        }
        let expiring = !a.stale()
            && !a.banked_resets.stale
            && a.banked_resets.count.is_some_and(|n| n > 0)
            && a.banked_resets
                .nearest_expiry
                .is_some_and(|at| at > now && at <= now + 3 * 86400);
        if expiring {
            note += &format!(
                " Banked reset expires {}. A reset can only be spent once a window is exhausted.",
                date(a.banked_resets.nearest_expiry.unwrap())
            );
        }
        if note.is_empty() && fix.is_none() {
            continue;
        }
        let title = if state == "Available" {
            "Reset expiring"
        } else {
            state
        };
        let class = if title == "Reset expiring" {
            "warn"
        } else {
            class
        };
        let action = fix
            .map(|cmd| command(&format!("cmd-fix-{i}"), &cmd, false))
            .unwrap_or_default();
        let fresh = if matches!(state, "Exhausted" | "Nearly exhausted" | "Available") {
            "fresh-action"
        } else {
            ""
        };
        items.push(format!(r#"<article class="issue {fresh}"><div class="issue-head"><div class="issue-account"><b>{}</b><span class="alias" translate="no">{}</span></div><span class="state {class}">{title}</span></div><p>{note}</p>{action}</article>"#, escape(a.name()), escape(&a.alias)));
    }
    format!(
        r#"<section class="attention" aria-labelledby="attention-title"><div class="section-head"><h2 id="attention-title">Needs attention <span class="count">{}</span></h2></div>{}</section>"#,
        items.len(),
        if items.is_empty() {
            if all_stale {
                "<p class=\"all-clear\">Refresh usage to check account health.</p>".into()
            } else {
                "<p class=\"all-clear\"><span class=\"state ok\">All clear</span><br>No account needs a fix.</p>".into()
            }
        } else {
            items.join("")
        }
    )
}
fn ledger(accounts: &[Account], best: Option<&Account>, all_stale: bool, now: i64) -> String {
    let mut rows = String::new();
    let mut sorted: Vec<_> = accounts.iter().collect();
    sorted.sort_by_key(|a| {
        (
            if best.is_some_and(|b| b.alias == a.alias) {
                0
            } else {
                match a.state().1 {
                    "Available" => 1,
                    "Exhausted" => 2,
                    "Nearly exhausted" => 3,
                    "Stale usage" => 4,
                    "Renewal pending" => 5,
                    _ => 6,
                }
            },
            &a.alias,
        )
    });
    for a in sorted {
        let pick = best.is_some_and(|b| b.alias == a.alias);
        let class = format!(
            "{} {}",
            if pick { "recommended" } else { "" },
            if a.stale() { "stale" } else { "" }
        );
        let plan = a
            .plan
            .as_ref()
            .map(|p| format!("<span class=\"tag\">{}</span>", escape(&p.to_uppercase())))
            .unwrap_or_default();
        let note = if pick {
            "Recommended"
        } else {
            match a.billing_class {
                crate::api::BillingClass::UsageBased => "Usage-based · bills credits",
                crate::api::BillingClass::Unknown => "Billing unknown",
                _ => "",
            }
        };
        let resets = &a.banked_resets;
        let count = match resets.count {
            Some(0) => "None".into(),
            Some(n) => n.to_string(),
            None => "Unknown".into(),
        };
        let expiry = resets
            .nearest_expiry
            .map(|at| format!("<span>Expires {}</span>", date(at)))
            .unwrap_or_default();
        let redeemable = if a.redeemable(now) {
            "<span class=\"redeemable fresh-action\">Redeemable now</span>"
        } else {
            ""
        };
        let inventory = if resets.stale {
            "<span>Reset inventory stale</span>"
        } else {
            ""
        };
        let (state_class, state) = a.state();
        let state = if all_stale && state == "Stale usage" {
            "Last observed"
        } else {
            state
        };
        rows += &format!(
            r#"<tr role="row" class="{class}"><td role="cell" class="cell-account"><div class="account-name"><strong>{}</strong><span class="line"><span class="alias" translate="no">{}</span>{plan}</span><span class="note {}">{note}</span></div></td><td role="cell" class="cell-usage">{}</td><td role="cell" class="cell-usage">{}</td><td role="cell" class="cell-banked"><span class="cell-label" aria-hidden="true">Banked resets</span><div class="banked"><b>{count}</b>{expiry}{redeemable}{inventory}</div></td><td role="cell" class="cell-state"><div class="status"><span class="state {state_class}">{state}</span><span class="status-detail" data-age="{}">Last observed {}</span></div></td></tr>"#,
            escape(a.name()),
            escape(&a.alias),
            if pick { "pick" } else { "billing" },
            window(&a.primary, a, "5-hour", false, now),
            window(&a.secondary, a, "7-day", false, now),
            a.usage_age_seconds
                .map(|n| n.to_string())
                .unwrap_or_default(),
            age(a)
        );
    }
    format!(
        r#"<section class="section" aria-labelledby="accounts-title"><div class="section-head"><h2 id="accounts-title">All accounts <span class="count">{}</span></h2><p>Refreshes every 60 seconds</p></div><table class="ledger" role="table" aria-labelledby="accounts-title"><thead role="rowgroup"><tr role="row"><th scope="col">Account</th><th scope="col" class="col-usage">5-hour window</th><th scope="col" class="col-usage">7-day window</th><th scope="col" class="col-banked">Banked resets</th><th scope="col" class="col-state">State</th></tr></thead><tbody role="rowgroup">{rows}</tbody></table></section>"#,
        accounts.len()
    )
}
fn machines(snapshot: &Snapshot) -> String {
    let mut rows = String::new();
    let mut revoked = String::new();
    let mut count = 0;
    let mut revoked_count = 0;
    let mut machines: Vec<_> = snapshot.machines.iter().collect();
    machines.sort_by_key(|m| std::cmp::Reverse(m.last_seen_at));
    for m in machines {
        let seen = m
            .last_seen_at
            .map(date)
            .unwrap_or_else(|| "No token delivery yet".into());
        if m.status == "revoked" {
            revoked_count += 1;
            revoked += &format!(
                "<li><b>{}</b><span>Revoked</span><span>Last token delivery {seen}</span></li>",
                escape(&m.name)
            );
            continue;
        }
        count += 1;
        let live = m
            .last_seen_at
            .is_some_and(|at| snapshot.server_time - at < 300);
        let (class, state) = if live {
            ("live", "In use")
        } else {
            ("idle", "Idle")
        };
        let alias = m.last_used_alias.as_deref().unwrap_or("Account unknown");
        let label = snapshot
            .accounts
            .iter()
            .find(|a| a.alias == alias)
            .map(|a| escape(a.name()))
            .unwrap_or_default();
        rows += &format!(
            r#"<tr role="row" class="{class}"><td role="cell">{}</td><td role="cell" class="cell-account">{label}<span class="alias" translate="no">{}</span></td><td role="cell" class="cell-seen"><span class="seen">{seen}</span></td><td role="cell" class="cell-state"><span class="state {class}" data-seen="{}">{state}</span></td></tr>"#,
            escape(&m.name),
            escape(alias),
            m.last_seen_at.unwrap_or(0)
        );
    }
    let disclosure = if revoked_count == 0 {
        String::new()
    } else {
        format!(
            r#"<details class="revoked" data-param="revoked" data-value="show"><summary>{revoked_count} revoked machine{plural}</summary><ul>{revoked}</ul></details>"#,
            plural = if revoked_count == 1 { "" } else { "s" }
        )
    };
    format!(
        r#"<section class="section" aria-labelledby="machines-title"><div class="section-head"><h2 id="machines-title">Machines <span class="count">{count}</span></h2><p>The account each machine last received a token for</p></div><table class="machines" role="table" aria-labelledby="machines-title"><thead role="rowgroup"><tr role="row"><th scope="col">Machine</th><th scope="col">Account</th><th scope="col" class="col-seen">Last token delivery</th><th scope="col" class="col-state">State</th></tr></thead><tbody role="rowgroup">{rows}</tbody></table><p class="machines-note">“In use” means a token delivery in the last 5 minutes. The server cannot see whether a session is still running.</p>{disclosure}</section>"#
    )
}

pub(super) fn overview(snapshot: &Snapshot) -> String {
    let best = recommendation(&snapshot.accounts);
    let all_stale = !snapshot.accounts.is_empty() && snapshot.accounts.iter().all(Account::stale);
    // Only observations we can refresh can set the browser's next refresh.
    // Unavailable/routing-refused accounts do not affect the answer and may
    // retain old telemetry indefinitely.
    let valid_for = snapshot
        .accounts
        .iter()
        .filter(|a| a.ready())
        .filter_map(|a| a.usage_age_seconds)
        .map(|age| 60u64.saturating_sub(age))
        .min()
        .unwrap_or(60);
    let notice = if all_stale {
        "All usage observations are stale. Cannot confirm headroom."
    } else {
        ""
    };
    let email = escape(&snapshot.identity.email);
    format!(
        include_str!("accounts.html"),
        email = email,
        server_time = snapshot.server_time,
        has_accounts = if snapshot.accounts.is_empty() {
            "false"
        } else {
            "true"
        },
        valid_for = valid_for,
        refresh_margin = super::super::catalog::REFRESH_MARGIN.as_secs(),
        notice = notice,
        notice_hidden = if all_stale { "" } else { "hidden" },
        answer = answer(&snapshot.accounts, best, snapshot.server_time),
        fallback = fallback("cmd-live"),
        attention = attention(&snapshot.accounts, all_stale, snapshot.server_time),
        ledger = ledger(&snapshot.accounts, best, all_stale, snapshot.server_time),
        machines = machines(snapshot)
    )
}

#[cfg(test)]
mod tests;
