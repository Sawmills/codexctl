# ADR 0002: PostgreSQL shared state for account-server HA

- **Status:** accepted
- **Date:** 2026-10-03
- **Decision:** Move durable account-server state to encrypted PostgreSQL rows and use a fenced per-account lease for refresh ownership.

## Context

The account server currently stores encrypted vaults, runtime journals, device
and user registries, reset journals, and login-renewal records on one
`ReadWriteOncePod` volume. File locks make one broker safe, but they cannot
coordinate independent pods. OAuth refresh-token rotation invalidates a losing
refresh, so a pod-level mutex is insufficient. Enrollment challenges and usage
caches are process-local, which also makes a multi-pod Service inconsistent.

Sawmills already operates encrypted private PostgreSQL/RDS for durable service
state and applies schema migrations with a Kubernetes migration job
(`/home/amir/Code/agent-platform/infra/README.md:27-43`, `62-69`). Another
Sawmills design uses PostgreSQL leader election so only one replica runs shared
scheduled work, with takeover after leader loss
(`/home/amir/Code/condition-curator-saw12143-proof/docs/plans/2026-08-20-saw-10050-batch-monitor-reads.md:251-257`).

## Decision

PostgreSQL becomes the source of truth for users, devices, account identity and
metadata, encrypted credentials, credential revisions, reset operations,
relogin operations, and expiring enrollment state. The existing vault key stays
outside the database and encrypts credential payloads before insertion.

Each account has a lease row containing holder identity, fencing epoch, and
expiry. A refresh write must match the holder and epoch in the same transaction
that stores the rotated credentials and new revision. Any pod may read a
committed access token; only the lease holder may refresh or mutate credentials.
Followers wait for the revision written by the holder instead of refreshing.

Usage observations, failure counters, and native process homes remain pod-local
derived state. A pod-local Codex child is never started for an account unless
that pod owns its lease. Enrollment and reset operations retain durable,
transactional idempotency records.

## Alternatives rejected

1. **EFS/RWX with file locks:** preserves the file layout but does not provide a
   database fencing epoch, leaves enrollment state in memory, and introduces
   NFS lock/rename failure modes for credential journals.
2. **Leader-only writes with follower proxying:** reduces initial code changes,
   but a leader and its RWO volume remain a single failure domain. It still
   needs replicated durable state before a follower can safely take over.
3. **One global Kubernetes Lease for all refreshes:** can prevent races, but
   makes one pod a throughput bottleneck and does not by itself make file
   journals or enrollment durable. Per-account database leases preserve
   parallelism while fencing exactly the conflicting operation.

## Consequences

This is a substantial migration: schema design, encrypted row codecs, a
dual-read/dual-write compatibility release, connection pooling, and real
PostgreSQL crash/lease tests are required. Database availability becomes a
runtime dependency, so short-lived committed access tokens and explicit
readiness/alerts are part of the implementation. In return, replicas can run
on independent nodes and zones, refresh ownership survives pod failure without
Multi-Attach, and every write that can spend or rotate credentials has a
transactional fence.
