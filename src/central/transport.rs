use anyhow::{Result, bail};
use std::time::Duration;

/// TLS is required unless the operator explicitly enables isolated loopback tests.
pub fn origin(server: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(server)?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"));
    let test_loopback = std::env::var("CODEXCTL_ALLOW_INSECURE_LOOPBACK").as_deref() == Ok("1");
    if !(url.scheme() == "https" || (url.scheme() == "http" && local && test_loopback))
        || url.host_str().is_none()
        || url.path() != "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "server must be an HTTPS origin; isolated local tests can explicitly set CODEXCTL_ALLOW_INSECURE_LOOPBACK=1"
        );
    }
    Ok(url)
}

pub fn blocking() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(195))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?)
}
