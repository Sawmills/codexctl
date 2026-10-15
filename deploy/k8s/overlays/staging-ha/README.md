# Codexctl staging HA overlay

This overlay is a render-only phase 3 artifact and must never be applied manually.
It is not referenced by `deploy/k8s/staging-application.yaml`. Deploy staging HA
through Argo CD's tracked `deploy/k8s/overlays/staging` overlay and the
[B33 runbook](../../../../docs/superpowers/plans/b33-attempt-3-cutover.md), after
HQ confirms the fresh `codexctl_a5` provisioning receipt (infra#1604), the
separately reviewed ExternalSecret repoint (infra#1605), and `codexctl-postgres`
Ready on that target. Preserve the old database and its history.

It runs three stateless broker pods in PostgreSQL mode. Pods spread across zones
and hostnames, require hostname anti-affinity, use only `emptyDir` volumes, and
keep `PodDisruptionBudget.minAvailable: 1`. The overlay has no
`karpenter.sh/do-not-disrupt` annotation. The `codexctl-secrets`
ExternalSecret supplies the vault key, SSO secret, and metrics token. The infra
repository owns `codexctl-postgres`; infra#1605 repoints its five SSM references
to `/rds/quota-manager/codexctl-a5/`. Egress allows DNS, HTTPS, and private VPC PostgreSQL. The container
mounts the [AWS RDS global CA bundle](../../rds-ca/README.md) read-only at
`/etc/codexctl/rds-ca/global-bundle.pem` through
`CODEXCTL_CENTRAL_DB_CA_FILE`. Certificate and hostname verification stay enabled.
Kustomize rewrites the workload image to the
reviewed immutable ECR manifest digest; it does not use the mutable staging tag.

Build it with:

```sh
kubectl kustomize deploy/k8s/overlays/staging-ha
```

Use this overlay only for render validation; deployment uses the tracked staging overlay.
