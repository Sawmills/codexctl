use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};

use codexctl::status_format::format_duration;

use crate::api;
use crate::commands::resets;
use crate::config;
use crate::profile;

pub enum Filter {
    All,
    RateLimited,
    UsageBased,
}

enum CreditsStatus {
    Ok,
    Unlimited,
    None,
    Overage,
}

struct RateLimitedAccount {
    alias: String,
    /// Operator-set display name. Present only when they set one, which is what
    /// keeps the column out of a table that has nothing to put in it.
    label: Option<String>,
    limits: Vec<LimitStatus>,
    token_expiry: Option<i64>,
    /// Banked rate-limit resets held by this account.
    reset_credits: i64,
    /// How many of them can be redeemed right now (nonzero only once a window
    /// is exhausted).
    reset_credits_applicable: i64,
    /// When the soonest redeemable credit lapses. An unspent credit is simply
    /// lost, so this is the part worth acting on.
    reset_credit_expiry: Option<i64>,
    is_active: bool,
    is_error: bool,
    billing_unknown: bool,
    plan: String,
    credits: Option<api::Credits>,
    error_msg: String,
    pace: Option<codexctl::status_pace::Pace>,
}

struct LimitStatus {
    name: String,
    windows: Vec<WindowStatus>,
    availability_score: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WindowKey {
    Duration(u64, usize),
    Position(usize),
}

struct WindowStatus {
    key: WindowKey,
    label: String,
    used_pct: f64,
    reset: String,
}

impl LimitStatus {
    fn from_rate_limit(name: String, rate_limit: &api::RateLimit) -> Self {
        let mut duration_occurrences = HashMap::new();
        let mut windows: Vec<_> = rate_limit
            .windows()
            .map(|(position, window)| {
                let (key, label) = match window.duration_seconds() {
                    Some(seconds) => {
                        let occurrence = duration_occurrences.entry(seconds).or_insert(0);
                        let key = WindowKey::Duration(seconds, *occurrence);
                        *occurrence += 1;
                        let mut label = window
                            .duration_label()
                            .unwrap_or_else(|| seconds.to_string());
                        if *occurrence > 1 {
                            label = format!("{label} #{}", *occurrence);
                        }
                        (key, label)
                    }
                    None => (
                        WindowKey::Position(position),
                        positional_window_label(position).to_string(),
                    ),
                };
                WindowStatus {
                    key,
                    label,
                    used_pct: window.used_percent,
                    reset: format_window_reset(Some(window)),
                }
            })
            .collect();
        windows.sort_by_key(|window| window.key);
        Self {
            name,
            windows,
            availability_score: rate_limit.availability_score(),
        }
    }

    fn unavailable() -> Self {
        Self {
            name: "Codex".to_string(),
            windows: Vec::new(),
            availability_score: 0.0,
        }
    }
}

fn positional_window_label(position: usize) -> &'static str {
    if position == 0 {
        "Primary"
    } else {
        "Secondary"
    }
}

struct UsageBasedAccount {
    alias: String,
    label: Option<String>,
    credit_balance: Option<String>,
    seat_limit_cents: Option<u64>,
    credits_status: CreditsStatus,
    spend_control_reached: bool,
    token_expiry: Option<i64>,
    is_active: bool,
    is_error: bool,
    error_msg: String,
    pace: Option<codexctl::status_pace::Pace>,
}

impl RateLimitedAccount {
    fn from_usage(
        alias: String,
        label: Option<String>,
        is_active: bool,
        token_expiry: Option<i64>,
        usage: &api::RateLimitResponse,
    ) -> Self {
        Self {
            alias,
            label,
            limits: rate_limit_statuses(usage),
            token_expiry,
            reset_credits: usage.reset_credits_available(),
            reset_credits_applicable: usage.reset_credits_applicable(),
            reset_credit_expiry: None,
            is_active,
            is_error: false,
            billing_unknown: usage.billing_class() == api::BillingClass::Unknown,
            plan: match usage.plan_type.as_deref() {
                Some("pro") => "Pro (More)",
                Some("prolite") => "Pro",
                Some("promax") => "Pro (Max)",
                Some("free") => "Free",
                Some("go") => "Go",
                Some("plus") => "Plus",
                Some("team") => "Team",
                Some("business") => "Business",
                Some("enterprise") => "Enterprise",
                Some("edu") => "Edu",
                Some(plan) => plan,
                None => "",
            }
            .trim()
            .chars()
            .filter(|character| !character.is_control())
            .take(80)
            .collect(),
            credits: usage.credits.clone(),
            error_msg: String::new(),
            pace: codexctl::status_pace::Pace::from_usage(usage, chrono::Utc::now().timestamp()),
        }
    }

    fn availability_score(&self) -> f64 {
        if self.is_error {
            return 1000.0;
        }
        let Some(main) = self.limits.first() else {
            return 1000.0;
        };
        if main.windows.is_empty() {
            return 1000.0;
        }
        main.availability_score
    }
}

impl UsageBasedAccount {
    fn from_usage(
        alias: String,
        label: Option<String>,
        is_active: bool,
        token_expiry: Option<i64>,
        usage: &api::RateLimitResponse,
    ) -> Self {
        let credits_status = match &usage.credits {
            Some(c) if c.unlimited => CreditsStatus::Unlimited,
            Some(c) if c.overage_limit_reached => CreditsStatus::Overage,
            Some(c) if c.has_credits => CreditsStatus::Ok,
            _ => CreditsStatus::None,
        };
        Self {
            alias,
            label,
            credit_balance: usage.credits.as_ref().and_then(|c| c.balance.clone()),
            seat_limit_cents: None,
            credits_status,
            spend_control_reached: usage.spend_control.as_ref().is_some_and(|sc| sc.reached),
            token_expiry,
            is_active,
            is_error: false,
            error_msg: String::new(),
            pace: codexctl::status_pace::Pace::from_usage(usage, chrono::Utc::now().timestamp()),
        }
    }

    fn health_score(&self) -> f64 {
        if self.is_error {
            return 1000.0;
        }
        match self.credits_status {
            CreditsStatus::None => 300.0,
            CreditsStatus::Overage => 200.0,
            _ if self.spend_control_reached => 100.0,
            _ => 0.0,
        }
    }
}

pub fn run(filter: Filter, json: bool) -> Result<()> {
    #[cfg(feature = "central-prototype")]
    if codexctl::central::remote::show(
        true,
        match filter {
            Filter::All => None,
            Filter::RateLimited => Some(api::BillingClass::RateLimited),
            Filter::UsageBased => Some(api::BillingClass::UsageBased),
        },
        json,
    )? {
        return Ok(());
    }
    let StatusSnapshot {
        rate_limited,
        usage_based,
        fetched_at,
        accounts,
    } = load_sorted_statuses()?;
    let no_profiles = accounts.is_empty();
    let accounts: Vec<_> = accounts
        .into_iter()
        .filter(|account| match filter {
            Filter::All => true,
            Filter::RateLimited => rate_limited.iter().any(|row| row.alias == account.alias),
            Filter::UsageBased => usage_based.iter().any(|row| row.alias == account.alias),
        })
        .collect();
    if json {
        return codexctl::status_json::print(&accounts);
    }

    if no_profiles {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
    }
    let show_rl = matches!(filter, Filter::All | Filter::RateLimited);
    let show_ub = matches!(filter, Filter::All | Filter::UsageBased);
    let has_rows = (show_rl && !rate_limited.is_empty()) || (show_ub && !usage_based.is_empty());

    if has_rows {
        print_live_fetched_at(fetched_at);
    }

    if show_rl {
        let rate_limited_refs: Vec<&RateLimitedAccount> = rate_limited.iter().collect();
        print_rate_limited_table("Rate-Limited Accounts", &rate_limited_refs);
    }

    if show_rl && !rate_limited.is_empty() && show_ub && !usage_based.is_empty() {
        println!();
    }

    if show_ub {
        let usage_based_refs: Vec<&UsageBasedAccount> = usage_based.iter().collect();
        print_usage_based_table("Usage-Based Accounts", &usage_based_refs);
    }

    if (show_rl && rate_limited.is_empty() && !show_ub)
        || (show_ub && usage_based.is_empty() && !show_rl)
        || (rate_limited.is_empty() && usage_based.is_empty())
    {
        println!("no matching accounts found.");
    }

    if let Some(table) = fleet_pace_table(&accounts) {
        println!("\n{table}");
    }

    Ok(())
}

