# Reading Account Status

Run `codexctl status` for live usage, or `codexctl status --json` for automation.
See the [JSON contract](status-json.md) for fields and failure behavior.

Status sorts accounts by availability and fetches their usage in parallel.
The account column is the saved profile alias, with `*` marking the active account.

Status shows a `Pace` column when fresh weekly window data is available.
`+12 ahead` means usage is faster than the straight line through that window;
`-8 behind` means usage is slower. Values that round to zero show `0 on pace`
without color. The `Fleet` row or footer shows the mean of known
pace across the displayed accounts. `-` marks unknown pace, and empty pace columns
are hidden. See [Account Status JSON](status-json.md) for the calculation and
version 2 fields.

Rate-limit windows are matched and labeled by their server-declared duration. For example, the
table can show `15m`, `1h`, `5h`, or `7d` columns. A column appears only when at least one returned
bucket contains that duration. Each account has one table row. Named model or feature buckets use
aligned lines inside that row. `codexctl` does not invent a 5-hour window when the service returns
only a weekly window.

OpenAI's current [subscription documentation](https://learn.chatgpt.com/docs/pricing) describes one
shared agentic usage and credit pool, with plan and model-specific allowances. It does not promise
one universal window layout. The live [multi-bucket rate-limit
response](https://developers.openai.com/codex/app-server#6-rate-limits-chatgpt) is therefore
authoritative for the columns that `codexctl` shows.

Automatic account selection uses the main `Codex` bucket. Additional buckets are model-specific or
feature-specific status. They do not block general account selection without a reliable mapping
from the requested model or feature to that bucket.

The `Resets` column shows banked rate-limit resets (see [banked resets](resets.md#list-and-redeem)):
`3 (2 now)` means three are held and two can be redeemed this second; a bare count turns red when
a credit lapses within three days.

The `Credits` column shows OpenAI credit balances and availability. Balances use
credits, for example `53,306.16 credits`, with two decimal places and comma separators.
These values are not dollars. The column appears when an account reports credits data,
including a zero balance. `status --json`
and the server dashboard expose `has_credits`, `unlimited`, `balance`, and
`overage_limit_reached`. Missing or null credits data stays absent in JSON.

The `Token` column shows the stored access token's remaining lifetime: green for
days, yellow for hours, and red below one hour. `invalidated` means OpenAI rejected
the grant before token expiry. A fresh Codex sign-in for another seat can invalidate
an existing grant. For local profiles, `codexctl use` only swaps the local auth
file. Renew a server account with `codexctl login <alias>` when required.

Usage-based accounts are shown in a separate table with balance, seat limit, credits, and spend
control status. The `Credit balance` column uses credits. The `Seat` column uses dollars.

When billing classification is unknown, status still shows the returned usage windows, resets,
and token expiry. A `Billing` column marks those accounts as `unknown`. An unrecognized plan or
missing main usage windows can cause this classification. Automatic selection still excludes
these accounts.

The `Plan` column shows the subscription returned by OpenAI, including Pro, Pro (More), and
Pro (Max). Subscription windows and purchased credits can coexist. Credits do not make a known
subscription unknown; spend control still determines billing risk during recovery. Plan names
are not invoices or credit balances in dollars.
