//! Verification of a shared renewal after device-login candidate publication.
use super::*;
use crate::central::storage::login::{LoginOperation, LoginPhase};

impl Broker {
    async fn check_login_authority(&self, headers: &HeaderMap, op: &LoginOperation) -> Result<()> {
        let central = self
            .central
            .as_ref()
            .context("shared renewal needs PostgreSQL")?;
        if central.login_heartbeat(op).await? || self.stopping.load(Ordering::Acquire) {
            bail!("login canceled");
        }
        self.authorize(headers)
            .await
            .map_err(|_| anyhow::anyhow!("machine revoked"))?;
        Ok(())
    }

    pub(in crate::central) async fn verify_shared_renewal(
        &self,
        headers: &HeaderMap,
        op: &mut LoginOperation,
    ) -> Result<()> {
        let central = self
            .central
            .as_ref()
            .context("shared renewal needs PostgreSQL")?;
        let device = self
            .authorize(headers)
            .await
            .map_err(|_| anyhow::anyhow!("machine revoked"))?;
        let owner_ref = self
            .owner(&device, &op.alias)
            .await
            .map_err(|_| anyhow::anyhow!("renewal account missing"))?;
        let (imports, mut owner, lease) = loop {
            self.check_login_authority(headers, op).await?;
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
            if let Some(lease) = central.login_account_lease(op).await? {
                break (imports, owner, lease);
            }
            // Settlement needs the owner lock to publish and release its lease.
            // Unrelated account work also needs the imports lock.
            drop(owner);
            drop(imports);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let result = async {
            reconcile_owner_from_central(self, &mut owner, &op.account_id)
                .await
                .map_err(|_| anyhow::anyhow!("cannot reconcile renewal account"))?;
            let candidate = op
                .payload
                .candidate
                .clone()
                .context("missing shared candidate")?;
            if let Err(error) = owner.validate_owned_auth(&candidate) {
                op.payload.error = Some("wrong_account".into());
                // Device login has exited and no refresh-capable child has started.
                // Keep both reservations, but permit a fresh explicit renewal to repair them.
                central.login_save(op, LoginPhase::Rejected).await?;
                return Err(error);
            }
            if let Some(rpc) = owner.rpc.as_mut() {
                rpc.settle_and_stop().await?;
            } else {
                previous_owner_exited(&owner.home)?;
            }
            owner.rpc = None;
            let before = owner.vault.clone();
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
                    .clear_for_launch(&owner, relogin::AdmissionKind::Renewal, &imports)?;
                launch_owner(&mut owner, &self.binary, proof).await?;
                let revision = owner.snapshot()?.revision;
                owner
                    .tokens(TokenRequest {
                        previous_revision: Some(revision),
                        billing: true,
                        ..Default::default()
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("renewal verification failed"))?;
                if !owner.rpc.as_ref().is_some_and(Rpc::verified_login) {
                    bail!("renewal login not verified");
                }
                owner.vault.verified = true;
                let record = settled_owner_record(&mut owner, &before).await?;
                Ok::<_, anyhow::Error>(record)
            };
            let monitor = async {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    if self.stopping.load(Ordering::Acquire) {
                        bail!("server stopping");
                    }
                    central.login_heartbeat(op).await?;
                    if !central.renew(&lease, IMPORT_LEASE_TTL).await? {
                        bail!("renewal refresh lease lost");
                    }
                    self.authorize(headers)
                        .await
                        .map_err(|_| anyhow::anyhow!("machine revoked"))?;
                }
            };
            let record = tokio::select! {
                result=verification => result,
                result=monitor => result,
            }?;
            self.authorize(headers)
                .await
                .map_err(|_| anyhow::anyhow!("machine revoked"))?;
            central
                .login_complete(op, &lease, &record, before.revision)
                .await?;
            owner.verification_input = None;
            Ok(())
        }
        .await;
        if result.is_err() && op.phase == LoginPhase::Verifying {
            // A partial verification remains fenced in the shared journal. PR3 owns recovery.
            terminate_lost_import(&mut owner).await;
            op.payload.error = Some("relogin_verification_unresolved".into());
            let _ = central.login_save(op, LoginPhase::Unresolved).await;
        }
        central.release_lease(&lease).await?;
        result
    }
}
