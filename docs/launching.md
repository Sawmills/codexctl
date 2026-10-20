# Launching Codex

Use `codexctl codex` for server-account launches and account recovery.
For local command pinning, see [local profiles](local-profiles.md#pin-a-local-command).

## Recovery

Use `codexctl codex` as the Codex launcher when you want account failover:

```bash
codexctl codex
codexctl codex -- "start prompt"
codexctl codex -- -C ~/Code/codexctl -m gpt-5
codexctl codex resume 019e8489-aa28-7071-ab90-16b81c7cfd1d
codexctl codex --allow-billing -- "start prompt"   # unattended: may use credits
```

The wrapper runs `codex` in a PTY and watches for this spend-cap message:

```text
You hit your spend cap set by the owner of your workspace. Ask an owner to increase your spend cap to continue.
```

Codex is launched from the current directory where `codexctl codex` was run. When detected, it
terminates that Codex process, switches to another account, then resumes with
`codex resume <session-id> "Continue the previous request."`. For a new session it discovers the
session id from the new Codex session file created under `~/.codex/sessions/`; for an existing
session, pass `resume <session-id>` so the wrapper can recover without discovery.

Local account selection during spend-cap recovery:

- **Never** switches to usage-based or unknown-billing accounts.
- Prefers accounts with headroom that cannot bill credits, ordered by the next
  weekly reset. See [reset-aware selection](resets.md#reset-aware-selection).
- When only credit-billing accounts remain (spend cap not reached, so they draw credits past
  100%), it asks for confirmation before switching, and refuses on a non-interactive terminal.
  Pass `--allow-billing` to approve those switches without prompting (e.g. for unattended runs).

When the account server is active, `codexctl codex` spreads a new launch across
included accounts. It chooses the account with the fewest live token sessions,
then the lowest recent 429 rate. Rate collection has a five-second timeout; if it
times out, selection continues without 429 data. Usage-based accounts and exhausted windows are
never selected automatically. The selected account is pinned to this child, so
starting a new lane does not move existing sessions on the machine. If Codex stops after `exceeded retry limit, last
status: 429`, codexctl resumes the same session on the next least-loaded included
account, up to three recoveries per hour. A recovery keeps the original model,
permissions, working directory, and other arguments. Automatic selection and
429 recovery never redeem resets or select credit-billing accounts, even with
`--allow-billing` or `--allow-resets`. Explicit `--account` keeps its billing policy.
If no fresh included launch can be prepared, codexctl warns and launches the current
account through the existing provider. This fallback keeps that account's existing
approval; it does not grant new billing approval or redeem a reset.

## Pin a Server Account

For one server-account lane, use `codexctl codex --account <alias> [codex args...]`,
including `resume <session-id>`. It preserves the host account and keeps this session
pinned when other lanes switch. Billing accounts require terminal approval or
`--allow-billing`. An exhausted account requires `--allow-billing` to run on ChatGPT credits; no reset is redeemed.
See [server-account launches](central-server.md) for policy and exit codes.

`codexctl rate [--json] [--minutes 10]` reports successful responses, 429s, their ratio,
observed processes/PIDs, and weekly usage from read-only Codex logs and account status.
Uncertain attribution has its own row. Token-per-minute data is not available.

## Cap Local Codex State

`codexctl state cap` reports the size of the local Codex state and is a dry run by default:

```bash
codexctl state cap --max-bytes 21474836480
```

Pass `--apply` only from the reviewed Mac schedule. It checkpoints SQLite only when the database,
WAL, and shared-memory files have no holders, and compresses only rollout files older than the
configured age. Each archive is indexed with its original path and SHA-256. Before
`codexctl codex resume <id>` launches Codex, an indexed archive is restored and verified.
For `resume --last`, all indexed archives are restored under one maintenance lock before
Codex selects the session, with native arguments unchanged. The full expansion must fit
with a 1 GiB free-space reserve; each restore checks that reserve again. If space is short,
the launch fails with an error. This can temporarily grow the state directory by several
cold sessions. The same restore policy applies to `codexctl exec --account <alias> -- codex resume`.
A held or changed file is skipped or refused. Failures before source deletion keep
the original. After deletion, verified content remains in the durable archive and index.
Sizing and checkpointing use the same inventory of numbered `logs_*.sqlite` and
`thread_history_*.sqlite` databases and their WAL/SHM sidecars. A malformed archive
index refuses resume before Codex starts. Archive and index directories are synced
before source deletion; the source directory is synced after deletion.

The reviewed daily LaunchAgent is
[`deploy/macos/ai.sawmills.codexctl.state-cap.plist`](../deploy/macos/ai.sawmills.codexctl.state-cap.plist). Install it only after the guide's CX-LD
notice, then use `launchctl bootout gui/$(id -u) "$HOME/Library/LaunchAgents/ai.sawmills.codexctl.state-cap.plist"`
to roll it back. The plist uses a 20 GiB state cap and a 15 GiB free-space target.
Below that target it still reclaims eligible state. Compression requires the source
file's size plus a 1 GiB temporary reserve in free space.
