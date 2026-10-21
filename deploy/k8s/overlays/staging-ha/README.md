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
use `PodDisruptionBudget.maxUnavailable: 1` and integer RollingUpdate
`maxSurge: 0` / `maxUnavailable: 1`. The Pod template carries
`karpenter.sh/do-not-disrupt: "true"` to block voluntary consolidation. The
five-second preStop only delays; kubelet sends TERM after the hook returns,
leaving approximately 55 seconds of the 60-second grace for server settlement.
Staging's NodePool expires after 336 hours with no NodePool termination grace;
the annotation can block expiry drain indefinitely. Coordinate maintenance with
HQ and the infra owner; see [Kubernetes shape](../../../../docs/central-ha.md#4-kubernetes-shape).
The `codexctl-secrets`
ExternalSecret supplies the vault key and SSO secret. `/metrics` needs no bearer;
[PR #259](https://github.com/Sawmills/codexctl/pull/259) retires its manifest wiring.
Rollout, scrape readback and existing SSM parameter deletion remain separate operator gates. The infra
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

Run the lifecycle/availability regression locally with
`python3 tests/ha_lifecycle_test.py -v` from the repository root. It needs PyYAML,
`kubectl` for local rendering, Docker, and cached
`node@sha256:b26b04c123d9ff8ab646ceb18b9d75a1173acf64b9a401094b906d27b29338d4`
(the runtime base image). The test uses synthetic PID 1 processes in read-only,
network-isolated containers; it never reads a live account or contacts Kubernetes.
This is a manual regression check: Cargo and the hosted CI workflow do not run
Python tests. CI wiring is a held workflow-owner follow-up on SAW-12988; lane
commits cannot edit `.github/workflows` under A124.
