# codexctl relay

The relay keeps a Codex lane working through short OpenAI capacity stops. It runs on
127.0.0.1 and sits between a lane and `https://chatgpt.com/backend-api/codex`.

## Why it exists

Codex 0.161.0 stops a turn at once on two errors:

- HTTP 429 "exceeded retry limit, last status: 429". The request layer never retries a
  429, and the turn loop retries it only when the server sends `Retry-After`.
- "Selected model is at capacity. Please try a different model." This is a
  `response.failed` event with code `server_is_overloaded`. Codex retries it only with
  server retry advice.

The provider settings `request_max_retries` and `stream_max_retries` do not reach either
error.

## Codex versions

The relay helps only where Codex honors retry advice. Each row was checked with a
real Codex client and the fake upstream in `tests/relay_codex_e2e.py`.

| Codex           | HTTP 429                                     | Model at capacity               |
| --------------- | -------------------------------------------- | ------------------------------- |
| 0.160.0         | Not covered: Codex treats every 429 as final | Covered, including after output |
| 0.160.1         | Not covered: Codex treats every 429 as final | Covered                         |
| 0.161.0         | Covered                                      | Covered                         |
| 0.162.0-alpha.9 | Covered                                      | Covered                         |

Codex 0.160.1 ignores `Retry-After` on a 429 (`protocol/src/error.rs` at
rust-v0.160.1). Codex 0.160.0 has the same rules. Run `codex --version` on the host
before you enable the relay.

To check a lane host, run the fake-upstream check with that host's Codex. For 0.160.0 or 0.160.1, add
`--rate-429 stop`, so the check expects the 429 stop and still tests the capacity cases:

```sh
python3 -I tests/relay_codex_e2e.py --codexctl <codexctl> --codex <codex> [--rate-429 stop]
```

## What it does

The relay adds retry advice. Codex's own loop then retries and shows
`Reconnecting... n/N`. The relay never retries by itself. It never changes the model,
the account, or the auth headers, and it never resumes a goal.

- **HTTP 429 rate limit:** the relay adds `Retry-After` when upstream sent none.
- **Capacity event before any output:** the relay adds `error.headers["retry-after"]`.
  It also sets the code to `rate_limit_exceeded` and appends "Please try again in
  Ns." to the message. Codex 0.161.0 reads only that form; newer Codex reads the
  header.
- **Capacity event after output:** the relay retains output already delivered,
  sends the same advised `response.failed` as before output, and lets Codex wait
  the jittered policy delay. Codex rebuilds the turn from history, retaining
  completed tool calls; the relay does not replay the request or output.
  A completed response ends the failure
  streak. Partial output alone does not reset the budget.
  Failure frames stay in memory until classified, up to 1 MiB. Larger failure
  frames retain bounded error and retry metadata and omit the oversized payload.
  An unclassified frame beyond that bound becomes a terminal size refusal, even
  if its type appears later. An oversized failure cut short by upstream EOF uses
  valid bounded metadata rather than forwarding a discarded frame's suffix.
  Exhaustion removes upstream retry advice. Ordinary identified output continues
  to stream without waiting for a complete frame; no payload is spooled to disk.
- **Usage, quota, billing, plan and flex 429s:** pass through unchanged, so the quota
  guard still sees them.
- **Everything else:** passes through unchanged, byte for byte. This covers 401, 403
  and 5xx, streamed rate-limit codes, and non-capacity errors after output. The relay
  records a bounded `stream_failure` observation for these errors.

Delays double from 2 s up to 60 s, with jitter. Each Codex thread (`thread-id`
header) has one failure streak. When a streak spends its budget, the relay passes the
original failure through and removes any upstream `Retry-After`. Codex then stops, and
the board resumes the lane. A new streak starts when the error kind changes or on
the first failure after exhaustion (that request is a resume). With no POST
in flight, a new POST arriving 120 s beyond the advised retry time restarts
the streak. Active streaming
does not count as idle. Elapsed client waits count toward
exhaustion when the next failure arrives. A POST request guard protects the thread
until the response body ends or is dropped; overlapping POSTs keep that
protection until all finish. Idle reset cannot replenish an in-flight streak.
Other methods do not register or protect a POST retry streak.
Global pruning runs at most once per minute on incoming POSTs. It removes
only streaks outside their own budget, with no request in flight and more than
an hour of idle time since their latest activity or advised retry time.
Between sweeps, request tracking and idle reset use keyed lookups.
A different terminal stream failure ends the streak
without reporting recovery. Partial output does not pause the elapsed-time
budget. A client interruption without a terminal event does not prove completion;
reusing that thread within the idle window can retain its old budget.

