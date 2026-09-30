use anyhow::{Result, bail};
use std::time::Duration;

/// Credentials never travel over cleartext except a local loopback connection.
pub fn origin(server: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(server)?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && local))
        || url.host_str().is_none()
        || url.path() != "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("server must be an HTTPS origin (loopback HTTP is allowed for local tests)");
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
