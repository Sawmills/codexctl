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

## Schema Version 1

```json
{
  "version": 1,
  "accounts": [
    {
      "alias": "personal",
      "label": "Personal",
      "plan": "pro",
      "source": "server",
      "state": "server",
      "primary_used_percent": 12.5,
      "secondary_used_percent": 37.0,
      "primary_window_seconds": 18000,
      "secondary_window_seconds": 604800,
      "primary_resets_at": "2099-12-25T05:00:00Z",
      "secondary_resets_at": "2100-01-01T00:00:00Z",
      "resets_at": "2100-01-01T00:00:00Z",
      "billing_class": "rate_limited",
      "error": null,
      "usage_age_seconds": 12,
      "usage_stale": false
    }
  ]
}
```

Every listed field is present in every account row.
Unknown values are `null`, except `billing_class`, which uses `unknown`.
An empty result is `{"version":1,"accounts":[]}`.

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
- `primary_window_seconds`, `secondary_window_seconds`: Declared duration in seconds of the corresponding short or long window, or `null` when unknown.
  Durations use the same upstream evidence as the statusline; they are never inferred from plan, position, or reset time.
  Upstream minutes are converted to seconds. A weekly-only primary window is placed in the secondary fields, alongside its percentage and reset time.
- `primary_resets_at`, `secondary_resets_at`: Each window's reset time in RFC 3339 UTC format, or `null` when unknown.
- `resets_at`: The long window's reset time in RFC 3339 UTC format, or `null`.
  This existing field remains an alias of `secondary_resets_at`.
- `billing_class`: `rate_limited`, `usage_based`, or `unknown`.
  Failed requests keep unknown billing, regardless of the table's display group.
- `error`: A brief account error, or `null`.
  Server accounts marked unavailable use `account unavailable` because the catalog supplies no detailed cause.

- `usage_age_seconds`: Age of the last successful server usage observation, or `null` when unknown or local.
- `usage_stale`: Whether server usage is stale or missing, or `null` for local rows.
  Stale server rows retain the last usage values and include an `error`; consumers must treat their quota as unknown.

Older account servers omit the duration fields and short-window reset time.
Their rows report `null` for those values while retaining the known long-window reset time.
Upgrade the account server as well as the CLI to expose durations for server accounts.

An alias can occur twice when a profile and a server account share it.
Use `source` with `alias` to distinguish those rows.
Array order is not a ranking contract.
Consumers must accept new fields within version 1.
Changes to existing field types or meanings require a new version.

## Fetch and Failure Behavior

JSON output uses the same data fetch as table output.
It adds no upstream requests and does not change the account server.
Local `list --json` reads metadata only, so usage stays `null` and billing stays `unknown`.
Connected `list --json` includes the catalog usage and any unmigrated profile usage that the table already fetches.
The schema reports the main Codex windows, not additional named limits, credits, or banked resets.
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
