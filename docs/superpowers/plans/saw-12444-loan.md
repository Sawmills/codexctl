# Plan: Loan a Server Account to a Teammate (SAW-12444)

A lender grants one server account to one borrower until the account's next
weekly reset or an earlier end time. The account server then issues access
tokens to every machine of the borrower. The design is in
[ADR 0004](../../adr/0004-loan-a-server-account-to-a-teammate.md). HQ approved
this plan with changes L1 to L8 on 2026-10-06; this version includes them.

## Surface

CLI (new `codexctl loans` command group; client code in a new module, not in
`src/central/remote.rs`, which another lane edits now):

| Command | Who | Effect |
|---|---|---|
| `codexctl loans lend <alias> --to <email> [--until <RFC 3339>]` | lender | Creates the grant; prints the borrowed reference and end time |
| `codexctl loans list` | both | Lists active and recent loans, lent and borrowed |
| `codexctl loans end <id>` | both | Ends the loan (`revoked` by lender, `returned` by borrower) |
| `codexctl loans audit [<id>]` | both | Shows the audit events for loans the caller is part of |

No accept step: the lender's grant is the approval (Q2). The borrower selects
the account by its borrowed reference `<lender>/<alias>`.

Account server (handlers and grant rules in a new `src/central/loans.rs`;
storage in `src/central/storage/loans.rs`):

| Route | Purpose |
|---|---|
| `POST /v1/loans` | Grant. Lender only. Refuses a self-loan, an unknown or disabled borrower, a second active grant, and stale usage or a past weekly reset |
| `GET /v1/loans` | Grants where the caller is lender or borrower |
| `POST /v1/loans/end` | End by lender or borrower; idempotent for an ended grant |
| `GET /v1/loans/audit` | Audit events, filtered to the caller's grants |

## Seams (L1, L5, L6, L7)

- `owner()` and `resolve_alias` stay unchanged (L1). A new
  `Broker::borrowed_owner(device, reference)` resolves the borrower's active
  grant to the lender's `Owner`. Only `/v1/token` and `account_catalog` call
  it. All other `owner()` callers keep answering 404 for a borrowed reference:
  reset redeem (`resets.rs:290`), login renewal start, status, and cancel
  (`relogin/http.rs:14,48,112`), and import.
- In PostgreSQL mode `borrowed_owner` loads the lender's account through
  `resolve_alias(lender, alias)`, which calls
  `load_account_by_alias(lender, alias)` (L7). A missing lender alias ends the
  grant with reason `account_removed`.
- Recovery seams (L5): server-account recovery and resets use
  `remote::select_for_activation` (`remote.rs:1367`, called at
  `native.rs:1057`), `remote::select_for_codex`, and
  `remote::find_rate_limit_recovery_candidate`. Local-profile recovery
  (`commands/codex.rs` `recovery_plan`, `use_profile::ResetPlan`) never sees a
  server account and needs no change.
- Client names (L6): a typed `AccountRef { Owned, Borrowed }` parses a name;
  `store::validate_alias` stays strict. The touched commands are `codexctl use`
  (`native::activate`, connection file and active pointer), `codexctl codex
  --account` (`prepare_pinned_codex`), and the status line (active pointer
  read).

## Rules

- **Token issue (L2):** `/v1/token` checks the grant before the refresh and
  again after it, next to the existing machine re-authorization. An ended or
  expired grant gives 403 `loan_ended`. A disabled lender or a changed
  credential subject gives 409 `loan_paused`. A running lane shows the error
  and never moves to another account by itself.
- **Revoke:** no new tokens after the end. Issued tokens live until their
  OpenAI `exp`. The CLI says this and names login renewal as the hard cut.
- **Borrowed reference (Q2):** built and stored at grant time, lowercase, each
  half validated as a path component. Resolved only through the borrower's
  active grants. More than one match gives 409 `ambiguous_loan`.
- **Quota (Q1):** the borrower's selection ranks owned accounts before borrowed
  ones. Auto-select and recovery skip a borrowed account when any window is at
  or above 95 percent. An explicit `--account` still works until the window is
  exhausted.
- **Billing:** same as an owned account. Auto-select and recovery pick only
  verified included usage, never a usage-based account. Credits need
  `--allow-billing` on the borrower's machine (Q6).
- **Resets:** only the lender redeems. `select_for_activation` skips borrowed
  accounts as reset candidates; the server answers 404 through `owner()`.
- **Live sessions (L3):** a borrower's session uses the lender's account key
  with the borrower's user ID, so the PostgreSQL foreign key holds and both
  users count. In file mode the token issue audit events are the usage record.
- **Storage (L8):** file mode adds `loans` and `loan_audit` to the encrypted
  `FileState` under `central-storage.lock`. PostgreSQL adds `account_loans`
  and `account_loan_audit`. Dual mode writes PostgreSQL first, then the file.
  Loans do not use `reject_unshared_workflow`.
- **Retention (L4):** every loan write prunes grants that ended more than 90
  days ago and audit events older than 90 days, in both modes. Token issue
  events coalesce on (grant, machine, UTC hour): a unique index in
  PostgreSQL, a key check before the append in file mode.
- **Failures:** loan handlers record failures with `record_failure` and
  bounded `operation`, `stage`, and `reason` labels.

## Tests (red first, one seam at a time)

1. Grant rules: self-loan, unknown borrower, second active grant, `--until`
   after the weekly reset, stale usage or a past `resets_at`, non-lender.
2. Lender-only paths: a borrowed reference gets 404 from reset redeem, login
   renewal start, status, and cancel, and import.
3. Token: the borrower gets a token while active; `loan_ended` after end and
   after expiry; `loan_paused` after lender disable and after a subject change;
   no token when the loan ends during the refresh (race test); a revoked
   borrower machine gets 401.
4. Reference: a lender email change and a new same-local-part user do not
   redirect a grant; two matching grants give `ambiguous_loan`.
5. Catalog: the borrowed account shows `loan`, `user_id` is the borrower, and
   live sessions count both users.
6. Client selection: owned before borrowed; skip borrowed at 95 percent; never
   a usage-based borrowed account; no reset on a borrowed account.
7. Client names: `AccountRef` parse, the borrowed connection path, and the
   active pointer round trip.
8. Storage: file round trip, one active grant, coalescing, 90-day prune.
   PostgreSQL tests use the `central-real-db-tests` feature.
9. CLI: `loans lend`, `list`, `end`, and `audit` against a synthetic account
   server with synthetic users only.

Gates on the Mac: `CARGO_BUILD_JOBS=2` for `cargo fmt --all -- --check`,
`cargo clippy --all-targets`, and `cargo test --all-targets -- --test-threads=2`.
The staging proof (one borrower request) waits for HQ approval of a real loan.
