# Plan: Loan a Server Account to a Teammate (SAW-12444)

A lender grants one server account to one borrower until the account's next
weekly reset or an earlier end time. The account server then issues access
tokens to every machine of the borrower. The design is in
[ADR 0004](../../adr/0004-loan-a-server-account-to-a-teammate.md). This plan
covers the surface, the rules, and the test-first order.

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
the account as `<lender>/<alias>` with `codexctl use`, `codexctl codex
--account`, or auto-select.

Account server (new `src/central/loans.rs` for handlers and grant rules;
storage in a new `src/central/storage/loans.rs` submodule):

| Route | Purpose |
|---|---|
| `POST /v1/loans` | Grant. Lender only; refuses an unknown weekly reset, a second active grant, a self-loan, and a disabled or ambiguous borrower |
| `GET /v1/loans` | Grants where the caller is lender or borrower |
| `POST /v1/loans/end` | End by lender or borrower; idempotent for an ended grant |
| `GET /v1/loans/audit` | Audit events, filtered to the caller's grants |

Small edits to existing paths, kept narrow because of the parallel
`feat/central-add-account` lane:

- `managed.rs` `resolve_alias` and `owner()`: parse `<lender>/<alias>`, check
  the active grant, return the lender's `Owner`.
- `managed.rs` `account_catalog`: append active borrowed accounts with a new
  optional `loan` field (`lender`, `endsAt`, `id`) on `Account`.
- `managed.rs` `token`: write a coalesced `token_issued` audit event and record
  the borrower on the live session.
- Client: native homes and selection treat a `loan` account as a borrowed
  reference; local state goes under `borrowed/<lender>/<alias>`.

## Rules

- **Token issue:** each `/v1/token` call for a borrowed reference rechecks the
  grant. An ended grant gives 403 `loan_ended`; an expired one ends itself with
  reason `expired` and gives the same error. A disabled lender or a changed
  credential subject gives 409 `loan_paused`. A running lane shows the error
  and never moves to another account by itself.
- **Revoke:** no new tokens after the end. Issued tokens live until their OpenAI
  `exp`. The CLI output says this and names login renewal as the hard cut.
- **Quota:** the lender's lanes come first. The borrower's catalog marks a
  borrowed account as unavailable for auto-select when any window is at or
  above 95 percent. An explicit `--account` still works until the window is
  exhausted.
- **Billing:** the same as an owned account. Auto-select and recovery pick a
  borrowed account only with verified included usage, never a usage-based one.
  Credits need `--allow-billing` on the borrower's machine (Q6).
- **Resets:** only the lender redeems. `/v1/resets/redeem` keeps its
  `vault.user != device.user` refusal, and `select_for_activation` skips
  borrowed accounts as reset candidates.
- **Lender-only actions:** login renewal, import, and machine revoke keep
  resolving the caller's own aliases only; a borrowed reference gives 404.
- **Storage:** file mode `loans.json` plus `loan-audit.jsonl`; PostgreSQL
  tables `account_loans` and `account_loan_audit`; dual mode writes both.
  Loans do not use `reject_unshared_workflow`, so they work in every mode.
- **Audit:** grant, token issue (one row per machine, grant, and hour), end,
  expiry, pause, and session usage. Both parties read it. Rows older than 90
  days are deleted on the existing retention pass.
- **Failures:** each loan handler records failures with the existing
  `record_failure` metric, with bounded `operation`, `stage`, and `reason`
  labels.

## Tests (red first, one seam at a time)

1. Grant rules: self-loan, unknown borrower, second active grant, `--until`
   after the weekly reset, unknown weekly reset, non-lender grant.
2. Resolution: borrower resolves `<lender>/<alias>`; a third user and an ended
   grant get 404 or `loan_ended`; an owned alias never resolves to a loan.
3. Token: borrower gets a token while active; `loan_ended` after end and after
   expiry; `loan_paused` after lender disable and after a subject change.
4. Escalation: borrower gets 404 on reset redeem, login renewal, and import of
   the borrowed reference.
5. Catalog: borrowed account shows `loan`; unavailable for auto-select at 95
   percent; a usage-based borrowed account is never auto-selected.
6. Client: `select_for_activation` skips borrowed accounts for resets; native
   home paths for a borrowed reference stay under `borrowed/`.
7. Storage: file and PostgreSQL round trip, the one-active-grant index, audit
   retention at 90 days. PostgreSQL tests use the existing test-database gate.
8. CLI: `loans lend`, `list`, `end`, `audit` against a synthetic account server
   with synthetic users only.

Gates on the Mac: `CARGO_BUILD_JOBS=2` for `cargo fmt --all -- --check`,
`cargo clippy --all-targets`, and `cargo test --all-targets -- --test-threads=2`.
The staging proof (one borrower request) waits for HQ approval of a real loan.

## Open Points for the Challenge

- The challenge (item 3) said to keep borrowed accounts out of auto-select. The
  HQ decision for Q8 allows auto-select under the owned-account rules. This
  plan follows HQ and adds the 95 percent backoff.
- `<lender>` is the email local part. The server refuses a grant if two
  enabled company users share that local part.
