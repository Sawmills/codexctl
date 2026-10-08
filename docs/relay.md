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
  and 5xx, streamed rate-limit codes, and any error after output started.

Delays double from 2 s up to 60 s, with jitter. Each Codex thread (`thread-id`
header) has one failure streak. When a streak spends its budget, the relay passes the
original failure through and removes any upstream `Retry-After`. Codex then stops, and
the board resumes the lane. A new streak starts when the error kind changes, after 120 s
without a failure, or on the first failure after a stop (that request is a resume).

| Error | Default budget | Flag |
|---|---|---|
| HTTP 429 rate limit | 180 s | `--rate-budget-secs` |
| Model at capacity | 600 s | `--overloaded-budget-secs` |

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
```

- `kind`: `rate_429` or `overloaded`.
- `outcome`: `advised` (one per advice), `recovered` (one per streak that ended in
  output), `exhausted` (one per streak that spent its budget), or
  `terminal_passthrough`.

This is instrumentation only. No alert reads it yet.
