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
the first failure after exhaustion (that request is a resume). All streaks restart
when a new request arrives 120 s beyond the advised retry time. Active streaming
does not count as idle. Elapsed client waits count toward
exhaustion when the next failure arrives. On each new request, entries older than
an hour since their last failure are evicted to bound retained state. This can
also discard a still-active, hour-long stream's streak when another request arrives.
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
codexctl relay status   # exit 0 only when the relay answers
```

The service manager restarts the relay when it exits. A bind failure is fatal. On stop,
the relay stops accepting and drains open streams for up to 30 s. An open stream that
is cut then is retried by Codex as a dropped stream.

If the relay is down, an opted-in lane shows "Reconnecting... waiting for network" and
does not stop. Run `codexctl relay status` before you start a lane on it.

## Enable account-server routing on a host

Start the relay service first, then enable the supported host setting from an
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

The existing central capacity endpoint accepts only its four capacity labels and does
not have a `stream_failure` event schema. Stream-failure forwarding is therefore a
follow-up server API change; this release keeps the observation local to the relay.

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

Counts distinguish requests, overloads, rate limits and other failures. They cover
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
