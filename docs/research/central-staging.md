# Central Accounts in Staging

Use configurable OpenID Connect (OIDC) sign-in, a dedicated application client,
and one persistent account owner. Each user owns a separate account catalog.
Regular Codex receives access tokens through its existing token helper.

This note records source discovery on 2026-09-29. It does not establish live SSO,
deployment, or account migration acceptance.

## Existing Company Sign-In

Staging Argo Workflows uses the issuer
`https://argocd.ue1.staging.plat.sm-svc.com/api/dex`. Its configuration references
the `argo-workflows-sso` Kubernetes Secret. Infra supports dedicated Dex clients
through `dex_static_clients` and injects client secrets from Kubernetes Secret
references. These are reusable integration points; codexctl needs its own client
ID, secret, and exact HTTPS callback. Sources inspected:

- `argocd-deploy/plat/ue1-staging/argocd/argo-workflows-application.yaml:98-114`
- `infra/stacks/orgs/sawmills/plat/staging/us-east-1/argocd.yaml:56-63`
- `infra/components/terraform/eks/argocd/main.tf:125-135,213-229`
- `infra/components/terraform/eks/argocd/variables-argocd.tf:219-230`

AWS SSO SAML supplies the existing company connector. The catalog pins Dex v2.31.2.
Dex's current official documentation warns that its SAML connector is
unmaintained and may allow authentication bypass. Keep the issuer configurable.
Deployment should resolve this upstream security concern or use a maintained
company OIDC provider. No secret values were read. Sources:
`infra/stacks/catalog/eks/argocd.yaml:19-20,39-50` and
[Dex SAML connector documentation](https://dexidp.io/docs/connectors/saml/).

Clerk is a separate customer-auth integration in
`auth-service/deploy/k8s/overlays/staging/configmap-patch.yaml` and
`auth-service/internal/auth/clerk_oauth.go`. It does not establish the company
identity provider for this application.

## OIDC and Machine Enrollment

Use `openidconnect` 4.0.1 with optional `reqwest` and `rustls-tls` features.
Its MIT license and Rust 1.65 minimum fit this Apache-2.0, Rust 1.89 project.
The crate uses reqwest 0.12 in its examples. It provides discovery, authorization
code with PKCE, and ID token verification. This avoids custom signature and
protocol validation. Sources: [crate manifest](https://github.com/ramosbugs/openidconnect-rs/blob/4.0.1/Cargo.toml)
and [official crate documentation](https://docs.rs/openidconnect/4.0.1/openidconnect/).

The server should use authorization code with PKCE S256, random state, and a
nonce. Validate signatures, issuer, audience, expiry, and nonce. Key a user by
issuer and subject; email is a display field. Require an explicit company
allowlist, such as verified issuer-specific group membership. Group enforcement
needs a provider-specific claim definition. Disable HTTP redirects and set
timeouts. Sources: [OIDC token validation](https://openid.net/specs/openid-connect-core-1_0.html#IDTokenValidation)
and [crate security guidance](https://docs.rs/openidconnect/4.0.1/openidconnect/#security-warning).

For `codexctl connect`, create an expiring enrollment request. Open the system
browser, authenticate with company SSO, and ask the user to confirm the device
name and displayed code. The CLI polls using a separate high-entropy secret.
Return its device credential only after confirmation, consume enrollment once,
and support revocation. Bound polling and failed-code attempts. This application
enrollment can wrap OIDC without assuming that Dex offers a device grant.
Sources: [native browser and PKCE guidance](https://www.rfc-editor.org/rfc/rfc8252)
and [device grant phishing and polling guidance](https://www.rfc-editor.org/rfc/rfc8628).

## Persistent Refresh Ownership

Start with one StatefulSet replica and a CSI `ReadWriteOncePod` PVC. Ordinary
`ReadWriteOnce` permits multiple pods on one node. Verify driver support and CSI
sidecar versions before applying manifests. Retain an exclusive file lock as
an application guard. Source: [Kubernetes access modes](https://kubernetes.io/docs/concepts/storage/persistent-volumes/#access-modes).

A StatefulSet normally preserves one pod per identity. Force deletion can start
a replacement while the old pod still runs. Do not force replacement until the
old node or process is fenced. A Kubernetes Lease alone cannot stop an isolated
process from refreshing at OpenAI. Keep replicas fixed until distributed
ownership has a real fencing mechanism. Source:
[StatefulSet force deletion](https://kubernetes.io/docs/tasks/run-application/force-delete-stateful-set-pod/).

Give shutdown enough time to finish queued refreshes, stop the Codex owner, and
persist its final auth file. Kubernetes starts the grace clock before `preStop`
and eventually sends SIGKILL. Graceful shutdown cannot guarantee recovery from
OOM or node loss. Source: [pod termination](https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/#pod-termination).

## Linux Child Processes and Recovery

The current RPC child uses `kill_on_drop`. For Linux parent crashes, add
`PR_SET_PDEATHSIG(SIGKILL)` in the child's `pre_exec` and recheck its parent PID
after setting it. This closes the race where the parent dies before the setting.
Use a long-lived spawning thread: Linux tracks thread death, including when
other parent threads remain alive. The setting survives ordinary exec, but
forked grandchildren do not inherit it. Source:
[Linux parent-death signal semantics](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html).

After taking the owner lock and proving the previous child is gone, inspect any
retained runtime auth. Verify its account identity and token ordering, then
persist the newest valid credentials before starting another owner. Preserve
unreadable or conflicting evidence and mark that account unavailable. Other
accounts should remain usable. This is an application recommendation derived
from the prototype's current refusal to start with `owner-runtime-*` directories
in `src/central/server.rs`; it is not a Linux guarantee. An upstream refresh that
rotates a token before writing it locally can still require a fresh login.

## RSA Dependency Exception

`openidconnect` 4.0.1 depends on `rsa` 0.9.10. [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) affects RSA private key operations through observable timing. No patched release exists as of 2026-09-30.

This server uses the library to verify issuer signatures. Its [RSA verification path](https://docs.rs/crate/openidconnect/4.0.1/source/src/core/crypto.rs) constructs `RsaPublicKey` from public JWKS fields and calls `verify`. The server does not construct an RSA private key, decrypt RSA ciphertext, or sign RSA tokens. The vault uses AES-GCM. The OIDC client uses its client secret for authentication.

The affected private key path is therefore unreachable in this integration. `.cargo/audit.toml` and `osv-scanner.toml` exclude this advisory alone. Other advisories still fail checks. Remove or reassess the exception before adding RSA signing or decryption.

## Monitoring Schema Boundary

The staging Prometheus selects ServiceMonitor and PrometheusRule resources labeled `release: kube-prometheus` across namespaces. Its current ServiceMonitor CRD endpoint schema contains only `interval`, `path`, `port`, `scheme`, and a limited `tlsConfig`. Server dry runs reject both modern `authorization` and legacy `bearerTokenSecret`. The authenticated ServiceMonitor is therefore separate from the staging overlay. Update the CRD under the monitoring owner's change process before registering that resource. The PrometheusRule and other staged resources pass server dry-run validation in an existing namespace. `promtool` 3.5.0 validates both alert expressions. No resources were applied, and no notifications were sent.

## Network Policy Boundary

A read-only inspection of the staging VPC CNI found `--enable-network-policy=false` on `aws-eks-nodeagent`. The supplied NetworkPolicy therefore does not enforce traffic restrictions in the current cluster. The internal ALB and API authentication remain separate controls. Confirm policy enforcement with the platform owner before relying on namespace restrictions. No cluster settings were changed.

## Native Rejection Evidence

The pinned Codex `account/read` path can fail after refresh because configuration or workspace routing fails.
An RPC error alone cannot prove that OpenAI rejected the refresh grant.
See the [pinned account reader](https://github.com/openai/codex/blob/rust-v0.159.0/codex-rs/app-server/src/request_processors/account_processor/workspace_routing.rs#L157-L180).

The [pinned auth-status implementation](https://github.com/openai/codex/blob/rust-v0.159.0/codex-rs/app-server/src/request_processors/account_processor.rs#L1041-L1116) keeps the ChatGPT method but suppresses its exported token on permanent refresh failure.
The broker first requires an exportable cached token from its private file-backed ChatGPT owner.
It infers rejection only when the same method later suppresses that token after the forced attempt.
It also requires unchanged credentials and a stopped owner before another input can replace the rejected grant.
Other auth methods, missing fields, unsupported calls, routing errors, and incomplete RPC work keep the reservation.
Reassess this inference when changing the pinned native owner version.
