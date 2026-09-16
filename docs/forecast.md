# Weekly Quota Forecast

Run `codexctl forecast` to see whether at least one account remains available
throughout the next seven days. The headline shows the first modeled gap and
total gap time, or states that no gaps are predicted. Confidence stays low when history is short or rates use
assumptions. This is a scenario based on past demand, not a guarantee.

The `--details` view includes the longest gap. `NOW` counts accounts with quota at the latest fetch. `WEEK LOW` gives the lowest
modeled count. `HISTORY` shows the time since the oldest saved observation within
the past week; observations count individual account records. That duration does
not imply continuous sampling or equally long history for every account.

The default view is a seven-row hourly heatmap. Each row starts at its printed
local timestamp and spans 24 elapsed hours. Columns label elapsed hours after
that start, rather than local clock hours. Each cell shows the lowest modeled
account count during its hour. A one-second gap still receives a red `×`.

The legend distinguishes gaps, one account, two or three accounts, four or more,
and unknown capacity. Symbols also distinguish these states without color.
Incomplete account coverage uses `?` for positive cells; `×` then means a gap
among known accounts. Narrow terminals split the hours into bands without
removing cells. The first three gap intervals appear below the heatmap.

Use `codexctl forecast --details` for account balances, daily coverage totals,
and full model assumptions. The compact view keeps confidence, history duration,
reset assumptions and sampling status visible.

```bash
codexctl forecast
codexctl schedule             # preview hourly samples
codexctl schedule --install   # add the sampling job to your user crontab
codexctl schedule --remove    # remove only the codexctl cron job
```

## Shared Workload

The model groups accounts by reported plan and quota-window durations. Within
each group, it assumes equal allowance sizes and adds the estimated weekly
consumption rates. It divides that constant demand equally among usable accounts.
When an account hits any main limit, the remaining accounts receive its work.
Each recipient keeps its estimated ratio of short-window use to weekly use.

Resets restore quota at the reported timestamp and repeat at the window duration.
If all accounts in a group are blocked, work stops until a reset. The model does
not accumulate a backlog during that gap. Demand never moves between groups:
percentage points from different plans do not establish comparable token counts.
Even matching plan names do not prove equal capacity; this remains an assumption.

A short window can block an account before its weekly allowance runs out.
An unknown short-window rate assumes no future short-window use and lowers
confidence. Known exhaustion still blocks the account until reset. An exhausted
weekly window with no rate also remains blocked until reset; its own demand
is unknown and coverage is incomplete. After reset it can receive a share of
known demand from its group.

Balanced routing is a scenario, not an account-switching policy. This command
never switches accounts or redeems resets. Changed workloads, model choices,
context lengths or routing can change the result. Paid credits and additional model-specific limits are excluded. Missing account data prevents
a conclusion about the whole fleet.

## Banked Reset Scenario

The forecast assumes reset use is enabled. It uses the live banked-credit inventory
already fetched by status. The headline shows the number of verified credits and
compares total gaps with and without banked resets.

At exhaustion, the model spends one credit from that account, choosing the
nearest expiry first. A credit must remain valid at that instant. Scheduled
resets happen first when the two events coincide. Credits never move between
accounts, and the model does not predict future credit grants.

The simulation assumes one credit clears all exhausted main windows and starts
new windows of the same duration. Unexhausted windows keep their usage and clocks.
These reset effects are assumptions, so the display marks the scenario provisional.
The forecast never redeems a credit or changes reset approval settings.

Only available `codex_rate_limits` credits with valid expiry timestamps enter the
simulation. Duplicate IDs count once. Unknown types, missing expiries and failed
inventory fetches cannot add capacity; incomplete inventory produces a warning.
Live credits do not enter the saved usage history.

## Usage History

The estimator uses percentage-point changes between compatible observations from
the past week. It excludes changed resets, declining usage, changed plans or
window layouts, and gaps longer than two days. It requires positive consumption
and one hour of observations. Half a window suffices for windows under two hours.

Without enough history, the estimator uses consumption since the current window
began and marks the result provisional. A fresh window or one without consumption
has no estimated rate. Accounts without a weekly estimate still appear in the
current quota table, but their future capacity is unknown.

The `status` and `forecast` commands save observations in
`~/.codexctl/usage-history.json.gz` as gzip-compressed JSON. Status displays after `use`, `login` and a
successful `reset` also save them. The file retains four weeks, with at most one
observation per seat per five minutes. Aliases for the same seat count once.

On the first write, existing uncompressed history migrates under the store lock.
The old file is removed only after the compressed file is saved atomically.
Corrupt history remains unchanged. Compression preserves every retained observation.

Records contain seat identifiers, aliases, plans, timestamps, percentages and
reset times, without credentials. Writes use private permissions, a lock and
atomic replacement. Invalid history remains unchanged and produces a warning.

## Automatic Samples

The cron schedule runs `status` at minute 00 of every hour in cron's timezone.
Install it with the binary you intend to keep using. The job records its absolute
path and home directory, preserving an invoked symlink to the running binary.
Reinstall after a move or upgrade changes that path. Repeated installation keeps
one managed entry and preserves other jobs.

Cron must be available and the machine must be awake. Missed cron runs are not
replayed. Output is discarded; errors use cron's normal delivery. Previewing the
schedule makes no changes.

The forecast checks for a matching managed cron entry. On macOS it also reports
whether the `ai.sawmills.codexctl-sampling` LaunchAgent is loaded.
The footer reports both mechanisms when they apply. Registration
alone does not prove successful sampling. The cron commands do not install or
remove a separately configured LaunchAgent.

## Rendering and Sources

The display uses existing `comfy-table` and `crossterm` dependencies. Color requires
a terminal, an unset `NO_COLOR`, and a `TERM` other than `dumb`. Redirected output
stays plain. See the [visualization research](research/2026-09-15-cli-visualization.md).

The [API types](../src/api.rs) define quota windows and seat identity.
The [estimator](../src/forecast.rs) derives rates;
the [shared-workload model](../src/forecast/shared.rs) calculates availability.
