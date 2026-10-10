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

The relay helps only where Codex honors retry advice. Each row was checked live with
`tests/relay_codex_e2e.py`.

| Codex           | HTTP 429                                     | Model at capacity |
| --------------- | -------------------------------------------- | ----------------- |
| 0.160.1         | Not covered: Codex treats every 429 as final | Covered           |
| 0.161.0         | Covered                                      | Covered           |
| 0.162.0-alpha.9 | Covered                                      | Covered           |

Codex 0.160.1 ignores `Retry-After` on a 429 (`protocol/src/error.rs` at
rust-v0.160.1). Codex 0.160.0 has the same rules (source check at rust-v0.160.0, not
run live). Run `codex --version` on the lane host before you opt a lane in.

To check a lane host, run the live check with that host's Codex. For 0.160.1, add
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
- **Usage, quota, billing, plan and flex 429s:** pass through unchanged, so the quota
  guard still sees them.
- **Everything else:** passes through unchanged, byte for byte. This covers 401, 403
  and 5xx, streamed rate-limit codes, and any error after output started. The relay
  records a bounded `stream_failure` observation for these errors.

Delays double from 2 s up to 60 s, with jitter. Each Codex thread (`thread-id`
header) has one failure streak. When a streak spends its budget, the relay passes the
original failure through and removes any upstream `Retry-After`. Codex then stops, and
the board resumes the lane. A new streak starts when the error kind changes, after 120 s
without a failure, or on the first failure after a stop (that request is a resume).

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

## Opt in one lane

The relay is opt-in per lane. Do not change `base_url` in a shared `config.toml`. Add
these overrides to the lane's `codex` command:

```sh
codexctl relay status || exit 1
codex ... \
  -c 'model_providers.codexctl-central.base_url="http://127.0.0.1:47631/backend-api/codex"' \
  -c 'model_providers.codexctl-central.stream_max_retries=20' \
  -c 'model_providers.codexctl-central.http_headers={ChatGPT-Account-ID="<id>",X-Codexctl-Account-Class="included"}'
```

- `stream_max_retries=20` lets the 600 s capacity budget end before Codex's own retry
  count does.
- `X-Codexctl-Account-Class` (`included` or `credit`) labels metrics only. The relay
  removes it before it forwards the request.

To roll back, start the lane without the `base_url` override.

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
  output), `exhausted` (one per streak that spent its budget), or
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

The staging and staging-ha Prometheus rules alert on one exhausted event in a 15-minute
window. They intentionally exclude `advised`: that outcome means the relay is still
retrying and does not need operator action. Alerts carry `severity=warning` and
`service=codexctl`; the platform Alertmanager
`warnings-slack` receiver routes them to `#warning-alerts`. The platform route file is
owned outside this repository, so `amtool` cannot validate the live route here. The
monitoring guide must run `amtool config routes test` against that platform file with
`severity=warning service=codexctl` and confirm `warnings-slack`.
