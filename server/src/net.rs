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

/// A request error with its causes ("error sending request: dns error: failed to lookup
/// address…"), which say what actually went wrong. Never the URL: it may carry tokens.
pub fn describe(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut parts = vec![error.to_string()];
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    parts.join(": ")
}

/// A plain client for Telegram's IP list and alerts.
pub fn client() -> Result<reqwest::Client> {
    Ok(client_builder()?.timeout(Duration::from_secs(30)).build()?)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn errors_say_what_went_wrong_without_the_url() {
        let client = super::client().unwrap();
        // Nothing listens on port 1.
        let error = client.get("http://127.0.0.1:1/secret-token").send().await.unwrap_err();
        let text = super::describe(error);
        assert!(text.starts_with("error sending request: "), "{text}");
        assert!(text.to_lowercase().contains("connect"), "names the cause: {text}");
        assert!(!text.contains("secret-token"), "{text}");
    }
}
