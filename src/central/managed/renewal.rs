//! Verification of a shared renewal after device-login candidate publication.
use super::*;
use crate::central::storage::{
    identity::CandidateIdentityMismatch,
    login::{LoginAuthority, LoginKind, LoginOperation, LoginPhase},
};

impl Broker {
    async fn check_login_authority(&self, op: &LoginOperation) -> Result<()> {
        let central = self
            .central
            .as_ref()
            .context("shared renewal needs PostgreSQL")?;
        if central.login_heartbeat(op).await? || self.stopping.load(Ordering::Acquire) {
            bail!("login canceled");
        }
        if !central.login_authority_live(op).await? {
            bail!("machine revoked");
        }
        Ok(())
    }

    pub(in crate::central) async fn verify_shared_renewal(
        &self,
        op: &mut LoginOperation,
    ) -> Result<()> {
        let central = self
            .central
            .as_ref()
            .context("shared renewal needs PostgreSQL")?;
        self.check_login_authority(op).await?;
        // Reject a wrong login before waiting on reservations that it may itself
        // have created for another renewal's selected account.
        let shared = central
            .load_account(op.account()?)
            .await?
            .context("renewal account missing")?;
        let _: Vault = serde_json::from_value(shared.vault)?;
        let identity = central.validate_login_candidate(op).await;
        if let Err(error) = identity {
            if error.is::<CandidateIdentityMismatch>() {
                op.payload.error = Some("wrong_account".into());
                central.login_save(op, LoginPhase::Rejected).await?;
            }
            return Err(error);
        }
        if op.kind == LoginKind::Renewal && !central.login_reserve_renewal(op).await? {
            op.payload.error = Some("relogin_reserved".into());
            central.login_save(op, LoginPhase::Rejected).await?;
            return Ok(());
        }
        let owner_ref = self
            .owner_for_company_user(&op.user, op.payload.landed.as_deref().unwrap_or(&op.alias))
            .await
            .map_err(|_| anyhow::anyhow!("renewal account missing"))?;
        let (imports, mut held_owner, lease) = loop {
            self.check_login_authority(op).await?;
            let Ok(imports) =
                tokio::time::timeout(std::time::Duration::from_secs(1), self.imports.lock()).await
            else {
                continue;
            };
            let Ok(owner) =
                tokio::time::timeout(std::time::Duration::from_secs(1), owner_ref.lock()).await
            else {
                continue;
            };
            if let Some(lease) = central.login_account_lease(op, &self.holder_id).await? {
                break (imports, owner, lease);
            }
            // Settlement needs the owner lock to publish and release its lease.
            // Unrelated account work also needs the imports lock.
            drop(owner);
            drop(imports);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        // The renewed lease transfers settlement responsibility to this worker.
        // Older callbacks must not mutate or stop the replacement owner.
        held_owner.recovery_generation = held_owner.recovery_generation.wrapping_add(1);
        let mut owner = Some(held_owner);
        let authority = op.clone();
        let monitor = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if self.stopping.load(Ordering::Acquire) {
                    bail!("server stopping");
                }
                if central.login_heartbeat(&authority).await? {
                    bail!("login canceled");
                }
                if !central.renew(&lease, IMPORT_LEASE_TTL).await? {
                    bail!("renewal refresh lease lost");
                }
                if !central.login_authority_live(&authority).await? {
                    bail!("machine revoked");
                }
            }
        };
        let work = async {
            // Settlement holds only this account's mutex. Retain that guard in
            // the outer scope so cancellation can terminate the child directly.
            drop(imports);
            {
                let owner = &mut **owner.as_mut().context("renewal owner lock missing")?;
                reconcile_owner_from_central(self, owner, op.account()?)
                    .await
                    .map_err(|_| anyhow::anyhow!("cannot reconcile renewal account"))?;
                if let Some(rpc) = owner.rpc.as_mut() {
                    rpc.settle_and_stop().await?;
                } else {
                    previous_owner_exited(&owner.home)?;
                }
                owner.rpc = None;
            }
            // Release the stopped owner before reacquiring imports, preserving
            // the common imports-before-owner order without blocking other seats.
            drop(owner.take());
            let imports = self.imports.lock().await;
            owner = Some(owner_ref.lock().await);
            let owner = &mut **owner.as_mut().context("renewal owner lock missing")?;
            reconcile_owner_from_central(self, owner, op.account()?)
                .await
                .map_err(|_| anyhow::anyhow!("cannot reconcile renewal account"))?;
            let candidate = op
                .payload
                .candidate
                .clone()
                .context("missing shared candidate")?;
            let identity = central.validate_login_candidate(op).await;
            if let Err(error) = identity {
                if error.is::<CandidateIdentityMismatch>() {
                    op.payload.error = Some("wrong_account".into());
                }
                return Err(error);
            }
            let before = owner.vault.clone();
            // A failed refresh publication can leave local evidence ahead of
            // PostgreSQL. Keep that evidence for settlement, but compare the
            // final commit against the shared revision read under this lease.
            let committed_revision = central
                .load_account(op.account()?)
                .await?
                .context("renewal account missing")?
                .revision;
            let identities = central.retained_identities().await?;
            let agreement = (|| -> Result<()> {
                let identity = identities
                    .get(op.account()?)
                    .context("shared verification identity missing")?;
                identity.validate(&owner.vault.auth)?;
                identity.validate(&retained_auth(&owner.home)?)
            })();
            if let Err(error) = agreement {
                owner.available = false;
                owner.routing_refused = true;
                owner.shared_revision = None;
                return Err(error);
            }
            // This is durable before initialize, which can itself rotate credentials.
            central.login_begin_verification(op, &lease).await?;
            owner.vault.auth = candidate.clone();
            owner.vault.revision = before
                .revision
                .checked_add(1)
                .context("credential revision exhausted")?;
            owner.vault.verified = false;
            owner.vault.import_rejected = false;
            owner.verification_input = Some(candidate.clone());
            owner.available = true;
            owner.routing_refused = false;
            owner.refresh_enabled = true;
            vault::save(&owner.state, &owner.key, &owner.vault)?;
            store::atomic_write(
                &owner.home.join("auth.json"),
                &serde_json::to_vec(&candidate)?,
            )?;
            let verification = async {
                let proof = relogin::identity_inventory(&owner.state, &self.key, &owner.home)
                    .clear_for_shared_launch(
                        owner,
                        relogin::AdmissionKind::Renewal,
                        &imports,
                        &identities,
                    )?;
                spawn_owner(owner, &self.binary, proof)?;
                // Account authority and durable spawn evidence now cover initialization.
                drop(imports);
                initialize_owner(owner).await?;
                let revision = owner.snapshot()?.revision;
                let browser = matches!(op.authority, LoginAuthority::Browser(_));
                // Browser verification uses the candidate's checked identity claims
                // plus a cached native read matching the expected routing account.
                // This is its intended proof: verified_login requires forced refresh,
                // so Browser never uses it or requests rotation or billing reads.
                let request = if browser {
                    TokenRequest::default()
                } else {
                    TokenRequest {
                        previous_revision: Some(revision),
                        billing: true,
                        ..Default::default()
                    }
                };
                let observed = owner
                    .tokens(request)
                    .await
                    .map_err(|_| anyhow::anyhow!("renewal verification failed"))?;
                let verified = if browser {
                    observed.native_routing_supported && owner.routing_observed
                } else {
                    owner.rpc.as_ref().is_some_and(Rpc::verified_login)
                };
                if !verified {
                    bail!("renewal login not verified");
                }
                owner.vault.verified = true;
                let record = settled_owner_record(owner, &before).await?;
                Ok::<_, anyhow::Error>(record)
            };
            let record = verification.await?;
            self.check_login_authority(op).await?;
            central
                .login_complete(op, &lease, &record, committed_revision)
                .await?;
            owner.shared_revision = Some(record.revision);
            owner.verification_input = None;
            Ok(())
        };
        let result = tokio::select! {
            result = work => result,
            result = monitor => result,
        };
        let mut release_proven = result.is_ok();
        if result.is_err() {
            if owner.is_none() {
                owner = Some(owner_ref.lock().await);
            }
            let owner = &mut **owner.as_mut().context("renewal owner lock missing")?;
            release_proven = previous_owner_exited(&owner.home).is_ok();
            if op.phase == LoginPhase::Verifying || owner.rpc.is_some() || !release_proven {
                let rejected = op.phase == LoginPhase::Verifying
                    && !owner.vault.verified
                    && owner.vault.import_rejected;
                terminate_lost_import(owner).await;
                release_proven = previous_owner_exited(&owner.home).is_ok();
                // Only a completed rejection with an unchanged final journal is
                // repairable. An interrupted provider call remains unresolved.
                let rejected = rejected
                    && release_proven
                    && owner.verification_input.as_ref().is_some_and(|input| {
                        retained_auth(&owner.home).is_ok_and(|auth| &auth == input)
                    });
                op.payload.error = Some(
                    if rejected {
                        "relogin_verification_rejected"
                    } else if op.phase == LoginPhase::Verifying {
                        "relogin_verification_unresolved"
                    } else {
                        "relogin_settlement_unresolved"
                    }
                    .into(),
                );
                let phase = if rejected {
                    LoginPhase::Rejected
                } else {
                    LoginPhase::Unresolved
                };
                let _ = central.login_save(op, phase).await;
            }
        }
        if release_proven {
            central.release_lease(&lease).await?;
        }
        result
    }
}
