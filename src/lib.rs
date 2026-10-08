pub mod api;
#[cfg(feature = "central-prototype")]
pub mod central;
pub mod config;
pub mod daemon;
pub mod profile;
#[cfg(feature = "central-prototype")]
pub mod relay;
pub mod status_format;
pub mod status_json;
pub mod status_pace;
pub mod statusline;
pub mod store;

/// Models allowed in central relay capacity metric labels. Unknown models use `other`.
#[cfg(feature = "central-prototype")]
pub(crate) const RELAY_KNOWN_MODELS: &[&str] = &[
    "gpt-5",
    "gpt-5.1",
    "gpt-5.2",
    "gpt-5.3",
    "gpt-5.4",
    "gpt-5.5",
    "gpt-6",
    "gpt-6.1",
    "gpt-6.1-sol",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-sol",
];
