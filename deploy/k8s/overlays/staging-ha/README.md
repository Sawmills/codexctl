# Codexctl staging HA overlay

This overlay is a build-only phase 3 artifact. It is not referenced by
`deploy/k8s/staging-application.yaml` and must not be applied until HQ confirms
that infra#1513 has created the shared `codexctl` database and that
`codexctl-postgres` is Ready.

It runs three stateless broker pods in PostgreSQL mode. Pods spread across zones
and hostnames, require hostname anti-affinity, use only `emptyDir` volumes, and
keep `PodDisruptionBudget.minAvailable: 1`. The overlay has no
`karpenter.sh/do-not-disrupt` annotation. The `codexctl-secrets`
ExternalSecret supplies the vault key, SSO secret, and metrics token; the
The infra#1513 manifest owns the `codexctl-postgres` ExternalSecret and its five
SSM references. Egress allows DNS, HTTPS, and private VPC PostgreSQL. The container
uses the image system CA bundle through
`CODEXCTL_CENTRAL_DB_CA_FILE`. Kustomize rewrites the workload image to the
reviewed immutable ECR manifest digest; it does not use the mutable staging tag.

Build it with:

```sh
kubectl kustomize deploy/k8s/overlays/staging-ha
```

Do not apply this overlay as part of normal staging reconciliation.
