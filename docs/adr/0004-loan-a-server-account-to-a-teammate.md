# ADR 0004: Loan a Server Account to a Teammate

- **Status:** accepted (HQ plan approval with changes L1 to L8, 2026-10-06)
- **Date:** 2026-10-06
- **Ticket:** SAW-12444
- **Supersedes:** [ADR 0001](0001-server-account-has-one-company-user.md)

## Decision

A server account still belongs to exactly one company user, the one who
imported it. That company user (the lender) can grant a time-limited loan of one account to one other
company user (the borrower). A loan is a grant record. It never copies or moves
the credential, and the account stays in the lender's catalog under its
existing `account_key(user, alias)`. The account server issues access tokens
for the account to every enrolled machine of the borrower while the grant is
active. The refresh credential never leaves the server.

ADR 0001 said that one company user has sole use of a server account. This ADR
replaces that rule: one company user owns the account and can share use of it
through loans. Ownership, the account key, and the 409
`account_already_owned` conflict for a second import do not change.

## Context

Amir approved the build on 2026-10-06 and answered the open questions (Q1 to
Q13 on SAW-12444). He accepts the OpenAI terms risk: a seat licensed to one
person and used by another can lead to suspension of the lender's seat. Today
`resolve_alias` (`src/central/managed.rs:606`) skips every account of another
user, and `account_catalog` (`src/central/managed.rs:1475`) lists only the
caller's accounts, so a borrower has no path to a loaned account.

## Grant Record

A grant has these fields:

| Field | Meaning |
|---|---|
| `id` | Random grant ID |
| `account_key` | The lender's existing account key |
| `lender`, `borrower` | Company-user IDs |
| `alias` | The lender's alias at grant time |
| `subject` | Digests of the credential workspace and of each login claim (`uid`, `sub`) at grant time |
| `created_at`, `ends_at` | Start and end; `ends_at` is at or before the account's next weekly reset |
| `ended_at`, `ended_by`, `end_reason` | Set once, when the loan ends (`revoked`, `returned`, `expired`) |

Rules for grants:

- The lender creates the grant. The lender's own request is the approval; no
  administrator can grant a loan, and the borrower does not accept it.
- The default and the maximum `ends_at` is the account's next weekly reset. If
  the server does not know that reset time, it refuses the grant.
- One account has at most one active grant. A renewal is a new grant after the
  old one ends. A borrower cannot lend a borrowed account.
- Either party can end the grant. The end is final.

## How a Borrowed Account Resolves

The borrower addresses a loaned account as `<lender>/<alias>`, the borrowed
reference. The server builds it once, at grant time, from the lowercase local
part of the lender's company email and the lender's alias, and stores it on the
grant. It is a display name only: the server resolves it through the
borrower's active grants, never through the user table, so a later email change
or a new user with the same local part cannot redirect it. If more than one
active grant of the borrower matches, the server answers 409 `ambiguous_loan`.
Each half is validated as a path component, and aliases cannot contain `/`
(`src/store.rs:34`), so a borrowed reference never collides with an owned
alias.

The client parses an account name into a typed `AccountRef` (`Owned` or
`Borrowed`) instead of loosening `store::validate_alias`. A borrowed account
keeps its local connection file under `central/borrowed/<lender>/<alias>.json`,
outside the owned alias namespace.

`resolve_alias` and `owner()` do not change, so every caller that must be
lender-only (reset redeem, login renewal, import, machine revoke) keeps
answering 404 for a borrowed reference. A separate `borrowed_owner()` resolves
an active grant to the lender's existing `Owner`. Only `/v1/token` and the
catalog call it, so the refresh lease, the credential revision, and the refresh
path stay single. In PostgreSQL mode it loads the lender's account with
`load_account_by_alias(lender, alias)`. If the lender's alias no longer
resolves, the grant ends with reason `account_removed`. A re-import under the
same alias before that check makes the same account key, so the grant goes on
only while the login claims agree; a different login pauses it.

## Who May Do What

| Action | Lender | Borrower |
|---|---|---|
| Get an access token (`/v1/token`) | yes | yes, while the grant is active |
| See the account in the catalog | yes | yes, marked as loaned, with lender and end time |
| Select it with `codexctl use` or auto-select | yes | yes, same rules as an owned account |
| Spend credits | with `--allow-billing` | with `--allow-billing` |
| Redeem a banked reset | yes | no |
| Login renewal, import, or `codexctl devices --revoke` | yes | no |
| End the loan | yes | yes |
| Read the loan audit log | yes | yes |

A borrowed account follows the same selection rules as an owned account.
Recovery and auto-select pick it only with verified included usage and never
pick a usage-based account. Credit billing needs explicit approval, and reset
approval stays separate from billing approval.

## Revoke Semantics

`/v1/token` returns the raw ChatGPT access token, so the server cannot cancel a
token it already issued. After a loan ends, the server issues no new token to
the borrower. The token path checks the grant before the refresh and again
after it, next to the machine re-authorization, so a loan that ends during a
slow refresh delivers no token. A token already issued stays valid until its
OpenAI `exp`. A lender who needs a hard cut must run a login renewal, which also invalidates
the lender's own sessions. The borrower's running lane gets a clear `loan_ended` error at
its next token request and never moves to another account without a new
selection.

A loan pauses, with the error `loan_paused`, when the lender's user is disabled
or the token's login claims do not positively agree with the grant's `subject`,
for example after a login renewal to another login. The claims are compared
inside one namespace, so a token that gains a `uid` still matches through its
`sub`. A pause issues no tokens and does not end the grant.

## Storage

Grants and the audit log work in file mode and in PostgreSQL mode. Dual mode
writes PostgreSQL first and mirrors to the file, like other records.

- **File mode:** the encrypted `FileState` in `central-storage.enc` gets two
  new fields, `loans` and `loan_audit`, written under `central-storage.lock`.
  No plaintext loan file exists. An older binary that rewrites this file drops
  the two fields, so a rollback ends every loan.
- **PostgreSQL mode:** the `account_loans` table holds the grants. A partial
  unique index on `account_id` where `ended_at IS NULL` enforces one active
  grant. The `account_loan_audit` table holds the audit events.

The audit log records grant, token issue, end, expiry, and pause events. A
token issue event has a coalescing key of grant, machine, and UTC hour.
PostgreSQL enforces the key with a unique index and `ON CONFLICT DO NOTHING`;
file mode skips an append whose key is already in `loan_audit`. Each grant
and each end prunes grants that ended more than 90 days ago and audit events
older than 90 days, in both modes.

Live sessions of a borrower use the lender's account key with the borrower's
user ID, so the PostgreSQL foreign key holds and both users count in the
catalog load. File mode has no live-session store (`record_live_session` is a
no-op there), so the token issue events in the loan audit are the file-mode
usage record.

## Consequences

- Both users share one rate-limit window and can cause each other's 429
  responses. The lender's lanes come first: the borrower's selection ranks
  owned accounts before borrowed ones, and auto-select and recovery skip a
  borrowed account when any window is at or above 95 percent use.
- The OpenAI terms risk stays with Amir's decision. A suspension of the
  lender's seat affects the lender's own work too.

## Alternatives Rejected

1. **Copy the credential to the borrower's catalog.** Two refresh owners for one
   OpenAI login rotate each other's refresh tokens out, and the copy breaks
   the 409 ownership check.
2. **A shared account key without a user.** ADR 0001 named this path. It needs a
   migration of every account and a full ACL model; a grant record gets the
   approved scope without that cost.
3. **A proxy that hides the access token.** It would allow a hard revoke, but
   every Codex request would then pass through the account server.
