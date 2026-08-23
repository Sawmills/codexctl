use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};

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
    error_msg: String,
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
}

impl RateLimitedAccount {
    fn availability_score(&self) -> f64 {
        if self.is_error {
            return 1000.0;
        }
        let Some(main) = self.limits.first() else {
            return 1000.0;
        };
        main.availability_score
    }
}

impl UsageBasedAccount {
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

pub fn run(filter: Filter) -> Result<()> {
    let (rate_limited, usage_based, fetched_at) = load_sorted_statuses()?;

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

    Ok(())
}

pub fn run_focused(focused_alias: &str) -> Result<()> {
    let (rate_limited, usage_based, fetched_at) = load_sorted_statuses()?;
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

    Ok(())
}

fn load_sorted_statuses() -> Result<(
    Vec<RateLimitedAccount>,
    Vec<UsageBasedAccount>,
    chrono::DateTime<chrono::Utc>,
)> {
    let profiles = profile::list_profiles()?;
    let fetched_at = chrono::Utc::now();
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok((Vec::new(), Vec::new(), fetched_at));
    }

    let paths = config::default_paths()?;
    let active = profile::get_active_from(&paths)?;

    let rt = tokio::runtime::Runtime::new()?;
    let (mut rate_limited, mut usage_based) =
        rt.block_on(fetch_and_split(&profiles, &active, &paths))?;

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

    Ok((rate_limited, usage_based, fetched_at))
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
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    let columns = RateLimitColumns::for_accounts(accounts);
    table.set_header(columns.headers());
    for account in accounts {
        table.add_row(render_rate_limited_row(account, &columns));
    }
    println!("{table}");
    true
}

struct RateLimitColumns {
    named_limits: bool,
    labeled: bool,
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
        if self.named_limits {
            headers.push("Limit".to_string());
        }
        for window in &self.windows {
            headers.push(window.label.clone());
            headers.push(format!("{} Reset", window.label));
        }
        headers.extend(["Resets".to_string(), "Token".to_string()]);
        headers
    }
}

fn print_usage_based_table(title: &str, accounts: &[&UsageBasedAccount]) -> bool {
    if accounts.is_empty() {
        return false;
    }

    println!("{title}");
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    let headers = usage_based_headers(accounts);
    let labeled = headers.get(1).is_some_and(|header| header == "Label");
    table.set_header(headers);
    for account in accounts {
        table.add_row(render_usage_based_row(account, labeled));
    }
    println!("{table}");
    true
}

