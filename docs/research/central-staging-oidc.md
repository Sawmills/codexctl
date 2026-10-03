# Clerk OIDC for Central Staging

Historical Clerk investigation. The staging target is now direct Google Workspace
SSO; use the [Google configuration and cutover runbook](../central-server.md#one-time-clerk-identity-cutover).
The observations below describe the former provider, not the current setup.

Use a dedicated confidential Clerk OAuth application for the codexctl server.
Keep `public: false`, require PKCE, and register the exact HTTPS callback.
The current Rust flow fits the documented Clerk contract. Public discovery and
Google configuration match its needs. Browser sign-in still needs verification.

This note targets the staging operator. Sources were checked on 2026-10-01
against repository HEAD `eb9c6b72454e4b416e6a70c6afd9f94015acdde2`.
Research used primary sources and public metadata. Operator provisioning creates only
the dedicated OAuth client; it does not change users or shared sign-in settings.

## Application Configuration

The proposed Backend API request body is:

```json
{
  "name": "codexctl-staging",
  "public": false,
  "pkce_required": true,
  "consent_screen_enabled": true,
  "redirect_uris": [
    "https://codexctl.ue1.staging.plat.sm-svc.com/auth/callback"
  ],
  "scopes": "openid email profile"
}
```

Clerk's [2026-05-12 Backend API schema](https://github.com/clerk/openapi-specs/blob/main/bapi/2026-05-12.yml)
defines `POST https://api.clerk.com/v1/oauth_applications`. Only `name` is
required. All fields above are supported. `scopes` is a space-delimited
scope ceiling. Use `redirect_uris`; `callback_url` is deprecated.
The create response adds `client_secret` to the ordinary application object.
`GET /oauth_applications/{oauth_application_id}` and list responses omit that secret field. List contains
`data` and `total_count`.
These are REST field names. The JavaScript SDK uses `redirectUris`.
Source: [SDK create parameters](https://clerk.com/docs/reference/backend/oauth-applications/create).

Clerk supports `openid`, `email`, and `profile`. It permits PKCE for confidential
clients. Public mode removes the need for a client secret; it does not enforce
PKCE alone. Set the dedicated application's `pkce_required` field rather than
changing the shared instance policy. Keep consent enabled. Neither dynamic
registration nor a Clerk device grant is needed for this server's browser flow.
Source: [Clerk OAuth implementation](https://clerk.com/docs/guides/configure/auth-strategies/oauth/how-clerk-implements-oauth).

Use `openid email` as the smallest scope ceiling for the current implementation.
Allow `profile` only if the operator wants that conventional scope available.
The server currently adds `email`; `openidconnect` adds `openid` automatically.
It does not request `profile`, refresh access, or metadata.
Sources: [server authorization flow](../../src/central/enrollment.rs#L272)
and [openidconnect 4.0.1 client source](https://github.com/ramosbugs/openidconnect-rs/blob/4.0.1/src/client.rs).

## Discovery and Secret Handling

Set `issuer` to the selected Clerk instance's HTTPS Frontend API URL. Obtain the
application's `discovery_url` from its returned metadata and confirm the issuer
there. OIDC uses `/.well-known/openid-configuration`; the OAuth-only metadata
endpoint is a separate document. Clerk documents `client_secret_basic` and
`S256` support. The crate defaults to HTTP Basic client authentication when a
secret is present. Sources: [application metadata fields](https://clerk.com/docs/reference/backend/types/backend-oauth-application),
[Clerk OAuth metadata](https://clerk.com/docs/guides/configure/auth-strategies/oauth/how-clerk-implements-oauth),
and [crate client source](https://github.com/ramosbugs/openidconnect-rs/blob/4.0.1/src/client.rs).

Keep the new client secret in the existing private mounted file at
`/keys/oidc-client-secret`. Clerk shows its secret at creation and cannot show
it again. Never put the create response in logs or this research note.
See [Clerk SSO setup](https://clerk.com/docs/guides/configure/auth-strategies/oauth/single-sign-on).
The server configuration needs the instance issuer, returned client ID,
`client_secret_file`, and `allowed_domains: ["sawmills.ai"]`.
Source: [configuration loader](../../src/central/enrollment.rs#L40).

## Company Identity Restriction

Clerk documents `email` as the user's primary email and `email_verified` as its
verification state. These claims can appear in the ID token when the scopes
permit them. Clerk's token issuer is the Frontend API URL; its audience is the
application client ID. Source: [Clerk ID token claims](https://clerk.com/docs/guides/configure/auth-strategies/oauth/single-sign-on).

The current server verifies the ID token with the provider keys and login nonce.
It requires `email_verified == true`, then compares the email domain exactly
against `sawmills.ai`, ignoring case. An absent claim, an unverified email, or
`sawmills.ai.example.com` fails authorization. User identity comes from
issuer plus subject. See [callback authorization](../../src/central/enrollment.rs#L354).
The OpenID specification says verified email demonstrates email control and
does not make email a unique identity. Source:
[OIDC standard claims](https://openid.net/specs/openid-connect-core-1_0.html#StandardClaims).

This policy grants access to any verified `sawmills.ai` primary email in the
selected instance. It does not establish current employment or company
organization membership. If those are required, they need a separate explicit
authorization contract. Keep the domain gate in this application rather than
changing sign-in restrictions across a shared customer Clerk instance.

## Read-Only Preflight and Acceptance

Before creation, list existing applications and reuse a matching dedicated one.
The [SDK list API](https://clerk.com/docs/reference/backend/oauth-applications/list)
supports `nameQuery`, `limit` from 1 to 500, and `offset`. REST names are
`name_query`, `limit`, and `offset`. A name query matches a case-insensitive
substring or an exact client ID. Paginate through all results, then check the
client ID, issuer, public flag, PKCE flag, scopes, and callback array.
Set ordering explicitly because SDK and schema default descriptions differ.
The REST request is
`GET https://api.clerk.com/v1/oauth_applications?name_query=codexctl-staging&limit=100&offset=0&order_by=%2Bname`.

The callback comes from the staging host and the server's `/auth/callback`
path. Keep it exact, without a wildcard or trailing slash. Sources:
[staging ingress](../../deploy/k8s/overlays/staging/ingress.yaml)
and [server redirect](../../src/central/enrollment.rs#L277).
OIDC requires a registered redirect URI match. Source:
[OIDC authorization request](https://openid.net/specs/openid-connect-core-1_0.html#AuthRequest).

No documented incompatibility requires a code change. The SDK create page's
scope list omits `openid`, and its public flag text only mentions public PKCE.
Current OAuth guidance and the Backend API schema supply the fuller contract.
At the inspected HEAD, the staging SSO ConfigMap names Dex and needs the selected Clerk
issuer and dedicated client ID. Source: [staging SSO configuration](../../deploy/k8s/overlays/staging/sso.yaml).

Acceptance still requires the selected instance's OIDC discovery to pass the
server, then a browser authorization code exchange using the client secret and
S256 verifier. Confirm a verified company email succeeds and an unauthorized
email fails. Do not weaken the verified-email gate if Clerk omits a claim.
The dedicated confidential client is registered with required PKCE and the exact
callback. Browser sign-in remains unverified.

## Identity Instance Selection

Use `https://clerk.sawmills.ai`, the existing company production identity issuer.
The service remains in staging and has separate client credentials. Public
[discovery](https://clerk.sawmills.ai/.well-known/openid-configuration) advertises
`openid`, `email`, `email_verified`, `RS256`, `client_secret_basic`, and `S256`.
On 2026-10-01, the public [environment](https://clerk.sawmills.ai/v1/environment)
reported `auth_config.test_mode: false` and Google sign-in enabled and authenticatable.
These checks establish configuration, not a successful browser round trip.

The ordinary staging development instance was rejected. Its public
[environment](https://unbiased-jay-65.clerk.accounts.dev/v1/environment)
reports test mode enabled with email-code verification. Clerk accepts a fixed
code for test email addresses without sending email. A `+clerk_test` address
can keep a company domain while bypassing mailbox verification. That weakens
this server's verified-email policy. No test account or exploit was created.
See [Clerk test emails](https://clerk.com/docs/guides/development/testing/test-emails-and-phones).
Clerk recommends production instances for staging authentication. Shared identity
users are intentional for this company SSO client; no user data is changed.
See [Clerk environment guidance](https://clerk.com/docs/guides/development/managing-environments#staging-environments).

The issuer exposes Google configuration through the read-only Frontend API
`user_settings.social.oauth_google` fields. The Backend API instance object
reports its production environment type. See [Frontend API schema](https://github.com/clerk/openapi-specs/blob/main/fapi/2026-05-12.yml)
and [Backend API schema](https://github.com/clerk/openapi-specs/blob/main/bapi/2026-05-12.yml).
Never change shared instance settings to enroll this service.