| Error               | Default budget | Flag                       |
| ------------------- | -------------- | -------------------------- |
| HTTP 429 rate limit | 180 s          | `--rate-budget-secs`       |
| Model at capacity   | 600 s          | `--overloaded-budget-secs` |

## Run it

```sh
codexctl relay unit launchd > ~/Library/LaunchAgents/ai.sawmills.codexctl-relay.plist
launchctl load ~/Library/LaunchAgents/ai.sawmills.codexctl-relay.plist
# devbox
codexctl relay unit systemd > ~/.config/systemd/user/codexctl-relay.service
systemctl --user daemon-reload && systemctl --user enable --now codexctl-relay
codexctl relay status   # exit 0 only when the relay proves its listener identity
```

The service manager restarts the relay when it exits. A bind failure is fatal. On stop,
the relay stops accepting and drains open streams for up to 30 s. An open stream that
is cut then is retried by Codex as a dropped stream.

The upstream client allows five minutes of silence per read, including the wait
for response headers. Each successful read resets that timeout, so an active
stream can run longer than five minutes. The relay does not retry a timed-out
request; the client receives a gateway or stream error.

The relay measures its own process CPU on a separate OS thread every ten seconds.
Three consecutive windows above 150% CPU log the worker, task, queue and active
stream counts. On macOS the watchdog then runs `/usr/bin/sample` for three seconds,
with an eight-second deadline. Samples are private files under
`~/.codexctl/relay/diag/<UTC timestamp>.txt`. Before each capture the relay prunes
its timestamped samples older than seven days and retains at most eight samples,
including the new capture. Each sampler child has a hard 4 MiB file-size limit,
so retained samples use at most 32 MiB per relay. Other filenames are untouched.
A retention error skips capture; a capped output logs `size_limit`. Attempts are
limited to one per hour, including failed attempts. Other platforms log the counts and
`unsupported_platform` for the sample result. The watchdog stops with the relay.

`codexctl_relay_open_streams` is an unlabeled gauge on `/metrics`. It counts active
forwarded exchanges from upload through response drain, including those waiting
for upstream headers. Completion, failure and cancellation release the count.
Neither the gauge nor the local CPU watchdog has a dedicated external alert.

Release builds retain line tables. Darwin release assets include both binaries'
dSYMs in a ZIP and their `dwarfdump --uuid` output; the packager checks matching
UUIDs and strips debug information with `strip -S`, retaining the symbol table.
Linux debug archives include separate debug files and packed DWARF units when
present, linked from each executable with `.gnu_debuglink`. Use the assets from
the same release and architecture as the sampled process. Workflow publication
must use `scripts/package-release.sh`; the A124 owner-applied workflow delta is
tracked on SAW-13349.

`relay status` performs a bounded HMAC challenge against the exact listener. A plain
`200 ok`, an old relay binary, a missing or unsafe identity file, a replaced listener,
or a stopped relay fails closed. After upgrading codexctl, restart the service so it
creates or loads the identity key before enabling account-server routing. An existing
lane keeps its current child settings, but a new launch or activation refuses until
the listener proves identity. Use `codexctl relay disable` to roll back to direct
routing while the service is repaired.

The relay accepts the same-user `0755` shared `~/.codexctl/relay/` directory created
by v0.1.51. Its `identity/` directory stays private (`0700`) and keys stay `0600`;
group- or other-writable parents are refused without changing permissions. If you
remain on v0.1.52, verify the shared directory's owner and run
`chmod 700 ~/.codexctl/relay` before restarting that version: v0.1.52 incorrectly
requires the shared parent to be `0700` too.

## Enable account-server routing on a host

Start or restart the upgraded relay service first, then enable the supported host setting from an
unpinned shell without `CODEX_HOME` or `CODEXCTL_PINNED_ALIAS`:

```sh
codexctl relay status || exit 1
codexctl relay enable --listen 127.0.0.1:47631
```