pub fn run_focused(focused_alias: &str) -> Result<()> {
    let StatusSnapshot {
        rate_limited,
        usage_based,
        fetched_at,
        accounts,
    } = load_sorted_statuses()?;
    if accounts.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
    }
    if !rate_limited.is_empty() || !usage_based.is_empty() {
        print_live_fetched_at(fetched_at);
    }

    let selected_rate_limited: Vec<&RateLimitedAccount> = rate_limited
        .iter()
        .filter(|account| account.alias == focused_alias)
        .collect();
    let selected_usage_based: Vec<&UsageBasedAccount> = usage_based
        .iter()
        .filter(|account| account.alias == focused_alias)
        .collect();

    let mut printed_selected =
        print_rate_limited_table("Selected Rate-Limited Account", &selected_rate_limited);
    if printed_selected && !selected_usage_based.is_empty() {
        println!();
    }
    printed_selected |=
        print_usage_based_table("Selected Usage-Based Account", &selected_usage_based);

    if !printed_selected {
        println!("selected account status unavailable: {focused_alias}");
    }

    let other_rate_limited: Vec<&RateLimitedAccount> = rate_limited
        .iter()
        .filter(|account| account.alias != focused_alias)
        .collect();
    let other_usage_based: Vec<&UsageBasedAccount> = usage_based
        .iter()
        .filter(|account| account.alias != focused_alias)
        .collect();

    if !other_rate_limited.is_empty() || !other_usage_based.is_empty() {
        println!();
        println!("Other Accounts");
        let printed_rate_limited =
            print_rate_limited_table("Rate-Limited Accounts", &other_rate_limited);
        if printed_rate_limited && !other_usage_based.is_empty() {
            println!();
        }
        print_usage_based_table("Usage-Based Accounts", &other_usage_based);
    }

    if let Some(table) = fleet_pace_table(&accounts) {
        println!("\n{table}");
    }

    Ok(())
}

struct StatusSnapshot {
    rate_limited: Vec<RateLimitedAccount>,
    usage_based: Vec<UsageBasedAccount>,
    accounts: Vec<codexctl::status_json::AccountStatus>,
    fetched_at: chrono::DateTime<chrono::Utc>,
}

