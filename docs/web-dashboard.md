# Browser account overview

Open the account server's root URL to install codexctl, connect a machine, or sign
in with company SSO. A signed-in browser goes directly to `/accounts`.
The connect command uses the server’s configured `--public-url`, including behind
a reverse proxy. Request Host and forwarding headers cannot change its destination.
The public landing page shows no account or machine data. Its status pill polls
`/ready` every 60 seconds and distinguishes ready, not ready, and unknown.
Installation commands pin Linux downloads to the published v0.1.34 release and
check the release's SHA-256 file before extracting or installing.

`/accounts` is a read-only overview of the signed-in company user's server accounts
and registered machines. The browser polls `/accounts/data` every 60 seconds after
the preceding request finishes. Countdown clocks and observation ages update locally
every minute. Remaining time rounds up to a whole minute before carrying minutes
into hours and days.
Missing percentages, durations, reset times, and inventory counts remain unknown.
The page labels windows by their declared duration, including nonstandard windows;
it does not infer five hours or a week from their position. High usage starts at
80%; exhausted starts at 100%. Text labels accompany these colors.

After each authorized, successful `/v1/token` response, the server records its
company user, machine identifier, canonical account alias, and timestamp in memory.
No credentials are recorded. A failed request leaves the last successful observation
unchanged. Restarting the account server clears these observations; unseen machines
show last seen as unknown and omit account associations.

The hero shows “In use now” for non-revoked machines with a token delivery in the last
five minutes. Each machine gets its own row with its account label and alias, plan,
large long-window capacity, short-window capacity, reset countdown, and last delivery.
This is a token-delivery observation, not proof of a running session or every session's
account. If none are recent, “Suggested” features the available account with the most
fresh, included long-window headroom. It does not switch anything. Unmigrated local
profiles are not visible to the account server.

Cards sort in-use accounts by machine recency, then available accounts by long-window
headroom, high usage (at least 80% used), exhausted, unknown/stale usage, renewal pending,
and unavailable. Empty reset inventories say “No banked resets”; positive redeemable
counts stand out. Observation ages use seconds below a minute and minutes thereafter,
with a separate Stale pill for an individually old observation. When all observations
are stale, a single banner replaces repeated pills. Registered machines are prominent; revoked machines sit
under a collapsed disclosure, whose state is represented by `?revoked=show`.
Registration is not live connectivity. Names use the recorded machine identifier,
including enrollment's uniqueness suffix when present.

## Authentication and delivery

Dashboard sign-in reuses enrollment's OIDC issuer, verified email-domain policy,
PKCE, nonce, audience and signature validation, and issuer-plus-subject company user
identity. It uses the existing `/auth/callback`. A separate, short-lived HttpOnly
cookie binds dashboard login to the browser that initiated it.

Sessions are opaque random credentials held only in cookies; the account server
stores their hashes in memory for one hour. HTTPS cookies use `Secure`, `HttpOnly`,
`SameSite=Lax`, `Path=/`, and the `__Host-` prefix. Loopback HTTP is available only
under the existing development configuration. Restarting the account server ends
browser sessions. The existing machine enrollment and approval flow is unchanged.
An approved company identity can open an empty overview without enrolling a machine.
Disabled company users cannot sign in or use an existing session for account data.
The public landing page ignores sessions that cannot identify an enabled company user
and shows its signed-out view; account pages and JSON still enforce authorization.
Authorization is checked again after asynchronous data reads. The header shows the signed-in email.
Sign out submits a same-origin POST to `/accounts/sign-out`, revokes only the current
browser session, clears its cookie, and returns to the landing page. Other browser
sessions and machine credentials are unaffected. Missing or foreign Origin headers
are rejected. Pages use a same-origin referrer policy so native form submissions
retain their Origin while external documentation links disclose no referrer.

The page and JSON use `Cache-Control: no-store`. Embedded CSS and JavaScript are
pinned by SHA-256 in the CSP; connections and forms are restricted to the same origin,
and framing and base-URL changes are prohibited. There are no external assets or
trackers. The HTML contains no account data. JSON explicitly selects display fields,
excluding credentials, workspace and company-user IDs, reset IDs, and machine token
hashes. Dynamic text is inserted with DOM text nodes, never interpreted as HTML.

## Observations and cache

Account usage shares the existing catalog reader with the machine API. Concurrent
reads share an upstream request and its 60-second success/failure cooldown. Page HTML
never queries OpenAI. Dashboard reads never refresh credentials, start a refresh owner,
select an account, approve billing, or redeem a reset.

