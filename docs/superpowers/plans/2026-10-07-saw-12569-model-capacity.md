# SAW-12569 plan: retry 429 and "model at capacity" inside the request

Linked: SAW-12544 (429 bursts). Design decision (guide + w9C:tF, 2026-10-07 19:5x PDT): codexctl retries in-request only, up to about 10 minutes, and never sends `/goal resume`. The board resumes after a stop.

## Findings (Codex rust-v0.161.0, same on origin/main e1b5b56)

1. "Selected model is at capacity" is `CodexErr::ServerOverloaded`. It comes from an SSE `response.failed` event with code `server_is_overloaded` (`codex-api/src/sse/responses_error.rs:87`) or from HTTP 503 with that code (`codex-api/src/api_bridge.rs:101-106`).
2. "exceeded retry limit, last status: 429" is `RetryLimit`. Every non-usage HTTP 429 maps to it on the first attempt (`api_bridge.rs:181-240`). The request layer never retries 429: `retry_429: false` is hardcoded (`model-provider-info/src/lib.rs:455-460`). The words "retry limit" do not mean that retries ran.
3. The turn retry loop (`core/src/responses_retry.rs:88`) retries `ServerOverloaded` and `RetryLimit` only when the server sent Retry-After (`protocol/src/error.rs:416-418`). Without it, `retry_delay` returns `None` and the turn ends at once.
4. So the config knobs do not cover (1). codexctl already writes `request_max_retries = 12` and `stream_max_retries = 12` for `codexctl-central` (`src/central/native.rs:1553-1564`). Those reach 5xx and dropped streams, not these two errors. HTTP 503 overloaded is already retried by `request_max_retries`; the SSE form and 429 are not.
5. Lanes run plain `codex` with the `codexctl-central` provider (`lane-fresh.sh`), not `codexctl codex`. A fix in the `codexctl codex` launcher does not reach them. Custom providers use HTTPS SSE, not WebSockets (`supports_websockets` defaults to false).

## Options

| Option | Gain | Cost and risk |
|---|---|---|
| A. codexctl loopback relay (recommended) | codexctl already owns `base_url` for every central lane. Fix ships with a codexctl release. | New host process. If it is down, lanes get connection errors (Codex retries those). About 500 lines plus tests. |
| B. Codex upstream patch: backoff for `ServerOverloaded`/`RetryLimit` without Retry-After | Right layer, about 20 lines | Lanes run brew Codex; release timing is not ours. Upstream issue or PR is an outward message (Amir's call). |
| C. Config only | No code | Does not work (finding 4). |

## Design (Option A)

1. `codexctl relay serve`: binds 127.0.0.1 only. It forwards only to `https://chatgpt.com/backend-api/codex/*`, never to another host. One relay per host, run by launchd (Mac) or `systemd --user` (devbox). `codexctl relay install|status|uninstall`.
2. `codexctl relay enable` sets `model_providers.codexctl-central.base_url` to the relay. `disable` restores the upstream URL. Activation keeps the relay URL when the relay is enabled. Lanes pick it up on their next Codex start.
3. Retry classes, decided before any byte reaches Codex:
   - HTTP 429 that is not `usage_limit_reached`, `usage_not_included` or `insufficient_quota`. Those pass through at once, so the quota guard and account moves still see them.
   - HTTP 503 `server_is_overloaded`.
   - SSE `response.failed` with `server_is_overloaded`, `rate_limit_exceeded` or `slow_down`, when it arrives before the first event other than `response.created` or `response.in_progress`. The relay holds those head events until it decides. An error after output passes through unchanged.
4. Backoff: honor Retry-After. Else 2 s base, factor 2, cap 60 s, full jitter. Total budget 10 minutes (config `relay.retry_budget_secs`). When the budget ends, the relay passes the last upstream failure through byte for byte. Codex shows the normal stop and the board takes over. No fabricated success.
5. Same model, same account. The relay never changes the request body or the auth headers. A fallback model is out of scope for v1 (rule 45). If HQ wants one later, it is an explicit config key, logged, and counted with `outcome="fallback"`.
6. Account spread: capacity errors are per model, so a move does not help them. Per-account 429 after the budget goes to the board (3 resumes per hour, then one account move). The relay does no account moves.
7. Metrics and logs: one event per request that hit a retry class, not per attempt. Counter `codexctl_relay_capacity_events_total{kind, model, account_class, outcome}`. `kind` is `rate_429` or `overloaded`. `model` comes from an allowlist, else `other`. `account_class` is `included`, `credit` or `unknown`. `outcome` is `recovered`, `exhausted` or `terminal_passthrough`. One structured JSON log line per event carries the request id, attempts, and wait time, and never a token. The relay serves `/metrics` on loopback. Where the fleet scrapes it is open point 2.

## Tests first (mock upstream, real relay)

1. 429 rate, then 200 stream: the client sees only the 200 stream; one `recovered` event.
2. SSE `server_is_overloaded` before output, then success: the client sees one clean stream.
3. `response.failed` after an output delta: passthrough, no retry.
4. 429 `usage_limit_reached`: immediate passthrough, `terminal_passthrough`.
5. Budget exhausted: the last failure is passed byte for byte; one `exhausted` event.
6. Retry-After is honored. Jitter stays inside its bounds.
7. Enable and disable rewrite only `base_url`; activation keeps the relay URL.
8. The relay refuses a non-loopback bind and any other upstream host.
9. No log line contains `Authorization` or the token.
10. Build check: a real `codex` binary waits 120 s for response headers from the relay without a client timeout. If it times out, the relay sends headers early and the plan changes.

## Rollout

Release, then enable on one devbox host, then all hosts. Proof: a 3-hour window with at least one capacity event shows `recovered` events and no lane stopped over 5 minutes. The 15-minute stopped-lane alert belongs to the board (the guide).

## Open points for HQ

1. Option A (relay) or B (upstream Codex patch), or A now and B as an upstream issue? Recommended: A now; B needs Amir's word because it is outward.
2. Metrics destination: (a) relay `/metrics` plus JSONL only, the board reads it; (b) the relay also reports events to the central server, which adds the counter to its existing `/metrics` for a VictoriaMetrics alert. Recommended: (a) in this PR, (b) as a follow-up.
3. The `codexctl codex` launcher already relaunches with a recovery prompt after 429 (`src/commands/codex.rs:295-392`). That conflicts with "codexctl never resumes". Keep it unchanged (no central lane uses the launcher), or remove it? Recommended: keep it in this PR and open a follow-up ticket.
4. Retry budget 10 minutes is above Codex's 5-minute stream idle timeout. The relay holds no stream open while it waits before headers, so the idle timer does not apply (test 10 proves it). Confirm 10 minutes, or 4 minutes so the board's 1 to 5 minute resume stays inside the 5-minute goal?
