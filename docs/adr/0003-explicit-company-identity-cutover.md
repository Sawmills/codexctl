# Keep company-user ownership stable across an explicitly approved SSO cutover

Switching from Clerk to Google Workspace changes the issuer/subject identity. An
administrator may allowlist a particular Clerk subject and verified company email
for one link at the first verified, hosted-domain-checked Google sign-in. Store the
replacement identity on the existing company-user record in one locked atomic
write, retaining its ID, server accounts and machine credentials. This preserves
[ADR 0001](0001-server-account-has-one-company-user.md): each server account still
belongs to exactly one company user.

We rejected automatic email-based merging because email is mutable and can be
ambiguous, and rejected rewriting account/device ownership because that spans
registries and encrypted vaults. Linking requires one unambiguous existing company
user and explicit administrative authorization; a different second Google identity
is refused. The recorded identity survives removal of the temporary authorization.
The consumed Clerk identity no longer permits browser sign-in. Log each successful
link using identity digests only.

This is a one-way state change for older binaries, which cannot interpret the
recorded link. Back up state before cutover, remove the migration allowlist after
cutover, and plan rollback as a state restoration rather than merely changing the
issuer or deploying an older binary.
