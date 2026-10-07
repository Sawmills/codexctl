# Account Status JSON

`codexctl status --json` prints one JSON document to stdout.
Automation can read account usage without parsing terminal tables.
`codexctl list --json` uses the same schema.

```sh
codexctl status --json | jq .
codexctl status --json | jq '.accounts[] | {alias, source, secondary_used_percent, secondary_window_seconds, secondary_resets_at, error}'
codexctl status --json --rate-limited
codexctl list --json
```

## Schema Version 2

```json
{
  "version": 2,
  "fleet_pace_points": -13.0,
  "accounts": [
    {
      "alias": "personal",
      "label": "Personal",
      "plan": "pro",
      "source": "server",
      "state": "server",
      "primary_used_percent": 12.5,
      "secondary_used_percent": 37.0,
      "pace_points": -13.0,
      "elapsed_percent": 50.0,
      "primary_window_seconds": 18000,
      "secondary_window_seconds": 604800,
      "primary_resets_at": "2099-12-25T05:00:00Z",
      "secondary_resets_at": "2100-01-01T00:00:00Z",
      "resets_at": "2100-01-01T00:00:00Z",
      "resets_banked": 2,
      "resets_redeemable": 1,
      "resets_next_expiry": "2099-12-31T00:00:00Z",
      "billing_class": "rate_limited",
      "credits": {
        "has_credits": true,
        "unlimited": false,
        "balance": "12.50",
        "overage_limit_reached": false
      },
      "error": null,
      "usage_age_seconds": 12,
      "usage_stale": false
    }
  ]
}
```

Every listed field except `credits` is present in every account row.
Unknown values are `null`, except `billing_class`, which uses `unknown`.
The optional `credits` object is omitted when upstream credits data is absent or null.
An empty result is `{"version":2,"accounts":[],"fleet_pace_points":null}`.

- `alias`: The profile or server account alias.
- `label`: The display label, or `null`.
- `plan`: The upstream plan identifier, with saved profile metadata as a fallback, or `null`.
- `source`: `local` for a profile or `server` for a server account.
- `state`: `local`, `server`, `active`, or `unavailable`.
  Connected profiles retain the table's `local` state, including rows with errors.
  A failed standalone status row uses `unavailable`.
  `active` marks the active profile in standalone mode or the active server account in connected mode.
- `primary_used_percent`: The main Codex short-window usage percentage, or `null`.
- `secondary_used_percent`: The main Codex long-window usage percentage, or `null`.
  This window is normally weekly.
  Window duration determines short and long placement, including a weekly-only primary window.
- `pace_points`: Weekly used percentage minus elapsed percentage, or `null` when pace is unknown.
  Positive points mean ahead of pace: usage is faster than a straight line through the window.
  Negative points mean behind pace.
- `elapsed_percent`: Percentage of the weekly window that has elapsed, or `null` when pace is unknown.
  It is `(window_seconds - seconds_until_reset) / window_seconds * 100`, bounded to 0 through 100.
  A reported reset more than one window away gives 0 elapsed; the reset instant gives 100.
  After that instant, the observation is expired and both pace fields are `null`.
  Only a declared seven-day (604800-second) main Codex long window qualifies.
  Missing usage, duration, or reset time, invalid usage percentages, stale usage, and failed usage give `null` for both fields.
  Pace uses the current CLI clock and adds no requests.
- `primary_window_seconds`, `secondary_window_seconds`: Declared duration in seconds of the corresponding short or long window, or `null` when unknown.
  Durations use the same upstream evidence as the statusline; they are never inferred from plan, position, or reset time.
  Upstream minutes are converted to seconds. A weekly-only primary window is placed in the secondary fields, alongside its percentage and reset time.
- `primary_resets_at`, `secondary_resets_at`: Each window's reset time in RFC 3339 UTC format, or `null` when unknown.
- `resets_at`: The long window's reset time in RFC 3339 UTC format, or `null`.
  This existing field remains an alias of `secondary_resets_at`.