Reset counts use cached usage evidence and the reset-inventory endpoint's count.
When both are present, the larger count follows the existing reset-list convention.
Redeemable counts remain unknown when usage does not report them. Expiry comes from
available credits with a known future expiry; consumed credits are excluded.
Reset inventory has its own shared 60-second cache, keyed by server account and
credential revision, and uses only the saved access token. A failed read retains its
last observation, marks it stale, and waits 60 seconds before retrying. This path
reports stale data in the response and adds no operational alert. Credential changes
discard expiry evidence from an in-flight read. Stale values are observations, never
proof of capacity or permission to spend.

`GET /accounts/data` requires a browser session, not a machine bearer credential:

```json
{
  "version": 1,
  "identity": { "email": "amir@sawmills.ai" },
  "server_time": 4102434000,
  "accounts": [
    {
      "alias": "studio",
      "label": "Everyday building",
      "plan": "pro",
      "state": "available",
      "billing_class": "rate_limited",
      "credits": {
        "has_credits": true,
        "unlimited": false,
        "balance": "12.50",
        "overage_limit_reached": false
      },
      "primary": {
        "used_percent": 24.0,
        "left_percent": 76.0,
        "window_seconds": 18000,
        "resets_at": 4102444800
      },
      "secondary": {
        "used_percent": null,
        "left_percent": null,
        "window_seconds": null,
        "resets_at": null
      },
      "usage_age_seconds": 8,
      "usage_stale": false,
      "usage_error": null,
      "banked_resets": {
        "count": 2,
        "redeemable_now": 0,
        "nearest_expiry": 4097174400,
        "stale": false
      }
    }
  ],
  "machines": [
    {
      "name": "studio-laptop",
      "status": "registered",
      "last_seen_at": null
    }
  ]
}
```

Times are Unix seconds. `server_time` anchors browser countdowns without trusting the browser clock.
Machine `last_used_alias` is present only after a successful token response. Account state is `available`, `unavailable`, or
`renewal_pending`. Billing class is `rate_limited`, `usage_based`, or `unknown`.
The optional `credits` object preserves reported zero balances and false flags.
Its three flags are booleans; `balance` is the upstream string or null when unknown.
Credits are omitted when upstream data is absent or null, or when usage becomes stale.
This includes expiry or a credential change during the snapshot request.
Other unknown optional facts are null. The page displays dates in the browser's timezone.
This browser schema is separate from the CLI's [status JSON](status-json.md).

## Interface choice

Two interfaces were considered before implementation:

- Compose the machine endpoints: callers supply a machine bearer credential and
  coordinate catalog, resets, and devices; reset reads can refresh credentials.
  Browser SSO would need a credential bridge and additional cancellation policy.
- Read a company-user snapshot: handlers supply the authenticated company user;
  the catalog owns usage observations and the reset reader owns its inventory cache.
  Handlers return safe display fields or an authorization/registry failure. Individual
  usage failures remain visible as stale or unknown observations. Cancellation can
  stop read-only inventory work; catalog tasks retain their existing shared-fetch
  behavior. There are no credential mutations or redemption side effects.

The second interface is used. It keeps browser authentication separate from machine
credentials while preserving one catalog cache and existing machine API behavior.

## Browser evidence

The review screenshots under `~/b16-screens/` (also attached to the PR) use synthetic accounts and
machines, production HTML/JS/CSS and CSP, and a loopback JSON fixture. They do not
prove deployed SSO or OpenAI acceptance. The Rust integration tests use a real server
process and a synthetic OIDC issuer with RSA signatures and PKCE checks.

To reproduce the optional browser checks on a development machine with Node:

```sh
B16_RENDER_DIR=/tmp/b16-render cargo test --locked --test central_managed_test dashboard_pages_pin
npm install --prefix /tmp/b16-browser playwright @axe-core/playwright
/tmp/b16-browser/node_modules/.bin/playwright install chromium
NODE_PATH=/tmp/b16-browser/node_modules node tests/fixtures/dashboard_browser.cjs /tmp/b16-render
```

Install Chromium's OS libraries if needed with Playwright's `install-deps chromium`.
The harness captures light/dark desktop and phone views, checks WCAG A/AA rules with
axe, and exercises copy buttons, polling, hostile-looking text, and session loss.
Hosted CI remains the full Rust/platform/build/audit/Trunk gate. HQ owns deployment;
Amir approves the visual design before merge. Ship after v0.1.35.