The setting in `~/.codexctl/relay.json` applies to the activated account-server
provider and future pinned or automatic account-server launches. It survives
local account selection and disconnect. Existing children keep their launch
settings, so restart them to change routes. Only a loopback address with a nonzero
port is accepted. Provider overrides remain rejected by pinned launches.
Relay routes preserve the configured stream retry count (default 12). The client
limit can stop a turn before the relay budget is exhausted. Codex uses the same
counter for other stream errors with uncapped exponential backoff, so the relay
does not raise it to make a capacity streak outlast the budget. Enable, disable,
activation and launches all preserve explicit retry settings.
If a filesystem error interrupts the two route writes, a private pending intent
remains. The next configuration change or account-server launch completes it
under a route lock before selecting a URL. Fix the reported filesystem error
and retry; a plain Codex invocation alone does not perform that recovery.

Pinned and automatic launches attach the selected alias and billing class as
relay-only metadata. The shared activated provider follows the active account
through its token helper, so it sends no static account label; those requests
have an unknown class and no alias. The relay removes metadata headers before
forwarding. Routing does not select another
account, approve billing, redeem a reset, or change the model.

To restore direct routing, run `codexctl relay disable` from an unpinned shell and
restart existing children. This does not stop the relay service.

## Observe it

Each capacity event writes one JSON line to stderr. The line holds the kind, outcome,
model, account class, thread id, attempt, advised delay, and upstream request id. It
never holds a token or a body. `GET /metrics` on the relay serves:

```
codexctl_relay_capacity_events_total{kind,model,account_class,outcome}
codexctl_relay_stream_failures_total{kind,stage,model,account_class}
codexctl_relay_lane_stops_total{reason}
```

- `kind`: `rate_429` or `overloaded`.
- `outcome`: `advised` (one per advice), `recovered` (one per streak that ended in
  a completed response), `exhausted` (one per streak that spent its budget), or
  `terminal_passthrough`.

The relay sends the four labels for non-advised capacity outcomes to the authenticated
central endpoint `/v1/relay/capacity-events`. Advised events stay local so repeated
retries cannot consume the per-device central event budget. The relay uses a bounded
queue and drops an event when central is unavailable or the queue is full;
`codexctl_relay_central_dropped_total`
records each reason. The central server exports
`codexctl_central_relay_capacity_events_total` plus an event-time gauge for each
accepted exhausted label set. The gauge makes a one-off event visible even when
it arrives before the first Prometheus scrape. The server rejects unknown labels
or an over-limit device with HTTP 400 or 429.

For each streamed `/responses` request, the relay also writes at most one
`outcome="stream_failure"` JSON line when it sees `response.failed`, `error`,
`response.incomplete`, an unclean end, or an unclassified HTTP error. The line records
`stage` (`pre_output` or `after_output`), the bounded run-length event-type trace,
elapsed times, model and account class, and upstream `cf-ray` and
`x-oai-request-id` headers. Failure messages are reduced to their character length and
one of the bounded classes `capacity`, `rate_limit`, or `other`; their text and
parameters are never logged. Output text, deltas, request bodies, authorization, and
cookies are never included. The process emits at most 30 failure lines per minute and
then one suppression summary.

The account server also accepts a typed terminal-outcome report through this same
authenticated endpoint. It has one field, `lane_stop`, with one of four reasons:
`rate_limited`, `capacity`, `login_required`, or `token_refused`. It does not accept
identities, request IDs or messages in that report. General `stream_failure` detail
stays local.

`codexctl_relay_lane_stops_total{reason}` counts a terminal account failure once per
HTTP request or SSE response. Exhausted HTTP 429 and capacity retry budgets count;
advised attempts and recovered retries do not. Repeated failure frames in one
response count once. Known login-expiry or refresh-grant errors use `login_required`;
other HTTP 401/403 and known token errors use `token_refused`. Unclassified transport
or server errors do not prove an account stop. Codex retries streamed rate-limit
codes itself, so those frames alone do not prove it stopped. The relay cannot see
when Codex spends a separate client retry budget while advice is still available.

The account server exports `codexctl_central_lane_stops_total{reason}` for accepted
terminal reports. These are the same events as the local counter, so do not add
the two counters together. Reports use the existing best-effort queue and machine
rate limit; the local counter can exceed the server counter when reporting fails.
Use `codexctl_relay_central_dropped_total` to identify incomplete server coverage.
All four reason series start at zero. The process counters restart at zero;
retained evidence survives a relay restart within its retention and size bounds.

The HA rules record `codexctl:lane_stops:increase1d` by reason for dashboards.
Prometheus derives this rolling daily increase from samples; it is not an exact
UTC-day ledger. There is no new lane-stop alert.