- `resets_banked`: Number of banked resets held by the account, or `null` when usage is unavailable.
- `resets_redeemable`: Number of banked resets that may be redeemed now, or `null` when usage is unavailable.
- `resets_next_expiry`: RFC 3339 UTC expiry of the soonest redeemable banked reset, or `null` when no credit listing is available or no credit can be redeemed.
- `billing_class`: `rate_limited`, `usage_based`, or `unknown`.
  Failed requests keep unknown billing, regardless of the table's display group.
- `credits`: The reported credits object, when available.
  `has_credits`, `unlimited`, and `overage_limit_reached` are booleans.
  `balance` is the upstream OpenAI credit balance string, or `null` when unknown.
  The unit is credits, not dollars. JSON keeps the raw value, such as
  `"53306.1594250000"`. Text tables and the dashboard display it as `53,306.16 credits`.
  Reported zero balances and false flags remain present.
  Older account servers omit credits; upgrade the server to expose them.
- `error`: A brief account error, or `null`.
  Server accounts marked unavailable use `account unavailable` because the catalog supplies no detailed cause.

- `usage_age_seconds`: Age of the last successful server usage observation, or `null` when unknown or local.
- `usage_stale`: Whether server usage is stale or missing, or `null` for local rows.
  Stale server rows retain the last usage values and include an `error`; consumers must treat their quota as unknown.
  Their credits object is omitted to avoid presenting stale balances as current.

Older account servers omit the duration fields and short-window reset time.
Their rows report `null` for those values while retaining the known long-window reset time.
Upgrade the account server as well as the CLI to expose durations for server accounts.

An alias can occur twice when a profile and a server account share it.
Use `source` with `alias` to distinguish those rows.
Array order is not a ranking contract.
Consumers must accept new fields within version 2.
Changes to existing field types or meanings require a new version.

## Fleet Pace and Table Output

The top-level `fleet_pace_points` is the unweighted mean of known `pace_points`
in the emitted account rows. Each qualifying account has equal weight.
This equals mean weekly usage minus mean elapsed percentage over the same accounts.
Plan names do not give a reliable capacity weight, so the calculation does not use them.
Accounts with unknown pace are excluded. An empty qualifying fleet gives `null`.
Filters apply before the mean is calculated. Profiles and server accounts are separate rows.

Status tables show a `Pace` column and a `Fleet` row when at least one displayed
account has known pace. They hide both when all pace is unknown.
Unknown row values display `-`. Known values round to whole percentage points:
`+12 ahead`, `-8 behind`, or `0 on pace`. Ahead carries a yellow warning color.
JSON keeps the unrounded numbers. A focused table computes its fleet mean over
the accounts displayed in that table.
Small nonzero values can round to `+0 ahead` or `-0 behind`; their direction and warning remain.

Version 2 adds `pace_points`, `elapsed_percent`, and `fleet_pace_points`.
Other version 1 fields retain their types and meanings.

## Fetch and Failure Behavior

JSON output uses the same data fetch as table output.
It adds no upstream requests.
Local `list --json` reads metadata only, so usage stays `null` and billing stays `unknown`.
Its credits object is omitted.
Connected `list --json` includes the catalog usage and any unmigrated profile usage that the table already fetches.
The schema reports the main Codex windows, credits, and banked reset inventory.
Additional named limits are excluded.
It does not expose tokens, credentials, or workspace identifiers.

`--rate-limited` and `--usage-based` retain the table's filter behavior.
A display group does not prove a billing class.
JSON keeps the billing class from the usage response, even when the table groups unknown billing with rate-limited accounts.
Migration-fenced profiles remain excluded from connected output, as in the table.

An account fetch error leaves a row with an `error` value.
For command-level failures, such as catalog rejection or an unreadable profile registry, the command exits nonzero and writes the error to stderr.
That failure does not emit a partial JSON document.
A zero exit status alone does not prove that every account fetch succeeded.
Consumers must inspect each row's `error` and handle `null` usage as unknown.
