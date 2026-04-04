# codexctl — Codex CLI Account Manager

## Purpose

A Rust CLI tool for managing multiple OpenAI Codex CLI accounts. Solves the pain of manually swapping `~/.codex/auth.json` across 7+ accounts and having no unified view of rate limit status.

## Commands

| Command | Description |
|---|---|
| `codexctl status` | Table of all accounts: alias, email, plan, 5h usage%, 7d usage%, reset times |
| `codexctl save [alias]` | Save current `~/.codex/auth.json` as a named profile. Auto-detects email from token; optional alias override. |
| `codexctl use <alias>` | Direct switch — copies profile's auth.json into `~/.codex/auth.json` |
| `codexctl switch` | Interactive fuzzy picker to select and switch account |
| `codexctl list` | List all saved profiles (alias, email, active marker) |
| `codexctl remove <alias>` | Delete a saved profile |
| `codexctl whoami` | Show currently active account (alias + email + plan) |
| `codexctl completions <shell>` | Generate shell completions (zsh/bash/fish) |

## Data Layout

```
~/.codexctl/
  active                    # plain text file containing the active profile alias
  profiles/
    work-main/
      auth.json             # copy of ~/.codex/auth.json at save time
      meta.json             # { "alias": "work-main", "email": "...", "plan": "pro", "saved_at": "..." }
    personal/
      auth.json
      meta.json
    ...
```

## Profile Management

### Save (`codexctl save [alias]`)

1. Read `~/.codex/auth.json` — fail if missing or unparseable.
2. Extract identity: call `GET https://chatgpt.com/backend-api/me` with the access token to get email. If the call fails (expired token, network), prompt user for an alias instead of auto-detecting.
3. If no alias provided and email retrieved, derive alias from email local part (e.g., `amir@sawmills.ai` -> `amir-sawmills`).
4. If alias already exists, confirm overwrite.
5. Copy `auth.json` into `~/.codexctl/profiles/<alias>/auth.json`.
6. Write `meta.json` with alias, email (if known), and timestamp.
7. Set as active in `~/.codexctl/active`.

### Switch (`codexctl use <alias>`)

1. Verify profile exists in `~/.codexctl/profiles/<alias>/`.
2. Copy `profiles/<alias>/auth.json` to `~/.codex/auth.json`.
3. Update `~/.codexctl/active` to `<alias>`.
4. Print confirmation: `Switched to <alias> (<email>)`.

### Interactive Switch (`codexctl switch`)

1. Load all profiles from `~/.codexctl/profiles/`.
2. Present interactive fuzzy-filterable list using `dialoguer::FuzzySelect`.
3. Each item shows: `alias (email) [plan] — 5h: XX% used, 7d: YY% used`.
4. On selection, perform the same switch as `codexctl use`.

### Status (`codexctl status`)

1. Iterate all profiles in `~/.codexctl/profiles/`.
2. For each profile, call the rate limit API using that profile's access token.
3. Collect results (parallel with `tokio` or sequential — start sequential, optimize later).
4. Render table:

```
 Account       Email                Plan   5h Used   5h Reset      7d Used   7d Reset       Active
 work-main     amir@sawmills.ai     Pro    27%       in 3h 12m     46%       in 4d 2h       *
 personal      amir@gmail.com       Plus   0%        in 4h 58m     12%       in 6d 1h
 work-2        dev@sawmills.ai      Pro    89%       in 1h 03m     71%       in 2d 5h
```

5. If a token is expired (401/403), show `expired` in the status columns instead of failing.

## Rate Limit API

### Endpoint

`GET https://chatgpt.com/backend-api/wham/usage`

### Auth

`Authorization: Bearer <access_token>` from the profile's `auth.json`.

### Response (relevant fields)

```json
{
  "plan_type": "pro",
  "rate_limit": {
    "limit_id": "codex",
    "limit_name": "Codex",
    "primary": {
      "used_percent": 27.0,
      "window_minutes": 300,
      "resets_at": 1743789600
    },
    "secondary": {
      "used_percent": 46.0,
      "window_minutes": 10080,
      "resets_at": 1744137600
    }
  },
  "credits": {
    "balance": 0.0
  }
}
```

- `primary` = 5-hour window (300 minutes)
- `secondary` = 7-day window (10080 minutes)
- `resets_at` = unix timestamp

## Shell Completions

Use `clap_complete` with a custom completer for profile-aware tab completion:

- `codexctl use <TAB>` — lists all profile aliases from `~/.codexctl/profiles/`
- `codexctl remove <TAB>` — same
- `codexctl completions <TAB>` — lists shells: `zsh`, `bash`, `fish`

Generate via:
```bash
codexctl completions zsh > ~/.zfunc/_codexctl
```

The completer reads the profiles directory at completion time, so new profiles are immediately available.

## Dependencies

| Crate | Purpose |
|---|---|
| `clap` + `clap_complete` | CLI parsing + shell completions |
| `reqwest` (blocking) | HTTP calls to rate limit API |
| `serde` + `serde_json` | JSON parsing |
| `comfy-table` | Status table rendering |
| `dialoguer` | Interactive fuzzy picker |
| `chrono` | Timestamp formatting for reset times |
| `anyhow` | Error handling |

## File Structure

```
codexctl/
  Cargo.toml
  src/
    main.rs              # CLI entry point, clap definition
    commands/
      mod.rs
      save.rs            # profile save logic
      use_profile.rs     # direct switch
      switch.rs          # interactive picker
      status.rs          # rate limit table
      list.rs            # list profiles
      remove.rs          # delete profile
      whoami.rs          # current account info
      completions.rs     # shell completion generation
    profile.rs           # Profile struct, load/save, meta.json
    api.rs               # rate limit API client
    config.rs            # paths (~/.codexctl, ~/.codex), constants
```

## Edge Cases

- **Expired token**: `status` shows "expired" instead of crashing. `use` still switches (user may re-auth via `codex --login`).
- **No profiles saved**: `status`, `switch`, `list` print helpful message pointing to `codexctl save`.
- **Active profile deleted externally**: `whoami` detects mismatch and reports it.
- **Concurrent codex sessions**: switching affects the next codex session started, not running ones.
- **Token refresh**: out of scope — Codex CLI handles its own token refresh. We just store and swap the auth.json as-is.

## Non-Goals

- Token refresh / OAuth flow — Codex CLI owns this
- Auto-rotation based on rate limits — possible future feature
- Config.toml management — only auth.json switching for now
- Keyring integration — file-based auth.json only