`codexctl_central_billed_selections_total{path="auto"|"explicit"}` records the
selection of an account with verified credit billing. Client activation, private
launch preparation and app-auth enablement send one typed receipt only after
their consent and automatic-placement guards pass and the selection commits.
The path comes from the caller's selection intent. Token probes, declined
selections, included accounts and credential refreshes do not send receipts.
Both path series start at zero. This counts selections, not dollars spent.

The receipt contains only `billed_selection`, with value `auto` or `explicit`.
It uses the same authenticated endpoint and rate limit as other relay reports.
The sender does not retry, follow redirects or use a proxy. It sends after
mutation locks release, with a 250 ms HTTP deadline. A send failure logs
`billed_selection_report_failed` and leaves the selection result unchanged.
The local counter `codexctl_client_outcome_reports_failed_total` is exposed on the
relay. Its small private tally at `~/.codexctl/outcome-metrics/report-failures`
survives CLI and relay restarts. If its nonblocking lock or disk write fails, the
log has `counter_available=false`; the persisted tally then omits that failure.
Keep this coverage limit separate from the server's accepted-selection count.

The staging and staging-ha rules raise the critical
`CodexctlAutomaticBillingSelected` alert for an automatic billed-selection event
within the last hour, including its first scraped sample. It clears after an
hour without another event; the cumulative counter stays available. Automatic placement must
stay on included subscription accounts. Inspect the bounded selection outcome
logs, use an explicit subscription account, and fix the client placement guard.
Probes must never produce this alert. The rule is tested with promtool; live
critical notification delivery requires the platform Alertmanager route and is
not established by those tests.

`relay serve` also keeps sanitized evidence in `~/.codexctl/relay/evidence`.
Private daily JSONL files retain the current UTC day and six previous days, with a
4 MiB limit per day and a 256-record write queue. Request records stop at 3 MiB,
reserving the last 1 MiB for later failures. A separate worker writes files;
the same worker prunes expired files at startup and once per minute even when no
response arrives. Disk errors or a full queue do not delay response delivery. Records include UTC
hour, actual HTTP status, bounded error code, validated retry headers, known
model, validated account alias, account class, and a sanitized request ID when
available. HTTP 429 records also keep the relay's rate/terminal decision, so
code-less rate limits and terminal billing stops are counted correctly. Older
records use their saved error code. Records never include bodies, messages,
prompts, output, or credentials.

```sh
codexctl relay evidence        # counts by model and UTC hour
codexctl relay evidence --json
```

Counts distinguish requests, overloads, rate limits and other failures. The JSON
report adds `lane_stops`, a map from terminal reason to count in each model/UTC-hour
row; text output shows `reason=count`. Sum rows for the required UTC date to read
its daily outcome. Terminal records do not count again as provider failures.
Counts cover
only valid retained records. A torn append does not prevent reports: both output
formats warn on stderr with the number of damaged lines excluded from counts.
The writer separates an unterminated crash tail before the next record, so that
new observation remains readable.
The warning never prints record contents. Non-regular files and oversized files
still fail the report. `codexctl_relay_evidence_dropped_total{reason}` reports
`queue_full`, `io`, and `daily_limit` losses. This metric has no dedicated alert;
`codexctl_relay_evidence_maintenance_failures_total` counts failed expiry passes,
with a bounded local failure log. It has no dedicated alert either. The existing
exhausted-capacity alerts below remain unchanged. No notification
delivery was tested for this change.

The devbox routing proof used one healthy turn on a pinned subscription account.
Recovery and exhaustion were proved with a real Codex 0.160.0 client against a
fake upstream. Hosted macOS CI validates the change before merge. After the normal
release and Homebrew install, HQ must run the bounded routing check on one Mac
lane; no Mac candidate binary or Mac live result is claimed here.

The staging and staging-ha Prometheus rules alert on one exhausted event in a 15-minute
window. They intentionally exclude `advised`: that outcome means the relay is still
retrying and does not need operator action. Alerts carry `severity=warning` and
`service=codexctl`; the platform Alertmanager
`warnings-slack` receiver routes them to `#warning-alerts`. The platform route file is
owned outside this repository, so `amtool` cannot validate the live route here. The
monitoring guide must run `amtool config routes test` against that platform file with
`severity=warning service=codexctl` and confirm `warnings-slack`.
