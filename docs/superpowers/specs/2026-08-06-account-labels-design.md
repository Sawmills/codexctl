# Account Labels: Distinguishing Profiles That Share One Email

## Problem

A profile is a directory named by its alias under `~/.codexctl/profiles/<alias>/`, and `meta.json`
records only `alias`, `email`, `plan`, and `saved_at`. The alias is already arbitrary, so two
accounts can already coexist. Nothing, however, records *which account is which*.

This breaks down when one person holds two accounts on one email address — a personal account and a
seat in a team or business workspace. Both profiles report the same `email`. `codexctl list` prints
only the alias and the plan. The alias string becomes the sole signal, and a plan value alone does
not say "team" or "personal".

Two existing behaviors turn that from an inconvenience into a hazard:

1. `codexctl save` with no alias defaults the alias to the detected email. For a second account on
   the same email, that targets the *existing* profile and offers `Overwrite? [y/N]`. Answering `y`
   destroys the stored tokens of the first account.
2. `profile::alias_for_auth_json_from` identifies the live `~/.codex/auth.json` by exact token value
   and, when the token has rotated, falls back to the `sub` claim alone. Two profiles that share one
   OpenAI login make that fallback ambiguous, so it returns `None` and the rotated tokens are never
   captured back into the profile. The profile later reports `expired` for no visible reason.

## Design

Record identity that the account itself asserts, and let the operator attach a human name on top of
it. Keep the alias as the only key and the only selector.

### Identity comes from the token, not the network

The Codex access token is a JWT whose claims already carry every fact needed:

| Claim                                         | Field                 | Meaning                        |
| --------------------------------------------- | --------------------- | ------------------------------ |
| `https://api.openai.com/profile`              | `email`, `name`       | who the human is               |
| `https://api.openai.com/auth`                 | `chatgpt_account_id`  | which workspace                |
| `https://api.openai.com/auth`                 | `chatgpt_user_id`     | which login                    |
| `https://api.openai.com/auth`                 | `chatgpt_plan_type`   | the plan                       |

A new `api::token_identity(token) -> Option<TokenIdentity>` decodes the payload once and returns
these together. It reuses the existing private `decode_jwt_payload`.

This replaces two unreliable sources:

- `commands::save::fetch_email` performs a live `GET /backend-api/me`. That call is best-effort and
  swallows every failure into `None`. It is also not dependable in practice: the endpoint answers
  `403` with an HTML challenge page to a plain client. It remains only as a fallback when the token
  carries no profile claim.
- `commands::login::email_from_alias` infers the email by testing the alias for an `@`. An alias
  such as `amir-team` therefore stores `email: null`. The claim supplies the real address instead.

Claim decoding needs no network, is deterministic, and still works on an expired token — which is
exactly when a profile most needs to stay identifiable.

### `meta.json` gains four optional fields

```json
{
  "alias": "amir-team",
  "label": "team",
  "email": "amir@sawmills.ai",
  "plan": "business",
  "account_id": "6df34c28-6856-42d4-8da8-a664980ec77d",
  "user_id": "user-uT4WAfEcoEA4I0gJ1ZM853Cq",
  "saved_at": "2026-08-06T18:00:00+00:00"
}
```

| Field        | Source                    | Written by                       |
| ------------ | ------------------------- | -------------------------------- |
| `label`      | the operator              | `label`, `--label`               |
| `account_id` | `chatgpt_account_id`      | `save`, `login`                  |
| `user_id`    | `chatgpt_user_id`         | `save`, `login`                  |
| `email`      | profile claim, then `/me` | `save`, `login`                  |
| `plan`       | unchanged                 | `save`, `login`, `status`        |

Every new field is `Option<T>` with `#[serde(default)]`, so a `meta.json` written by an earlier
version parses unchanged and simply reports no label. No migration step runs, and no existing
profile is rewritten until its next `save`, `login`, or `status`.

`account_id` and `user_id` are workspace and user identifiers, not credentials. They are already
present in plaintext inside the stored `auth.json`.

### Setting the label

```
codexctl label <alias> [text]     # set; omit text to clear
codexctl login --label <text> <alias>
codexctl save  --label <text> [alias]
```

`label` resolves the profile through `store::profile_dir`, so it inherits the existing path,
case-collision, and symlink checks. It takes the store lock and writes `meta.json` through
`store::atomic_write`, matching how every other mutation is persisted.

Label validation mirrors `store::validate_alias` minus the path rules, because a label is display
text and never a path component:

- trimmed; an empty result clears the label
- at most 40 bytes
- ASCII only
- no control characters

A label is deliberately **not** required to be unique. It is display text, so two profiles may both
read `team` without creating an unresolvable name.

### The label never becomes a selector

