//! Provider enablement and explicit machine access. Missing legacy scopes mean OpenAI only.
use super::{
    managed::{Broker, HttpError},
    vault,
};
use anyhow::{Context, Result, bail};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Openai,
    Anthropic,
}

pub fn legacy() -> Vec<Provider> {
    vec![Provider::Openai]
}
pub fn is_legacy(providers: &[Provider]) -> bool {
    providers == [Provider::Openai]
}

pub fn grant(state: &Path, machine: &str, providers: Vec<Provider>) -> Result<()> {
    if providers.is_empty() {
        bail!("at least one provider is required");
    }
    let _lock = vault::registry_lock(state, "devices.lock")?;
    let mut machines = vault::devices(state)?;
    let selected = machines
        .iter_mut()
        .find(|d| d.id == machine && !d.revoked)
        .context("machine not found or revoked")?;
    selected.providers = providers;
    vault::save_devices(state, &machines)
}

impl Broker {
    pub(super) fn authorize_provider(
        &self,
        headers: &HeaderMap,
        provider: Provider,
    ) -> Result<vault::Device, HttpError> {
        let machine = self.authorize(headers)?;
        if !self.providers.contains(&provider) || !machine.providers.contains(&provider) {
            return Err(self.error(StatusCode::FORBIDDEN, "provider_not_enabled"));
        }
        Ok(machine)
    }
}
