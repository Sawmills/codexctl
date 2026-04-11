# Status Command: Usage-Based Account Support

## Problem

`codexctl status` displays a single table with columns designed for rate-limited accounts (5h Used, 5h Reset, 7d Used, 7d Reset). Usage-based accounts (`self_serve_business_usage_based`) have no rate limits — they use a credit/spending model. These accounts currently show dashes in every column, adding noise without value.

## Design

Split the status output into two tables, one per billing model. Each table has columns meaningful to its account type.

### Table 1: Rate-Limited Accounts

No changes to the existing table. Shown when at least one rate-limited account exists.

```
Rate-Limited Accounts
┌───────────────────┬──────┬─────────┬──────────┬─────────┬──────────┬────────┐
│ Account           ┆ Plan ┆ 5h Used ┆ 5h Reset ┆ 7d Used ┆ 7d Reset ┆ Active │
╞═══════════════════╪══════╪═════════╪══════════╪═════════╪══════════╪════════╡
│ amir+7@sawmills.ai┆ team ┆ 2%      ┆ in 4h 59m┆ 16%     ┆ in 6d 1h ┆ *      │
└───────────────────┴──────┴─────────┴──────────┴─────────┴──────────┴────────┘
```

### Table 2: Usage-Based Accounts

Shown when at least one usage-based account exists. Displayed after the rate-limited table with a blank line separator.

```
Usage-Based Accounts
┌──────────────────────────┬───────────┬─────────┬────────────┬─────────┬──────────┬────────┐
│ Account                  ┆ Plan      ┆ Balance ┆ Seat Limit ┆ Credits ┆ Spending ┆ Active │
╞══════════════════════════╪═══════════╪═════════╪════════════╪═════════╪══════════╪════════╡
│ amir+ezra@sawmills.ai   ┆ biz_usage ┆ -       ┆ $200       ┆ ok      ┆ ok       ┆        │
└──────────────────────────┴───────────┴─────────┴────────────┴─────────┴──────────┴────────┘
```

**Columns:**

| Column | Source | Format |
|--------|--------|--------|
| Account | profile alias | string |
| Plan | `plan_type` | shortened display name (e.g. `biz_usage`) |
| Balance | `credits.balance` | dollar amount when populated, `-` when null |
| Seat Limit | `seat_type_credit_limits.usage_based[0].limit` | dollar amount (value / 100), `-` if unavailable |
| Credits | `credits.has_credits`, `credits.overage_limit_reached`, `credits.unlimited` | green "ok" / blue "unlimited" / red "none" / red "overage" |
| Spending | `spend_control.reached` | green "ok" / red "limit" |
| Active | active profile marker | `*` or empty |

### Detecting Account Type

An account is "usage-based" when:
- `rate_limit` is `null` AND `credits` is present with `has_credits: true`, OR
- `plan_type` contains `usage_based`

All other accounts with rate limit windows are "rate-limited".

Error accounts (bad auth, expired tokens) appear in whichever table matches their last known plan type from `meta.json`. If no plan is known, they appear in the rate-limited table (legacy default).

### CLI Flags

| Flag | Behavior |
|------|----------|
| (none) | Show both tables |
| `--rate-limited` | Show only rate-limited table |
| `--usage-based` | Show only usage-based table |

Flags are mutually exclusive.

### Data Model Changes

**`api.rs` — `RateLimitResponse`**: Add `credits` and `spend_control` fields.

```rust
pub struct RateLimitResponse {
    pub plan_type: Option<String>,
    pub rate_limit: Option<RateLimit>,
    pub credits: Option<Credits>,
    pub spend_control: Option<SpendControl>,
}

pub struct Credits {
    pub has_credits: bool,
    pub unlimited: bool,
    pub overage_limit_reached: bool,
    pub balance: Option<String>,
}

pub struct SpendControl {
    pub reached: bool,
}
```

**`api.rs` — new `fetch_account_settings_async`**: Fetch `accounts/{account_id}/settings` to get seat credit limits. The `account_id` comes from the `accounts/check` endpoint or can be extracted from the JWT claims. Since the account_id is shared across all accounts in the same workspace, fetch it once and reuse.

```rust
pub struct AccountSettings {
    pub seat_type_credit_limits: Option<SeatTypeCreditLimits>,
}

pub struct SeatTypeCreditLimits {
    pub usage_based: Option<Vec<CreditLimit>>,
}

pub struct CreditLimit {
    pub enforcement_mode: String,
    pub limit: u64, // cents
}
```

**`status.rs` — `AccountStatus`**: Add fields for credit-based data.

```rust
struct AccountStatus {
    // ... existing fields ...
    credits_status: Option<CreditsStatus>,
    spend_control_reached: Option<bool>,
    credit_balance: Option<String>,
    seat_limit_cents: Option<u64>,
    is_usage_based: bool,
}

enum CreditsStatus {
    Ok,
    Unlimited,
    None,
    Overage,
}
```

### Fetch Flow

1. Fetch `wham/usage` for all accounts in parallel (existing behavior).
2. For accounts identified as usage-based, fetch `accounts/{account_id}/settings` (once per unique account_id) to get seat limits.
3. Split results into rate-limited and usage-based groups.
4. Sort each group by availability/health score.
5. Render each group as its own table.

### Sorting for Usage-Based Accounts

Lower score = healthier. Used for display ordering.

| Condition | Score |
|-----------|-------|
| Credits ok, spending ok | 0 |
| Credits ok, spend limit reached | 100 |
| Credits unlimited | 0 |
| Overage limit reached | 200 |
| No credits | 300 |
| Error/expired | 1000 |

### Plan Display Names

Shorten long plan type strings for the table:

| API value | Display |
|-----------|---------|
| `self_serve_business_usage_based` | `biz_usage` |
| `enterprise_cbp_usage_based` | `ent_usage` |
| `team` | `team` |
| Other | as-is |