fn load_sorted_statuses() -> Result<StatusSnapshot> {
    let profiles = profile::list_profiles()?;
    let fetched_at = chrono::Utc::now();
    if profiles.is_empty() {
        return Ok(StatusSnapshot {
            rate_limited: Vec::new(),
            usage_based: Vec::new(),
            accounts: Vec::new(),
            fetched_at,
        });
    }

    let paths = config::default_paths()?;
    let active = profile::get_active_from(&paths)?;

    let rt = tokio::runtime::Runtime::new()?;
    let mut snapshot = rt.block_on(fetch_and_split(&profiles, &active, &paths))?;
    snapshot.fetched_at = fetched_at;
    let rate_limited = &mut snapshot.rate_limited;
    let usage_based = &mut snapshot.usage_based;

    rate_limited.sort_by(|a, b| {
        a.availability_score()
            .partial_cmp(&b.availability_score())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    usage_based.sort_by(|a, b| {
        a.health_score()
            .partial_cmp(&b.health_score())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(snapshot)
}

fn print_live_fetched_at(fetched_at: chrono::DateTime<chrono::Utc>) {
    let local = fetched_at.with_timezone(&chrono::Local);
    println!(
        "Live status fetched at {}",
        local.format("%a %b %d %H:%M:%S")
    );
    println!();
}

fn print_rate_limited_table(title: &str, accounts: &[&RateLimitedAccount]) -> bool {
    if accounts.is_empty() {
        return false;
    }

    println!("{title}");
    println!("{}", rate_limited_table(accounts));
    true
}

fn rate_limited_table(accounts: &[&RateLimitedAccount]) -> Table {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    let columns = RateLimitColumns::for_accounts(accounts);
    table.set_header(columns.headers());
    for account in accounts {
        table.add_row(render_rate_limited_row(account, &columns));
    }

    table
}

struct RateLimitColumns {
    named_limits: bool,
    labeled: bool,
    billing: bool,
    plan: bool,
    credits: bool,
    resets: bool,
    pace: bool,
    windows: Vec<WindowColumn>,
}

struct WindowColumn {
    keys: Vec<WindowKey>,
    label: String,
}

impl RateLimitColumns {
    fn for_accounts(accounts: &[&RateLimitedAccount]) -> Self {
        let healthy: Vec<_> = accounts
            .iter()
            .filter(|account| !account.is_error)
            .collect();
        let mut declared = BTreeMap::new();
        let mut positional = BTreeMap::new();
        for account in &healthy {
            for limit in &account.limits {
                for window in &limit.windows {
                    match window.key {
                        WindowKey::Duration(_, _) => {
                            declared
                                .entry(window.key)
                                .or_insert_with(|| window.label.clone());
                        }
                        WindowKey::Position(position) => {
                            positional
                                .entry(position)
                                .or_insert_with(|| window.label.clone());
                        }
                    }
                }
            }
        }
        let mut windows: Vec<_> = declared
            .into_iter()
            .map(|(key, label)| WindowColumn {
                keys: vec![key],
                label,
            })
            .collect();
        let historical_pair = if windows.len() == 2 {
            let five_hour = windows
                .iter()
                .any(|column| column.keys[0] == WindowKey::Duration(5 * 60 * 60, 0));
            let seven_day = windows
                .iter()
                .any(|column| column.keys[0] == WindowKey::Duration(7 * 24 * 60 * 60, 0));
            (five_hour && seven_day).then_some((
                WindowKey::Duration(5 * 60 * 60, 0),
                WindowKey::Duration(7 * 24 * 60 * 60, 0),
            ))
        } else {
            None
        };
        for (position, label) in positional {
            let target = match (position, historical_pair) {
                (0, Some((five_hour, _))) => Some(five_hour),
                (1, Some((_, seven_day))) => Some(seven_day),
                _ => None,
            };
            let can_alias = target.is_some_and(|target| {
                healthy.iter().all(|account| {
                    account.limits.iter().all(|limit| {
                        let has_position = limit
                            .windows
                            .iter()
                            .any(|window| window.key == WindowKey::Position(position));
                        let has_target = limit.windows.iter().any(|window| window.key == target);
                        !(has_position && has_target)
                    })
                })
            });
            let target_index = target
                .filter(|_| can_alias)
                .and_then(|target| windows.iter().position(|column| column.keys[0] == target));
            if let Some(index) = target_index {
                windows[index].keys.push(WindowKey::Position(position));
            } else {
                let column = WindowColumn {
                    keys: vec![WindowKey::Position(position)],
                    label,
                };
                if position == 0 {
                    windows.insert(0, column);
                } else {
                    windows.push(column);
                }
            }
        }
        Self {
            billing: accounts.iter().any(|account| account.billing_unknown),
            credits: accounts.iter().any(|account| account.credits.is_some()),
            resets: accounts.iter().any(|account| account.reset_credits > 0),
            pace: healthy.iter().any(|account| account.pace.is_some()),
            plan: healthy.iter().any(|account| !account.plan.is_empty()),
            named_limits: healthy.iter().any(|account| account.limits.len() > 1),
            // An error row still carries its label, so consider every account
            // here rather than only the healthy ones.
            labeled: accounts.iter().any(|account| account.label.is_some()),
            windows,
        }
    }

    fn headers(&self) -> Vec<String> {
        let mut headers = vec!["Account".to_string()];
        if self.labeled {
            headers.push("Label".to_string());
        }
        if self.plan {
            headers.push("Plan".to_string());
        }
        if self.named_limits {
            headers.push("Limit".to_string());
        }
        for window in &self.windows {
            headers.push(window.label.clone());
            headers.push(format!("{} Reset", window.label));
        }
        if self.resets {
            headers.push("Resets".to_string());
        }
        if self.pace {
            headers.push("Pace".to_string());
        }
        if self.credits {
            headers.push("Credits".to_string());
        }
        headers.push("Token".to_string());
        if self.billing {
            headers.push("Billing".to_string());
        }
        headers
    }
}

fn print_usage_based_table(title: &str, accounts: &[&UsageBasedAccount]) -> bool {
    if accounts.is_empty() {
        return false;
    }

    println!("{title}");
    println!("{}", usage_based_table(accounts));
    true
}

fn usage_based_table(accounts: &[&UsageBasedAccount]) -> Table {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    let headers = usage_based_headers(accounts);
    let labeled = headers.get(1).is_some_and(|header| header == "Label");
    let pace = headers.iter().any(|header| header == "Pace");
    table.set_header(headers);
    for account in accounts {
        table.add_row(render_usage_based_row(account, labeled, pace));
    }
    table
}

fn usage_based_headers(accounts: &[&UsageBasedAccount]) -> Vec<String> {
    let mut headers = vec!["Account".to_string()];
    if accounts.iter().any(|account| account.label.is_some()) {
        headers.push("Label".to_string());
    }
    headers.extend(
        ["Credit balance", "Seat", "Credits", "Spend", "Token"]
            .into_iter()
            .map(str::to_string),
    );
    if accounts
        .iter()
        .any(|account| !account.is_error && account.pace.is_some())
    {
        headers.push("Pace".into());
    }
    headers
}

fn fleet_pace_table(accounts: &[codexctl::status_json::AccountStatus]) -> Option<Table> {
    let points = codexctl::status_pace::fleet_points(accounts.iter().map(|row| row.pace_points))?;
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(["Summary", "Pace"]);
    table.add_row([
        Cell::new("Fleet"),
        codexctl::status_format::pace_cell(Some(points)),
    ]);
    Some(table)
}

/// Cyan marks the cell that answers "which account is this". It is the same
/// treatment the active marker gets, and no other cell here is colored unless
/// its color carries a severity.
fn label_cell(label: Option<&str>) -> Cell {
    match label {
        Some(label) => Cell::new(label).fg(Color::Cyan),
        None => Cell::new("-"),
    }
}

fn is_usage_based_plan(plan: &str) -> bool {
    plan.contains("usage_based")
}

async fn fetch_and_split(
    profiles: &[profile::Profile],
    active: &Option<String>,
    paths: &config::Paths,
) -> Result<StatusSnapshot> {
    let client = api::http_client()?;

    // Phase 1: fetch wham/usage for all accounts in parallel
    let futures: Vec<_> = profiles
        .iter()
        .map(|p| {
            let client = client.clone();
            let alias = p.meta.alias.clone();
            let label = p.meta.label.clone();
            let meta = p.meta.clone();
            let plan_from_meta = p.meta.plan.clone();
            let is_active = active.as_deref() == Some(&p.meta.alias);
            let auth_path = profile::auth_json_path_for_profile_from(paths, p, active.as_deref());
            let auth = api::read_auth_json(&auth_path);

            async move {
                let usage_result = match &auth {
                    Ok(a) => Some(
                        api::fetch_usage_async(&client, &a.access_token, a.account_id.as_deref())
                            .await,
                    ),
                    Err(_) => None,
                };
                codexctl::statusline::record_local(
                    paths,
                    &meta,
                    usage_result.as_ref().and_then(|r| r.as_ref().ok()),
                );
                (alias, label, plan_from_meta, is_active, auth, usage_result)
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    let mut accounts: Vec<codexctl::status_json::AccountStatus> = results
        .iter()
        .zip(profiles)
        .map(|(result, profile)| {
            let (_, _, _, is_active, auth, usage) = result;
            let mut row = codexctl::status_json::AccountStatus::local(&profile.meta, *is_active);
            match (auth, usage) {
                (Ok(_), Some(Ok(usage))) => {
                    row.set_usage(usage);
                    row.resets_banked = Some(usage.reset_credits_available());
                    row.resets_redeemable = Some(usage.reset_credits_applicable());
                }
                (Ok(auth), Some(Err(error))) => {
                    row.error = Some(
                        if error.to_string().contains("expired") {
                            auth_failure_label(&auth.access_token)
                        } else {
                            "error"
                        }
                        .into(),
                    );
                }
                _ => row.error = Some("bad auth.json".into()),
            }
            if row.error.is_some() {
                row.state = codexctl::status_json::State::Unavailable;
            }
            row
        })
        .collect();

    // Phase 2: classify and build account structs
    let mut rate_limited = Vec::new();
    let mut usage_based = Vec::new();
    let mut ub_needing_settings: Vec<(usize, String, String)> = Vec::new();
    // Only accounts that actually hold banked resets need their credit listing
    // read, so the common case stays at one request per profile.
    let mut rl_needing_credits: Vec<(usize, String, Option<String>)> = Vec::new();

    for (alias, label, plan_from_meta, is_active, auth, usage_result) in &results {
        let account_id = auth.as_ref().ok().and_then(|a| a.account_id.clone());
        let auth = match auth {
            Ok(a) => a,
            Err(_) => {
                let is_ub = plan_from_meta.as_deref().is_some_and(is_usage_based_plan);
                if is_ub {
                    usage_based.push(UsageBasedAccount {
                        alias: alias.clone(),
                        label: label.clone(),
                        credit_balance: None,
                        seat_limit_cents: None,
                        credits_status: CreditsStatus::None,
                        spend_control_reached: false,
                        token_expiry: None,
                        is_active: *is_active,
                        is_error: true,
                        error_msg: "bad auth.json".to_string(),
                        pace: None,
                    });
                } else {
                    rate_limited.push(RateLimitedAccount {
                        alias: alias.clone(),
                        label: label.clone(),
                        limits: vec![LimitStatus::unavailable()],
                        token_expiry: None,
                        reset_credits: 0,
                        reset_credits_applicable: 0,
                        reset_credit_expiry: None,
                        is_active: *is_active,
                        is_error: true,
                        billing_unknown: false,
                        plan: String::new(),
                        credits: None,
                        pace: None,
                        error_msg: "bad auth.json".to_string(),
                    });
                }
                continue;
            }
        };

        let token_expiry = api::token_expiry(&auth.access_token);

        let usage = match usage_result {
            Some(Ok(u)) => u,
            Some(Err(e)) => {
                let msg = if e.to_string().contains("expired") {
                    auth_failure_label(&auth.access_token)
                } else {
                    "error"
                };
                let is_ub = plan_from_meta.as_deref().is_some_and(is_usage_based_plan);
                if is_ub {
                    usage_based.push(UsageBasedAccount {
                        alias: alias.clone(),
                        label: label.clone(),
                        credit_balance: None,
                        seat_limit_cents: None,
                        credits_status: CreditsStatus::None,
                        spend_control_reached: false,
                        token_expiry,
                        is_active: *is_active,
                        is_error: true,
                        error_msg: msg.to_string(),
                        pace: None,
                    });
                } else {
                    rate_limited.push(RateLimitedAccount {
                        alias: alias.clone(),
                        label: label.clone(),
                        limits: vec![LimitStatus::unavailable()],
                        token_expiry,
                        reset_credits: 0,
                        reset_credits_applicable: 0,
                        reset_credit_expiry: None,
                        is_active: *is_active,
                        is_error: true,
                        billing_unknown: false,
                        plan: String::new(),
                        credits: None,
                        pace: None,
                        error_msg: msg.to_string(),
                    });
                }
                continue;
            }
            None => continue,
        };

        if let Some(plan) = &usage.plan_type {
            let _ = profile::update_meta_plan(alias, plan);
        }

        let billing_class = usage.billing_class();

        if billing_class == api::BillingClass::UsageBased {
            let idx = usage_based.len();
            usage_based.push(UsageBasedAccount::from_usage(
                alias.clone(),
                label.clone(),
                *is_active,
                token_expiry,
                usage,
            ));

            if let Some(account_id) =
                account_id.or_else(|| api::extract_account_id(&auth.access_token))
            {
                ub_needing_settings.push((idx, auth.access_token.clone(), account_id));
            }
        } else {
            let idx = rate_limited.len();
            rate_limited.push(RateLimitedAccount::from_usage(
                alias.clone(),
                label.clone(),
                *is_active,
                token_expiry,
                usage,
            ));

            if usage.reset_credits_available() > 0 {
                rl_needing_credits.push((idx, auth.access_token.clone(), account_id.clone()));
            }
        }
    }

    // Phase 3: fetch seat limits for usage-based accounts (deduplicate by account_id)
    let mut unique_account_ids: HashMap<String, (String, String)> = HashMap::new();
    for (_, token, account_id) in &ub_needing_settings {
        unique_account_ids
            .entry(account_id.clone())
            .or_insert_with(|| (token.clone(), account_id.clone()));
    }

    let settings_futures: Vec<_> = unique_account_ids
        .values()
        .map(|(token, account_id)| {
            let client = client.clone();
            let token = token.clone();
            let account_id = account_id.clone();
            async move {
                let result = api::fetch_account_settings_async(&client, &token, &account_id).await;
                (account_id, result)
            }
        })
        .collect();

    let settings_results = futures::future::join_all(settings_futures).await;
    let mut settings_map: HashMap<String, u64> = HashMap::new();
    for (account_id, result) in settings_results {
        if let Ok(settings) = result
            && let Some(limits) = settings.seat_type_credit_limits
            && let Some(ub_limits) = limits.usage_based
            && let Some(first) = ub_limits.first()
        {
            settings_map.insert(account_id, first.limit);
        }
    }

    for (idx, _, account_id) in &ub_needing_settings {
        if let Some(limit) = settings_map.get(account_id) {
            usage_based[*idx].seat_limit_cents = Some(*limit);
        }
    }

    // Phase 4: read banked-reset expiries. The usage response carries the
    // counts but not when each credit lapses, and a credit that lapses unspent
    // is simply lost — which is the part worth showing.
    let credit_futures: Vec<_> = rl_needing_credits
        .iter()
        .map(|(idx, token, account_id)| {
            let client = client.clone();
            let token = token.clone();
            let account_id = account_id.clone();
            async move {
                let result =
                    api::fetch_reset_credits_async(&client, &token, account_id.as_deref()).await;
                (*idx, result)
            }
        })
        .collect();

    for (idx, result) in futures::future::join_all(credit_futures).await {
        if let Ok(details) = result {
            rate_limited[idx].reset_credit_expiry = details
                .credits
                .iter()
                .filter(|c| c.is_available())
                .filter_map(|c| c.expires_at_timestamp())
                .min();
        }
    }

    for account in &mut accounts {
        if let Some(rate_limited) = rate_limited.iter().find(|row| row.alias == account.alias) {
            account.resets_next_expiry =
                codexctl::status_json::timestamp(rate_limited.reset_credit_expiry);
        }
    }

    Ok(StatusSnapshot {
        rate_limited,
        usage_based,
        accounts,
        fetched_at: chrono::Utc::now(),
    })
}

fn rate_limit_statuses(usage: &api::RateLimitResponse) -> Vec<LimitStatus> {
    let mut limits = Vec::new();
    if let Some(rate_limit) = &usage.rate_limit {
        limits.push(LimitStatus::from_rate_limit(
            "Codex".to_string(),
            rate_limit,
        ));
    } else {
        limits.push(LimitStatus::unavailable());
    }
    for additional in &usage.additional_rate_limits {
        let Some(rate_limit) = &additional.rate_limit else {
            continue;
        };
        let raw_name = additional
            .limit_name
            .as_deref()
            .or(additional.metered_feature.as_deref())
            .unwrap_or("Additional");
        let name: String = raw_name
            .trim()
            .chars()
            .filter(|character| !character.is_control())
            .take(80)
            .collect();
        limits.push(LimitStatus::from_rate_limit(
            if name.is_empty() {
                "Additional".to_string()
            } else {
                name
            },
            rate_limit,
        ));
    }
    limits
}

fn render_rate_limited_row(account: &RateLimitedAccount, columns: &RateLimitColumns) -> Vec<Cell> {
    if account.is_error {
        let mut row = vec![Cell::new(display_alias(&account.alias, account.is_active))];
        if columns.labeled {
            row.push(label_cell(account.label.as_deref()));
        }
        if columns.plan {
            row.push(Cell::new("-"));
        }
        if columns.named_limits {
            row.push(Cell::new("-"));
        }
        for _ in &columns.windows {
            row.extend([Cell::new("-"), Cell::new("-")]);
        }
        if columns.resets {
            row.push(Cell::new("-"));
        }
        if columns.pace {
            row.push(Cell::new("-"));
        }
        if columns.credits {
            row.push(Cell::new("-"));
        }
        row.push(token_cell(account.token_expiry, true, &account.error_msg));
        if columns.billing {
            row.push(Cell::new("-"));
        }
        return row;
    }

    let mut row = vec![Cell::new(display_alias(&account.alias, account.is_active))];
    if columns.labeled {
        row.push(label_cell(account.label.as_deref()));
    }
    if columns.plan {
        row.push(Cell::new(if account.plan.is_empty() {
            "-"
        } else {
            &account.plan
        }));
    }
    if columns.named_limits {
        row.push(Cell::new(
            account
                .limits
                .iter()
                .map(|limit| limit.name.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }
    for column in &columns.windows {
        let windows: Vec<_> = account
            .limits
            .iter()
            .map(|limit| {
                limit
                    .windows
                    .iter()
                    .find(|window| column.keys.contains(&window.key))
            })
            .collect();
        row.push(colorize_usage_lines(
            &windows
                .iter()
                .map(|window| window.map(|window| window.used_pct))
                .collect::<Vec<_>>(),
        ));
        row.push(Cell::new(
            windows
                .iter()
                .map(|window| window.map_or("-", |window| window.reset.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }
    if columns.resets {
        row.push(resets_cell(account));
    }
    if columns.pace {
        row.push(codexctl::status_format::pace_cell(
            account.pace.map(|pace| pace.points),
        ));
    }
    if columns.credits {
        row.push(credits_cell(account.credits.as_ref()));
    }
    row.push(token_cell(account.token_expiry, false, &account.error_msg));
    if columns.billing {
        row.push(if account.billing_unknown {
            Cell::new("unknown").fg(Color::Yellow)
        } else {
            Cell::new("rate-limited")
        });
    }
    row
}

fn credits_cell(credits: Option<&api::Credits>) -> Cell {
    Cell::new(codexctl::status_json::format_credits(credits))
}

/// The "Resets" column: banked rate-limit resets, and how many of them can be
/// redeemed right now. Green means `codexctl reset <alias>` would work this
/// second; red means a credit lapses within [`resets::EXPIRY_WARN_SECONDS`] and
/// would be lost unspent.
fn resets_cell(s: &RateLimitedAccount) -> Cell {
    if s.reset_credits <= 0 {
        return Cell::new("-");
    }
    if s.reset_credits_applicable > 0 {
        return Cell::new(format!(
            "{} ({} now)",
            s.reset_credits, s.reset_credits_applicable
        ))
        .fg(Color::Green);
    }
    let cell = Cell::new(s.reset_credits.to_string());
    match s.reset_credit_expiry {
        Some(expiry) if expiry - chrono::Utc::now().timestamp() <= resets::EXPIRY_WARN_SECONDS => {
            cell.fg(Color::Red)
        }
        _ => cell,
    }
}

fn render_usage_based_row(s: &UsageBasedAccount, labeled: bool, pace: bool) -> Vec<Cell> {
    let alias = display_alias(&s.alias, s.is_active);
    let mut row = vec![Cell::new(alias)];
    if labeled {
        row.push(label_cell(s.label.as_deref()));
    }

    if s.is_error {
        row.extend([
            Cell::new("-"),
            Cell::new("-"),
            Cell::new("-"),
            Cell::new("-"),
            token_cell(s.token_expiry, true, &s.error_msg),
        ]);
        if pace {
            row.push(Cell::new("-"));
        }
        return row;
    }

    let balance_str = s
        .credit_balance
        .as_deref()
        .map(codexctl::status_format::format_credit_balance)
        .unwrap_or_else(|| "-".to_string());

    let seat_limit_str = s
        .seat_limit_cents
        .map(|c| format!("${}", c / 100))
        .unwrap_or_else(|| "-".to_string());

    let (credits_str, credits_color) = match s.credits_status {
        CreditsStatus::Ok => ("ok", Color::Green),
        CreditsStatus::Unlimited => ("unlimited", Color::Cyan),
        CreditsStatus::None => ("none", Color::Red),
        CreditsStatus::Overage => ("overage", Color::Red),
    };

    let (spend_str, spend_color) = if s.spend_control_reached {
        ("limit", Color::Red)
    } else {
        ("ok", Color::Green)
    };

    row.extend([
        Cell::new(&balance_str),
        Cell::new(&seat_limit_str),
        Cell::new(credits_str).fg(credits_color),
        Cell::new(spend_str).fg(spend_color),
        token_cell(s.token_expiry, false, &s.error_msg),
    ]);
    if pace {
        row.push(codexctl::status_format::pace_cell(
            s.pace.map(|pace| pace.points),
        ));
    }
    row
}

/// The "Token" column: how long the stored access token is good for without a
/// re-login, or — for an errored row — what went wrong. An `invalidated` value
/// means the JWT still looks valid but OpenAI revoked the grant server-side (a
/// sibling seat was logged in), so the remaining lifetime would be misleading.
fn token_cell(token_expiry: Option<i64>, is_error: bool, error_msg: &str) -> Cell {
    if is_error {
        return Cell::new(error_msg).fg(Color::Red);
    }
    match token_expiry {
        None => Cell::new("-"),
        Some(exp) => {
            let diff = exp - chrono::Utc::now().timestamp();
            if diff <= 0 {
                return Cell::new("expired").fg(Color::Red);
            }
            let color = if diff >= 86400 {
                Color::Green
            } else if diff >= 3600 {
                Color::Yellow
            } else {
                Color::Red
            };
            Cell::new(format_duration(diff)).fg(color)
        }
    }
}

fn auth_failure_label(access_token: &str) -> &'static str {
    if api::is_token_expired(access_token) {
        "expired"
    } else {
        "invalidated"
    }
}

fn display_alias(alias: &str, is_active: bool) -> String {
    if is_active {
        format!("* {alias}")
    } else {
        alias.to_string()
    }
}

fn format_window_reset(window: Option<&api::RateLimitWindow>) -> String {
    codexctl::status_format::format_window_reset(
        window.and_then(api::RateLimitWindow::reset_timestamp),
    )
}

fn colorize_usage_lines(used_percent: &[Option<f64>]) -> Cell {
    let content = used_percent
        .iter()
        .map(|pct| pct.map_or_else(|| "-".to_string(), |pct| format!("{pct:.0}%")))
        .collect::<Vec<_>>()
        .join("\n");
    // A comfy-table cell has one foreground color. Leave multi-bucket cells
    // uncolored so a severe bucket does not falsely color a healthy one.
    if used_percent.len() != 1 {
        return Cell::new(content);
    }
    let Some(pct) = used_percent.iter().flatten().copied().reduce(f64::max) else {
        return Cell::new(content);
    };
    let color = if pct >= 80.0 {
        Color::Red
    } else if pct >= 50.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    Cell::new(content).fg(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";

    #[test]
    fn auth_failure_label_reports_invalidated_when_token_is_not_time_expired() {
        let token = format!("{JWT_HDR}.eyJleHAiOjk5OTk5OTk5OTl9.sig");

        assert_eq!(auth_failure_label(&token), "invalidated");
    }

    #[test]
    fn auth_failure_label_reports_expired_when_exp_claim_is_past() {
        let token = format!("{JWT_HDR}.eyJleHAiOjEwMDAwMDAwMDB9.sig");

        assert_eq!(auth_failure_label(&token), "expired");
    }

    fn rate_limited_account() -> RateLimitedAccount {
        RateLimitedAccount {
            alias: "amir+8@sawmills.ai".to_string(),
            label: None,
            limits: vec![LimitStatus {
                name: "Codex".to_string(),
                windows: vec![
                    WindowStatus {
                        key: WindowKey::Duration(5 * 60 * 60, 0),
                        label: "5h".to_string(),
                        used_pct: 10.0,
                        reset: "in 1h 00m".to_string(),
                    },
                    WindowStatus {
                        key: WindowKey::Duration(7 * 24 * 60 * 60, 0),
                        label: "7d".to_string(),
                        used_pct: 20.0,
                        reset: "in 1d 00h".to_string(),
                    },
                ],
                availability_score: 40.0,
            }],
            token_expiry: None,
            reset_credits: 0,
            reset_credits_applicable: 0,
            reset_credit_expiry: None,
            is_active: false,
            is_error: false,
            billing_unknown: false,
            plan: String::new(),
            credits: None,
            error_msg: String::new(),
            pace: None,
        }
    }

    #[test]
    fn rate_limited_status_snapshot_shows_pace_and_fleet_mean() {
        let usage = serde_json::from_value(serde_json::json!({
            "rate_limit": {"primary_window": {
                "used_percent": 12, "limit_window_seconds": 604800,
                "reset_at": 4102444800_i64
            }}
        }))
        .unwrap();
        let mut ahead = RateLimitedAccount::from_usage("ahead".into(), None, false, None, &usage);
        let behind_usage = serde_json::from_value(serde_json::json!({
            "rate_limit": {"primary_window": {
                "used_percent": 12, "limit_window_seconds": 604800,
                "reset_at": chrono::Utc::now().timestamp() + 302400
            }}
        }))
        .unwrap();
        let mut behind =
            RateLimitedAccount::from_usage("behind".into(), None, false, None, &behind_usage);
        let mut missing = rate_limited_account();
        missing.alias = "missing".into();
        for account in [&mut ahead, &mut behind, &mut missing] {
            for limit in &mut account.limits {
                for window in &mut limit.windows {
                    window.reset = "-".into();
                }
            }
        }
        let accounts = [&ahead, &behind, &missing];
        let table = rate_limited_table(&accounts);
        let rows: Vec<_> = accounts
            .iter()
            .map(|account| {
                let mut row =
                    codexctl::status_json::AccountStatus::local(&profile::Meta::default(), false);
                row.pace_points = account.pace.map(|pace| pace.points);
                row
            })
            .collect();
        assert_eq!(
            format!("{table}\n{}", fleet_pace_table(&rows).unwrap()),
            include_str!("../../tests/fixtures/status_pace.txt").trim_end()
        );
    }

    #[test]
    fn mixed_credit_usage_renders_returned_windows() {
        let usage: api::RateLimitResponse = serde_json::from_str(
            r#"{"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":96,"limit_window_seconds":604800}},"credits":{"has_credits":true},"rate_limit_reset_credits":{"available_count":3}}"#,
        ).unwrap();
        let account =
            RateLimitedAccount::from_usage("mixed".to_string(), None, false, None, &usage);
        let columns = RateLimitColumns::for_accounts(&[&account]);

        let row = render_rate_limited_row(&account, &columns);

        assert_eq!(row[1].content(), "Pro (More)");
        assert_eq!(row[2].content(), "96%");
        assert_eq!(row[4].content(), "3");
        assert!(!columns.billing);
    }

    #[test]
    fn pro_max_subscription_has_plan_and_live_windows() {
        let usage: api::RateLimitResponse = serde_json::from_str(
            r#"{"plan_type":"promax","rate_limit":{"primary_window":{"used_percent":29,"limit_window_seconds":604800}},"credits":{"has_credits":true}}"#,
        ).unwrap();
        let account = RateLimitedAccount::from_usage("p4".to_string(), None, false, None, &usage);
        let columns = RateLimitColumns::for_accounts(&[&account]);
        let row = render_rate_limited_row(&account, &columns);
        assert_eq!(row[1].content(), "Pro (Max)");
        assert_eq!(row[2].content(), "29%");
        assert_eq!(row.len(), columns.headers().len());
        assert!(!columns.billing);
    }

    #[test]
    fn absent_plan_hides_column_and_external_plan_text_is_bounded() {
        let mut usage: api::RateLimitResponse =
            serde_json::from_str(r#"{"rate_limit":{"primary_window":{"used_percent":29}}}"#)
                .unwrap();
        let missing =
            RateLimitedAccount::from_usage("missing".to_string(), None, false, None, &usage);
        assert!(!RateLimitColumns::for_accounts(&[&missing]).plan);
        usage.plan_type = Some(format!("\n{}\x1b\t", "x".repeat(100)));
        let external =
            RateLimitedAccount::from_usage("external".to_string(), None, false, None, &usage);
        let columns = RateLimitColumns::for_accounts(&[&missing, &external]);
        let row = render_rate_limited_row(&external, &columns);
        assert_eq!(row[1].content(), "x".repeat(80));
        assert_eq!(
            render_rate_limited_row(&missing, &columns)[1].content(),
            "-"
        );
        let error = RateLimitedAccount {
            is_error: true,
            error_msg: "bad auth".to_string(),
            ..rate_limited_account()
        };
        assert_eq!(
            render_rate_limited_row(&error, &columns).len(),
            columns.headers().len()
        );
    }

    #[test]
    fn unrecognized_plan_renders_returned_windows() {
        let usage: api::RateLimitResponse = serde_json::from_str(
            r#"{"plan_type":"new_plan","rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":604800}}}"#,
        ).unwrap();
        let account =
            RateLimitedAccount::from_usage("new-plan".to_string(), None, false, None, &usage);
        let columns = RateLimitColumns::for_accounts(&[&account]);

        let row = render_rate_limited_row(&account, &columns);

        assert_eq!(row[1].content(), "new_plan");
        assert_eq!(row[2].content(), "1%");
        assert_eq!(row.last().unwrap().content(), "unknown");
    }

    #[test]
    fn missing_usage_sorts_after_accounts_with_reported_windows() {
        let usage: api::RateLimitResponse =
            serde_json::from_str(r#"{"plan_type":"new_plan"}"#).unwrap();
        let missing =
            RateLimitedAccount::from_usage("missing".to_string(), None, false, None, &usage);
        let healthy = rate_limited_account();

        let missing_score = missing.availability_score();

        assert!(missing_score > healthy.availability_score());
    }

    #[test]
    fn additional_only_usage_keeps_its_name_and_sorts_last() {
        let usage: api::RateLimitResponse = serde_json::from_str(
            r#"{"plan_type":"new_plan","additional_rate_limits":[{"limit_name":"Reserve","rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":604800}}}]}"#,
        ).unwrap();
        let account = RateLimitedAccount::from_usage(
            "additional-only".to_string(),
            None,
            false,
            None,
            &usage,
        );
        let healthy = rate_limited_account();
        let columns = RateLimitColumns::for_accounts(&[&account]);

        let row = render_rate_limited_row(&account, &columns);

        assert_eq!(row[2].content(), "Codex\nReserve");
        assert_eq!(row[3].content(), "-\n1%");
        assert!(account.availability_score() > healthy.availability_score());
    }

    #[test]
    fn unknown_billing_keeps_live_usage_and_token_visible() {
        let account = RateLimitedAccount {
            billing_unknown: true,
            reset_credits: 4,
            token_expiry: Some(chrono::Utc::now().timestamp() + 10 * 86400),
            ..rate_limited_account()
        };
        let columns = RateLimitColumns::for_accounts(&[&account]);

        let row = render_rate_limited_row(&account, &columns);

        assert_eq!(columns.headers().last().unwrap(), "Billing");
        assert_eq!(row[1].content(), "10%");
        assert_eq!(row.last().unwrap().content(), "unknown");
    }

    #[test]
    fn unknown_billing_preserves_resets_and_token_expiry() {
        let expiry = chrono::Utc::now().timestamp() + 10 * 86400 + 3600 + 1800;
        let account = RateLimitedAccount {
            billing_unknown: true,
            reset_credits: 4,
            token_expiry: Some(expiry),
            ..rate_limited_account()
        };
        let columns = RateLimitColumns::for_accounts(&[&account]);

        let row = render_rate_limited_row(&account, &columns);

        assert_eq!(row[5].content(), "4");
        assert_eq!(row[6].content(), "10d 1h");
    }

    #[test]
    fn fetch_errors_stay_aligned_beside_unknown_billing() {
        let unknown = RateLimitedAccount {
            billing_unknown: true,
            ..rate_limited_account()
        };
        let failed = RateLimitedAccount {
            is_error: true,
            error_msg: "expired".to_string(),
            ..rate_limited_account()
        };
        let columns = RateLimitColumns::for_accounts(&[&unknown, &failed]);

        let row = render_rate_limited_row(&failed, &columns);

        assert_eq!(row.len(), columns.headers().len());
        assert_eq!(row[5].content(), failed.error_msg);
        assert_eq!(row.last().unwrap().content(), "-");
    }

    #[test]
    fn render_rate_limited_row_has_expected_column_count() {
        let account = rate_limited_account();
        let columns = RateLimitColumns::for_accounts(&[&account]);
        let row = render_rate_limited_row(&account, &columns);
        assert_eq!(columns.headers().len(), 6);
        assert_eq!(row.len(), 6);
    }

    #[test]
    fn credits_column_is_hidden_without_data_and_shows_compact_status_with_data() {
        let without = rate_limited_account();
        let columns = RateLimitColumns::for_accounts(&[&without]);
        assert!(!columns.headers().contains(&"Credits".to_string()));

        let usage: api::RateLimitResponse = serde_json::from_str(
            r#"{"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":1}},"credits":{"has_credits":true,"unlimited":false,"balance":"12.50","overage_limit_reached":true}}"#,
        )
        .unwrap();
        let with = RateLimitedAccount::from_usage("with-credits".into(), None, false, None, &usage);
        let columns = RateLimitColumns::for_accounts(&[&with]);
        let headers = columns.headers();
        let row = render_rate_limited_row(&with, &columns);
        let index = headers
            .iter()
            .position(|header| header == "Credits")
            .unwrap();
        assert_eq!(row[index].content(), "12.50 credits available overage");
    }

    #[test]
    fn credit_balance_uses_credit_units_in_text_and_keeps_raw_json() {
        let usage: api::RateLimitResponse = serde_json::from_value(serde_json::json!({
            "plan_type": "pro",
            "credits": {"has_credits": true, "balance": "53306.1594250000"}
        }))
        .unwrap();
        let account =
            RateLimitedAccount::from_usage("credit-units".into(), None, false, None, &usage);
        let columns = RateLimitColumns::for_accounts(&[&account]);
        let index = columns
            .headers()
            .iter()
            .position(|header| header == "Credits")
            .unwrap();
        assert_eq!(
            render_rate_limited_row(&account, &columns)[index].content(),
            "53,306.16 credits available"
        );
        let mut json_row =
            codexctl::status_json::AccountStatus::local(&profile::Meta::default(), false);
        json_row.set_usage(&usage);
        assert_eq!(
            serde_json::to_value(json_row).unwrap()["credits"]["balance"],
            "53306.1594250000"
        );
    }

    #[test]
    fn reported_zero_balance_keeps_the_credits_column_visible() {
        let usage: api::RateLimitResponse = serde_json::from_str(
            r#"{"plan_type":"pro","credits":{"has_credits":false,"unlimited":false,"balance":"0","overage_limit_reached":false}}"#,
        )
        .unwrap();
        let account =
            RateLimitedAccount::from_usage("empty-credits".into(), None, false, None, &usage);
        let columns = RateLimitColumns::for_accounts(&[&account]);
        let index = columns
            .headers()
            .iter()
            .position(|header| header == "Credits")
            .unwrap();
        assert_eq!(
            render_rate_limited_row(&account, &columns)[index].content(),
            "0.00 credits"
        );
    }

    #[test]
    fn credit_balance_text_is_bounded_and_cannot_control_the_terminal() {
        for balance in [
            "12.50\u{1b}[31m\n".to_string(),
            "x".repeat(1000),
            "1e100".into(),
        ] {
            let usage: api::RateLimitResponse = serde_json::from_value(serde_json::json!({
                "plan_type":"pro", "credits":{"has_credits":true,"balance":balance}
            }))
            .unwrap();
            let account =
                RateLimitedAccount::from_usage("credits".into(), None, false, None, &usage);
            let columns = RateLimitColumns::for_accounts(&[&account]);
            let index = columns
                .headers()
                .iter()
                .position(|header| header == "Credits")
                .unwrap();
            let row = render_rate_limited_row(&account, &columns);
            let cell = row[index].content();
            assert!(!cell.chars().any(char::is_control));
            assert!(cell.chars().count() <= 100);
            let mut json_row =
                codexctl::status_json::AccountStatus::local(&profile::Meta::default(), false);
            json_row.set_usage(&usage);
            assert_eq!(
                serde_json::to_value(json_row).unwrap()["credits"]["balance"],
                balance
            );
        }
    }

    #[test]
    fn reset_column_is_present_for_banked_credits_and_error_rows_match() {
        let account = RateLimitedAccount {
            reset_credits: 2,
            ..rate_limited_account()
        };
        let error = RateLimitedAccount {
            reset_credits: 2,
            is_error: true,
            ..rate_limited_account()
        };
        let columns = RateLimitColumns::for_accounts(&[&account, &error]);
        assert!(columns.resets);
        assert_eq!(columns.headers().last().unwrap(), "Token");
        assert_eq!(
            render_rate_limited_row(&account, &columns).len(),
            columns.headers().len()
        );
        assert_eq!(
            render_rate_limited_row(&error, &columns).len(),
            columns.headers().len()
        );
    }

    /// A store with no labels must render exactly the table it rendered before
    /// labels existed — no new column full of dashes.
    #[test]
    fn rate_limited_table_gains_a_label_column_only_when_labels_exist() {
        let bare = rate_limited_account();
        let columns = RateLimitColumns::for_accounts(&[&bare]);
        assert!(!columns.headers().contains(&"Label".to_string()));

        let labeled = RateLimitedAccount {
            label: Some("team".to_string()),
            ..rate_limited_account()
        };
        let columns = RateLimitColumns::for_accounts(&[&labeled]);
        let row = render_rate_limited_row(&labeled, &columns);

        assert_eq!(columns.headers()[1], "Label");
        assert_eq!(row[1].content(), "team");
        assert_eq!(row.len(), columns.headers().len());
    }

    /// The error row must stay aligned once the label column appears.
    #[test]
    fn rate_limited_error_row_keeps_its_width_with_labels() {
        let labeled = RateLimitedAccount {
            label: Some("team".to_string()),
            ..rate_limited_account()
        };
        let errored = RateLimitedAccount {
            is_error: true,
            error_msg: "expired".to_string(),
            label: Some("personal".to_string()),
            ..rate_limited_account()
        };

        let columns = RateLimitColumns::for_accounts(&[&labeled, &errored]);

        assert_eq!(
            render_rate_limited_row(&errored, &columns).len(),
            columns.headers().len()
        );
    }

    #[test]
    fn usage_based_table_gains_a_label_column_only_when_labels_exist() {
        let bare = usage_based_account(None);
        assert!(!usage_based_headers(&[&bare]).contains(&"Label".to_string()));

        let labeled = usage_based_account(Some("team"));
        let headers = usage_based_headers(&[&labeled]);

        assert_eq!(headers[1], "Label");
        assert_eq!(
            render_usage_based_row(&labeled, true, false).len(),
            headers.len()
        );
    }

    /// The error row must line up with the normal row or the table breaks.
    #[test]
    fn render_rate_limited_error_row_has_expected_column_count() {
        let account = RateLimitedAccount {
            is_error: true,
            error_msg: "expired".to_string(),
            ..rate_limited_account()
        };

        let columns = RateLimitColumns::for_accounts(&[&account]);
        let row = render_rate_limited_row(&account, &columns);
        assert_eq!(row.len(), columns.headers().len());
    }

    #[test]
    fn weekly_only_accounts_omit_empty_five_hour_columns() {
        let mut account = rate_limited_account();
        account.limits[0]
            .windows
            .retain(|window| window.label == "7d");
        account.limits[0].availability_score = 20.0;

        let columns = RateLimitColumns::for_accounts(&[&account]);

        assert_eq!(
            columns.headers(),
            vec!["Account", "7d", "7d Reset", "Token"]
        );
        assert_eq!(render_rate_limited_row(&account, &columns).len(), 4);
    }

    #[test]
    fn error_rows_do_not_add_hidden_limit_columns() {
        let mut healthy = rate_limited_account();
        healthy.limits[0]
            .windows
            .retain(|window| window.label == "7d");
        healthy.limits[0].availability_score = 20.0;
        let mut error = rate_limited_account();
        error.is_error = true;
        error.limits.push(LimitStatus {
            name: "Hidden".to_string(),
            windows: vec![WindowStatus {
                key: WindowKey::Duration(60 * 60, 0),
                label: "1h".to_string(),
                used_pct: 1.0,
                reset: "in 1h 00m".to_string(),
            }],
            availability_score: 2.0,
        });

        let columns = RateLimitColumns::for_accounts(&[&healthy, &error]);

        assert!(!columns.named_limits);
        assert_eq!(columns.windows.len(), 1);
        assert_eq!(columns.windows[0].label, "7d");
    }

    #[test]
    fn named_additional_limits_render_in_one_account_row() {
        let mut account = rate_limited_account();
        account.limits[0]
            .windows
            .retain(|window| window.label == "7d");
        account.limits[0].availability_score = 20.0;
        account.limits.push(LimitStatus {
            name: "GPT-5.3-Codex-Spark".to_string(),
            windows: vec![WindowStatus {
                key: WindowKey::Duration(7 * 24 * 60 * 60, 0),
                label: "7d".to_string(),
                used_pct: 0.0,
                reset: "in 6d 00h".to_string(),
            }],
            availability_score: 0.0,
        });

        let columns = RateLimitColumns::for_accounts(&[&account]);
        let row = render_rate_limited_row(&account, &columns);

        assert!(columns.named_limits);
        assert_eq!(row.len(), columns.headers().len());
        assert_eq!(row[0].content(), "amir+8@sawmills.ai");
        assert_eq!(row[1].content(), "Codex\nGPT-5.3-Codex-Spark");
        assert_eq!(row[2].content(), "20%\n0%");
        assert_eq!(row[3].content(), "in 1d 00h\nin 6d 00h");
    }

    #[test]
    fn server_declared_durations_drive_window_headers() {
        let rate_limit: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 25, "window_minutes": 15},
                "secondary_window": {"used_percent": 42, "window_minutes": 60}
            }"#,
        )
        .unwrap();
        let mut account = rate_limited_account();
        account.limits = vec![LimitStatus::from_rate_limit(
            "Codex".to_string(),
            &rate_limit,
        )];

        let columns = RateLimitColumns::for_accounts(&[&account]);

        assert_eq!(
            columns.headers(),
            vec!["Account", "15m", "15m Reset", "1h", "1h Reset", "Token",]
        );
        assert_eq!(render_rate_limited_row(&account, &columns).len(), 6);
    }

    #[test]
    fn equal_duration_windows_remain_separate() {
        let rate_limit: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 25, "window_minutes": 60},
                "secondary_window": {"used_percent": 42, "window_minutes": 60}
            }"#,
        )
        .unwrap();
        let mut account = rate_limited_account();
        account.limits = vec![LimitStatus::from_rate_limit(
            "Codex".to_string(),
            &rate_limit,
        )];

        let columns = RateLimitColumns::for_accounts(&[&account]);
        let row = render_rate_limited_row(&account, &columns);

        assert_eq!(
            columns.headers(),
            vec!["Account", "1h", "1h Reset", "1h #2", "1h #2 Reset", "Token",]
        );
        assert_eq!(row[1].content(), "25%");
        assert_eq!(row[3].content(), "42%");
    }

    #[test]
    fn durationless_legacy_windows_align_with_declared_fleet_columns() {
        let declared: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 10, "window_minutes": 300},
                "secondary_window": {"used_percent": 20, "window_minutes": 10080}
            }"#,
        )
        .unwrap();
        let legacy: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 30},
                "secondary_window": {"used_percent": 40}
            }"#,
        )
        .unwrap();
        let mut declared_account = rate_limited_account();
        declared_account.limits =
            vec![LimitStatus::from_rate_limit("Codex".to_string(), &declared)];
        let mut legacy_account = rate_limited_account();
        legacy_account.alias = "legacy@sawmills.ai".to_string();
        legacy_account.limits = vec![LimitStatus::from_rate_limit("Codex".to_string(), &legacy)];

        let columns = RateLimitColumns::for_accounts(&[&declared_account, &legacy_account]);
        let legacy_row = render_rate_limited_row(&legacy_account, &columns);

        assert_eq!(
            columns.headers(),
            vec!["Account", "5h", "5h Reset", "7d", "7d Reset", "Token"]
        );
        assert_eq!(legacy_row[1].content(), "30%");
        assert_eq!(legacy_row[3].content(), "40%");
    }

    #[test]
    fn legacy_secondary_alignment_survives_primary_column_insertion() {
        let declared: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 10, "window_minutes": 300},
                "secondary_window": {"used_percent": 20, "window_minutes": 10080}
            }"#,
        )
        .unwrap();
        let conflicting_primary: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 30},
                "secondary_window": {"used_percent": 40, "window_minutes": 300}
            }"#,
        )
        .unwrap();
        let legacy_secondary: api::RateLimit =
            serde_json::from_str(r#"{"secondary_window": {"used_percent": 50}}"#).unwrap();
        let mut declared_account = rate_limited_account();
        declared_account.limits =
            vec![LimitStatus::from_rate_limit("Codex".to_string(), &declared)];
        let mut conflicting_account = rate_limited_account();
        conflicting_account.alias = "conflict@sawmills.ai".to_string();
        conflicting_account.limits = vec![LimitStatus::from_rate_limit(
            "Codex".to_string(),
            &conflicting_primary,
        )];
        let mut secondary_account = rate_limited_account();
        secondary_account.alias = "secondary@sawmills.ai".to_string();
        secondary_account.limits = vec![LimitStatus::from_rate_limit(
            "Codex".to_string(),
            &legacy_secondary,
        )];

        let columns = RateLimitColumns::for_accounts(&[
            &declared_account,
            &conflicting_account,
            &secondary_account,
        ]);
        let secondary_row = render_rate_limited_row(&secondary_account, &columns);

        assert_eq!(
            columns.headers(),
            vec![
                "Account",
                "Primary",
                "Primary Reset",
                "5h",
                "5h Reset",
                "7d",
                "7d Reset",
                "Token",
            ]
        );
        assert_eq!(secondary_row[5].content(), "50%");
    }

    #[test]
    fn weekly_only_declared_window_does_not_absorb_legacy_primary() {
        let declared: api::RateLimit = serde_json::from_str(
            r#"{"primary_window": {"used_percent": 20, "window_minutes": 10080}}"#,
        )
        .unwrap();
        let legacy: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 30},
                "secondary_window": {"used_percent": 40}
            }"#,
        )
        .unwrap();
        let mut declared_account = rate_limited_account();
        declared_account.limits =
            vec![LimitStatus::from_rate_limit("Codex".to_string(), &declared)];
        let mut legacy_account = rate_limited_account();
        legacy_account.alias = "legacy@sawmills.ai".to_string();
        legacy_account.limits = vec![LimitStatus::from_rate_limit("Codex".to_string(), &legacy)];

        let columns = RateLimitColumns::for_accounts(&[&declared_account, &legacy_account]);
        let legacy_row = render_rate_limited_row(&legacy_account, &columns);

        assert_eq!(
            columns.headers(),
            vec![
                "Account",
                "Primary",
                "Primary Reset",
                "7d",
                "7d Reset",
                "Secondary",
                "Secondary Reset",
                "Token",
            ]
        );
        assert_eq!(legacy_row[1].content(), "30%");
        assert_eq!(legacy_row[5].content(), "40%");
    }

    #[test]
    fn arbitrary_declared_pair_does_not_absorb_legacy_windows() {
        let declared: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 10, "window_minutes": 15},
                "secondary_window": {"used_percent": 20, "window_minutes": 60}
            }"#,
        )
        .unwrap();
        let legacy: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 30},
                "secondary_window": {"used_percent": 40}
            }"#,
        )
        .unwrap();
        let mut declared_account = rate_limited_account();
        declared_account.limits =
            vec![LimitStatus::from_rate_limit("Codex".to_string(), &declared)];
        let mut legacy_account = rate_limited_account();
        legacy_account.alias = "legacy@sawmills.ai".to_string();
        legacy_account.limits = vec![LimitStatus::from_rate_limit("Codex".to_string(), &legacy)];

        let columns = RateLimitColumns::for_accounts(&[&declared_account, &legacy_account]);

        assert_eq!(
            columns.headers(),
            vec![
                "Account",
                "Primary",
                "Primary Reset",
                "15m",
                "15m Reset",
                "1h",
                "1h Reset",
                "Secondary",
                "Secondary Reset",
                "Token",
            ]
        );
    }

    #[test]
    fn ambiguous_legacy_windows_do_not_alias_into_large_declared_fleet() {
        let fast: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 10, "window_minutes": 15},
                "secondary_window": {"used_percent": 20, "window_minutes": 60}
            }"#,
        )
        .unwrap();
        let standard: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 30, "window_minutes": 300},
                "secondary_window": {"used_percent": 40, "window_minutes": 10080}
            }"#,
        )
        .unwrap();
        let legacy: api::RateLimit = serde_json::from_str(
            r#"{
                "primary_window": {"used_percent": 50},
                "secondary_window": {"used_percent": 60}
            }"#,
        )
        .unwrap();
        let mut fast_account = rate_limited_account();
        fast_account.limits = vec![LimitStatus::from_rate_limit("Codex".to_string(), &fast)];
        let mut standard_account = rate_limited_account();
        standard_account.alias = "standard@sawmills.ai".to_string();
        standard_account.limits =
            vec![LimitStatus::from_rate_limit("Codex".to_string(), &standard)];
        let mut legacy_account = rate_limited_account();
        legacy_account.alias = "legacy@sawmills.ai".to_string();
        legacy_account.limits = vec![LimitStatus::from_rate_limit("Codex".to_string(), &legacy)];

        let columns =
            RateLimitColumns::for_accounts(&[&fast_account, &standard_account, &legacy_account]);

        assert_eq!(
            columns.headers(),
            vec![
                "Account",
                "Primary",
                "Primary Reset",
                "15m",
                "15m Reset",
                "1h",
                "1h Reset",
                "5h",
                "5h Reset",
                "7d",
                "7d Reset",
                "Secondary",
                "Secondary Reset",
                "Token",
            ]
        );
    }

    #[test]
    fn durationless_secondary_only_window_keeps_its_slot_label() {
        let rate_limit: api::RateLimit =
            serde_json::from_str(r#"{"secondary_window": {"used_percent": 42}}"#).unwrap();
        let mut account = rate_limited_account();
        account.limits = vec![LimitStatus::from_rate_limit(
            "Codex".to_string(),
            &rate_limit,
        )];

        let columns = RateLimitColumns::for_accounts(&[&account]);

        assert_eq!(
            render_rate_limited_row(&account, &columns).len(),
            columns.headers().len()
        );
    }

    #[test]
    fn unavailable_usage_is_not_colored_green() {
        let cell = colorize_usage_lines(&[None]);
        assert_eq!(cell, Cell::new("-"));
    }

    #[test]
    fn mixed_severity_bucket_lines_do_not_share_one_color() {
        let cell = colorize_usage_lines(&[Some(100.0), Some(0.0)]);

        assert_eq!(cell, Cell::new("100%\n0%"));
    }

    #[test]
    fn resets_column_is_blank_without_banked_credits() {
        assert_eq!(resets_cell(&rate_limited_account()).content(), "-");
    }

    #[test]
    fn resets_column_calls_out_credits_redeemable_now() {
        let account = RateLimitedAccount {
            reset_credits: 3,
            reset_credits_applicable: 2,
            ..rate_limited_account()
        };

        assert_eq!(resets_cell(&account).content(), "3 (2 now)");
    }

    /// Held but not yet applicable: the count alone, since a reset only clears
    /// an already-exhausted window.
    #[test]
    fn resets_column_shows_the_bare_count_when_nothing_applies_yet() {
        let account = RateLimitedAccount {
            reset_credits: 3,
            reset_credits_applicable: 0,
            ..rate_limited_account()
        };

        assert_eq!(resets_cell(&account).content(), "3");
    }

    fn usage_based_account(label: Option<&str>) -> UsageBasedAccount {
        UsageBasedAccount {
            alias: "amir+11@sawmills.ai".to_string(),
            label: label.map(str::to_string),
            credit_balance: Some("10.00".to_string()),
            seat_limit_cents: Some(2000),
            credits_status: CreditsStatus::Ok,
            spend_control_reached: false,
            token_expiry: None,
            is_active: false,
            is_error: false,
            error_msg: String::new(),
            pace: None,
        }
    }

    #[test]
    fn render_usage_based_row_has_expected_column_count() {
        let account = usage_based_account(None);

        let row = render_usage_based_row(&account, false, false);
        assert_eq!(row.len(), 6);
        assert_eq!(row[1].content(), "10.00 credits");
        assert_eq!(row[2].content(), "$20");
        assert_eq!(usage_based_headers(&[&account])[1], "Credit balance");
    }

    #[test]
    fn usage_based_weekly_status_shows_pace_and_contributes_to_fleet() {
        let usage = serde_json::from_value(serde_json::json!({
            "plan_type": "self_serve_business_usage_based",
            "rate_limit": {"primary_window": {
                "used_percent": 28, "limit_window_seconds": 604800,
                "reset_at": 4102444800_i64
            }}
        }))
        .unwrap();
        let account = UsageBasedAccount::from_usage("metered".into(), None, false, None, &usage);
        let table = usage_based_table(&[&account]);
        assert!(table.to_string().contains("Pace") && table.to_string().contains("+28 ahead"));
        let mut known =
            codexctl::status_json::AccountStatus::local(&profile::Meta::default(), false);
        known.set_usage(&usage);
        let mut subscription =
            codexctl::status_json::AccountStatus::local(&profile::Meta::default(), false);
        subscription.pace_points = Some(12.0);
        let missing = codexctl::status_json::AccountStatus::local(&profile::Meta::default(), false);
        let table = fleet_pace_table(&[subscription, known, missing]).unwrap();
        assert!(table.to_string().contains("Fleet") && table.to_string().contains("+20 ahead"));
        assert!(
            !usage_based_table(&[&usage_based_account(None)])
                .to_string()
                .contains("Pace")
        );
    }
}
