# Provider implementation choices

The implementation follows claudectl's 2026-10-03 account-server plan. The research
PR remains documentation-only; these changes are in a separate implementation PR.

Two interfaces were considered before implementing the provider module:

- Extend the existing OpenAI `Owner` and `Vault` with a provider enum. Callers would
  coordinate OAuth expiry, Codex RPC startup, identity parsing, and renewal journals
  through the same struct. Cancellation and persistence would branch throughout
  the established OpenAI recovery paths, exposing provider-specific failure states
  to every caller.
- Keep the established OpenAI protocol and refresh owner, and add a Claude module
  with account admission, access acquisition, and cached usage as its interface.
  The module owns provider HTTP calls, identity validation, expiry, serialization,
  encrypted persistence, and uncertain-refresh recovery. Shared enrollment and
  authorization select the provider before crossing this interface. Request
  cancellation cannot abandon an in-flight refresh: the request task holds a
  server shutdown permit until persistence completes.

The second interface keeps the OpenAI ownership rules intact and concentrates
Claude's opaque-token and expiry rules. Both providers reuse SSO, machine registry,
vault encryption and atomic storage, process ownership, and HTTP lifecycle.
Provider-specific records occupy separate namespaces. Missing historical machine
scopes mean OpenAI only; explicit enrollment or administrator approval grants Claude.

Tests exercise the standalone CLI/HTTP protocol and the Claude module's admission,
acquisition, and usage interface using a synthetic provider. Client tests exercise
the public launcher interface with real child processes. These are the interfaces
and acceptance cases in the approved implementation plan.
