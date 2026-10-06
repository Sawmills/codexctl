# A server account belongs to one company user

Superseded by [ADR 0004](0004-loan-a-server-account-to-a-teammate.md): one company
user still owns a server account and can lend its use to another company user.

Each server account belongs to exactly one company user. The account server keys
it by that user and the alias. If a second company user adds the same OpenAI login,
the server reports a conflict. All machines of the owning company user can use the
account at the same time. We chose this for the first deployment because sharing
across users needs access control, audit, and rules for who may renew or revoke.

## Consequences

Shared accounts are planned for later. They need a new account key that does not
contain the company user, and a migration of existing server accounts.