fn usage_based_headers(accounts: &[&UsageBasedAccount]) -> Vec<String> {
    let mut headers = vec!["Account".to_string()];
    if accounts.iter().any(|account| account.label.is_some()) {
        headers.push("Label".to_string());
    }
    headers.extend(
        ["Balance", "Seat", "Credits", "Spend", "Token"]
            .into_iter()
            .map(str::to_string),
    );
    headers
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
) -> Result<(Vec<RateLimitedAccount>, Vec<UsageBasedAccount>)> {
    let client = api::http_client()?;

    // Phase 1: fetch wham/usage for all accounts in parallel
    let futures: Vec<_> = profiles
        .iter()
        .map(|p| {
            let client = client.clone();
            let alias = p.meta.alias.clone();
            let label = p.meta.label.clone();
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
                (alias, label, plan_from_meta, is_active, auth, usage_result)
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;

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
            let credits = &usage.credits;
            let credits_status = match credits {
                Some(c) if c.unlimited => CreditsStatus::Unlimited,
                Some(c) if c.overage_limit_reached => CreditsStatus::Overage,
                Some(c) if c.has_credits => CreditsStatus::Ok,
                _ => CreditsStatus::None,
            };
            let credit_balance = credits.as_ref().and_then(|c| c.balance.clone());
            let spend_control_reached = usage.spend_control.as_ref().is_some_and(|sc| sc.reached);

            let idx = usage_based.len();
            usage_based.push(UsageBasedAccount {
                alias: alias.clone(),
                label: label.clone(),
                credit_balance,
                seat_limit_cents: None,
                credits_status,
                spend_control_reached,
                token_expiry,
                is_active: *is_active,
                is_error: false,
                error_msg: String::new(),
            });

            if let Some(account_id) =
                account_id.or_else(|| api::extract_account_id(&auth.access_token))
            {
                ub_needing_settings.push((idx, auth.access_token.clone(), account_id));
            }
        } else {
            let is_unknown = billing_class == api::BillingClass::Unknown;

            let idx = rate_limited.len();
            rate_limited.push(RateLimitedAccount {
                alias: alias.clone(),
                label: label.clone(),
                limits: rate_limit_statuses(usage),
                token_expiry,
                reset_credits: usage.reset_credits_available(),
                reset_credits_applicable: usage.reset_credits_applicable(),
                reset_credit_expiry: None,
                is_active: *is_active,
                is_error: is_unknown,
                error_msg: if is_unknown {
                    "unknown billing".to_string()
                } else {
                    String::new()
                },
            });

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

    Ok((rate_limited, usage_based))
}

fn rate_limit_statuses(usage: &api::RateLimitResponse) -> Vec<LimitStatus> {
    let mut limits = Vec::new();
    if let Some(rate_limit) = &usage.rate_limit {
        limits.push(LimitStatus::from_rate_limit(
            "Codex".to_string(),
            rate_limit,
        ));
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
    if limits.is_empty() {
        limits.push(LimitStatus::unavailable());
    }
    limits
}

fn render_rate_limited_row(account: &RateLimitedAccount, columns: &RateLimitColumns) -> Vec<Cell> {
    if account.is_error {
        let mut row = vec![Cell::new(display_alias(&account.alias, account.is_active))];
        if columns.labeled {
            row.push(label_cell(account.label.as_deref()));
        }
        if columns.named_limits {
            row.push(Cell::new("-"));
        }
        for _ in &columns.windows {
            row.extend([Cell::new("-"), Cell::new("-")]);
        }
        row.push(Cell::new("-"));
        row.push(token_cell(account.token_expiry, true, &account.error_msg));
        return row;
    }

    let mut row = vec![Cell::new(display_alias(&account.alias, account.is_active))];
    if columns.labeled {
        row.push(label_cell(account.label.as_deref()));
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
    row.push(resets_cell(account));
    row.push(token_cell(account.token_expiry, false, &account.error_msg));
    row
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

fn render_usage_based_row(s: &UsageBasedAccount, labeled: bool) -> Vec<Cell> {
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
        return row;
    }

    let balance_str = s
        .credit_balance
        .as_deref()
        .map(|b| format!("${b}"))
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
    match window {
        Some(w) => match w.reset_timestamp() {
            Some(reset_ts) => {
                let now = chrono::Utc::now().timestamp();
                let diff_secs = reset_ts - now;
                if diff_secs <= 0 {
                    "now".to_string()
                } else if diff_secs >= 86400 {
                    format!(
                        "in {} ({})",
                        format_duration(diff_secs),
                        format_reset_timestamp(reset_ts)
                    )
                } else {
                    format!("in {}", format_duration(diff_secs))
                }
            }
            None => "-".to_string(),
        },
        None => "-".to_string(),
    }
}

fn format_reset_timestamp(reset_ts: i64) -> String {
    chrono::DateTime::from_timestamp(reset_ts, 0)
        .map(|dt| {
            let local = dt.with_timezone(&chrono::Local);
            local.format("%a %b %d %H:%M").to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

fn format_duration(secs: i64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m")
    }
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
            error_msg: String::new(),
        }
    }

    #[test]
    fn render_rate_limited_row_has_expected_column_count() {
        let account = rate_limited_account();
        let columns = RateLimitColumns::for_accounts(&[&account]);
        let row = render_rate_limited_row(&account, &columns);
        assert_eq!(columns.headers().len(), 7);
        assert_eq!(row.len(), 7);
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
        assert_eq!(render_usage_based_row(&labeled, true).len(), headers.len());
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
            vec!["Account", "7d", "7d Reset", "Resets", "Token"]
        );
        assert_eq!(render_rate_limited_row(&account, &columns).len(), 5);
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
            vec![
                "Account",
                "15m",
                "15m Reset",
                "1h",
                "1h Reset",
                "Resets",
                "Token",
            ]
        );
        assert_eq!(render_rate_limited_row(&account, &columns).len(), 7);
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
            vec![
                "Account",
                "1h",
                "1h Reset",
                "1h #2",
                "1h #2 Reset",
                "Resets",
                "Token",
            ]
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
            vec![
                "Account", "5h", "5h Reset", "7d", "7d Reset", "Resets", "Token"
            ]
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
                "Resets",
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
                "Resets",
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
                "Resets",
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
                "Resets",
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
            columns.headers(),
            vec!["Account", "Secondary", "Secondary Reset", "Resets", "Token"]
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
        }
    }

    #[test]
    fn render_usage_based_row_has_expected_column_count() {
        let account = usage_based_account(None);

        assert_eq!(render_usage_based_row(&account, false).len(), 6);
    }
}
