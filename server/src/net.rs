//! HTTP clients with bundled root certificates, so the image needs no CA files.

use std::time::Duration;

use anyhow::Result;

pub fn client_builder() -> Result<reqwest::ClientBuilder> {
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| reqwest::Certificate::from_der(der))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(reqwest::Client::builder().tls_certs_only(roots).connect_timeout(Duration::from_secs(10)))
}

/// A plain client for Telegram's IP list and alerts.
pub fn client() -> Result<reqwest::Client> {
    Ok(client_builder()?.timeout(Duration::from_secs(30)).build()?)
}