`use`, `remove`, `reset`, and `label` continue to accept the alias only. One namespace means no
resolution order, no ambiguity rule, and no path by which a mistyped label switches the wrong
account. The label reaches selection only through the `switch` fuzzy picker, where it is part of the
matched display string and the operator confirms the highlighted row before anything changes.

### Display

`list` currently pads columns by hand with `{:<width$}`. It moves to `comfy_table` with the
`UTF8_FULL_CONDENSED` preset already used by `status`, so both commands render the same way.

```
$ codexctl list
┌───────────────────┬──────────┬──────────┬────────────────────┬────────┐
│ Account           ┆ Label    ┆ Plan     ┆ Email              ┆ Active │
╞═══════════════════╪══════════╪══════════╪════════════════════╪════════╡
│ amir@sawmills.ai  ┆ personal ┆ pro      ┆ amir@sawmills.ai   ┆        │
│ amir-team         ┆ team     ┆ business ┆ amir@sawmills.ai   ┆ *      │
└───────────────────┴──────────┴──────────┴────────────────────┴────────┘
```

The `Label` column appears in `list` and `status` only when at least one profile carries a label.
Until the feature is used, both tables keep their current shape. This mirrors how
`RateLimitColumns::for_accounts` already adds the `Limit` column only when some account needs it.

Colors follow the existing `status` conventions: the active marker and the label render in
`Color::Cyan`, and no color is applied to a cell whose meaning is not a severity.

`whoami` gains the label in the same position:

```
$ codexctl whoami
amir-team — team (amir@sawmills.ai) [business]
```

`switch` appends the label to each picker row so that typing `team` selects it:

```
amir-team — team (amir@sawmills.ai) [business] — 5h: 2%, 7d: 16%
```

A profile with no label renders `-` in a table cell and omits the ` — label` segment in the
single-line forms, so nothing shifts for an unlabeled store.

### Refusing to overwrite a different account

`save` resolves its alias, then compares the incoming token's `account_id` against the `account_id`
stored in the target profile's `meta.json`:

| Stored               | Incoming | Result                                     |
| -------------------- | -------- | ------------------------------------------ |
| absent (old profile) | any      | today's `Overwrite? [y/N]` prompt          |
| any                  | absent   | today's `Overwrite? [y/N]` prompt          |
| equal                | equal    | today's `Overwrite? [y/N]` prompt          |
| different            | known    | refuse, and name an explicit alias to pass |

A refusal needs positive evidence of a *different* account. Whenever either identifier is missing,
the command falls back to the existing prompt rather than blocking a legitimate re-save.

The refusal is an error, not a prompt, because the destructive answer is a single keystroke and the
correct action is always to choose a different alias:

```
$ codexctl save
error: profile 'amir@sawmills.ai' holds a different account
       (stored workspace 033569a0…, incoming 6df34c28…).
       Pass an explicit alias: codexctl save <alias>
```

This is the check that makes adding a second account on one email safe, so it belongs to this work
rather than to a later cleanup.

### Disambiguating the rotated-token fallback

`alias_for_auth_json_from` keeps its exact-token-match pass unchanged. Its `sub` fallback gains
`account_id` as a second key: a candidate matches when the subject matches **and** the account
identifiers either both exist and are equal, or at least one is absent.

Two profiles for one login in two workspaces therefore stay distinguishable, and
`capture_auth_file_profile_tokens` keeps folding rotated tokens back into the right profile. When
either side lacks the claim, the behavior is exactly today's, so no existing store regresses.

## Testing

Focused unit tests:

- `token_identity` extracts email, account id, user id, and plan from a synthetic JWT, and returns
  `None` for a malformed or claimless token.
- Label validation accepts a normal label, trims, clears on empty, and rejects over-length,
  non-ASCII, and control-character input.
- `alias_for_auth_json_from` returns the right alias for two profiles that share a `sub` and differ
  by `account_id`, and preserves today's result when a claim is absent.
- `Meta` deserializes a `meta.json` that predates the new fields.

CLI tests:

- `label` sets, overwrites, and clears; it fails on an unknown alias.
- `login --label` and `save --label` persist the label.
- `list` and `status` omit the `Label` column with no labels present and include it once one is set.
- `save` refuses when the target profile holds a different `account_id`.

Per `AGENTS.md`, no real token value enters a fixture. Test tokens are unsigned JWTs carrying only
synthetic claims, following the existing `JWT_HDR` pattern in `commands/status.rs`.

## Out of scope

- Renaming an existing profile. The alias remains fixed after creation; a rename would have to move
  the profile directory, the login home, and the active pointer together, which is its own design.
- Selecting a profile by label. Recorded here as a deliberate rejection, not an oversight.
- Any change to automatic selection, billing approval, or reset redemption. Labels are descriptive
  and never influence which account `use` picks.
