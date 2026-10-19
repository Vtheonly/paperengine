//! Shared HTTP client for source adapters.
//!
//! One client per adapter applies the source's rate policy, identifies the engine honestly in
//! the `User-Agent`, and classifies every response into the typed [`SourceError`] taxonomy.
//! 403/429 and authentication demands are surfaced as stop/back-off conditions — the engine
//! never retries past a refusal and never attempts to evade it.

use crate::sources::contract::{RequestPacer, SourceError};
use std::time::Duration;

pub const ENGINE_USER_AGENT: &str = concat!(
    "paperengine/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/Vtheonly/paperengine)"
);

#[derive(Debug, Clone)]
pub struct SourceHttpClientOptions {
    /// Contact e-mail appended to the User-Agent for polite pools (OpenAlex, Crossref).
    pub contact_email: Option<String>,
    /// Request timeout applied to every API call.
    pub request_timeout: Duration,
    /// Minimum spacing between requests, from the source's rate policy.
    pub min_request_interval: Duration,
}

impl Default for SourceHttpClientOptions {
    fn default() -> Self {
        Self {
            contact_email: None,
            request_timeout: Duration::from_secs(30),
            min_request_interval: Duration::from_secs(1),
        }
    }
}

#[derive(Debug)]
pub struct SourceHttpClient {
    client: reqwest::Client,
    user_agent: String,
    pacer: RequestPacer,
}

impl SourceHttpClient {
    pub fn new(options: SourceHttpClientOptions) -> Result<Self, SourceError> {
        let user_agent = match options.contact_email.as_deref() {
            Some(email) if email.contains('@') => format!("{ENGINE_USER_AGENT}; mailto:{email}"),
            _ => ENGINE_USER_AGENT.to_owned(),
        };
        let client = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            .timeout(options.request_timeout)
            .build()
            .map_err(|error| SourceError::Network {
                detail: error.to_string(),
            })?;
        Ok(Self {
            client,
            user_agent,
            pacer: RequestPacer::new(options.min_request_interval),
        })
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    async fn send(&self, url: &str) -> Result<reqwest::Response, SourceError> {
        self.pacer.wait_turn().await;
        self.client
            .get(url)
            .send()
            .await
            .map_err(|error| SourceError::Network {
                detail: error.to_string(),
            })
    }

    /// GET a URL and parse the body as JSON, classifying non-200 statuses into the taxonomy.
    pub async fn get_json(&self, url: &str) -> Result<serde_json::Value, SourceError> {
        let response = self.send(url).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status, &response));
        }
        let text = response
            .text()
            .await
            .map_err(|error| SourceError::Network {
                detail: error.to_string(),
            })?;
        serde_json::from_str(&text).map_err(|error| SourceError::MalformedResponse {
            detail: format!("invalid JSON body: {error}"),
        })
    }

    /// GET a URL and return the body as text, classifying non-200 statuses into the taxonomy.
    pub async fn get_text(&self, url: &str) -> Result<String, SourceError> {
        let response = self.send(url).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status, &response));
        }
        response.text().await.map_err(|error| SourceError::Network {
            detail: error.to_string(),
        })
    }
}

/// Map a non-success HTTP status to the typed error taxonomy, honoring `Retry-After`.
fn classify_status(status: reqwest::StatusCode, response: &reqwest::Response) -> SourceError {
    let code = status.as_u16();
    match code {
        401 => SourceError::Unauthorized { status: code },
        403 => SourceError::Forbidden { status: code },
        404 => SourceError::NotFound { status: code },
        429 => {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok());
            SourceError::RateLimited {
                status: code,
                retry_after_seconds: retry_after,
            }
        }
        400..=499 => SourceError::UnexpectedStatus { status: code },
        500..=599 => SourceError::ServerError { status: code },
        _ => SourceError::UnexpectedStatus { status: code },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_identifies_the_engine_and_contact() {
        let plain = SourceHttpClient::new(SourceHttpClientOptions {
            min_request_interval: Duration::from_millis(0),
            ..SourceHttpClientOptions::default()
        })
        .unwrap();
        assert!(plain.user_agent().starts_with("paperengine/"));
        assert!(!plain.user_agent().contains("mailto"));

        let polite = SourceHttpClient::new(SourceHttpClientOptions {
            contact_email: Some("researcher@example.org".into()),
            min_request_interval: Duration::from_millis(0),
            ..SourceHttpClientOptions::default()
        })
        .unwrap();
        assert!(polite
            .user_agent()
            .contains("mailto:researcher@example.org"));
    }

    #[test]
    fn invalid_email_is_ignored_in_user_agent() {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            contact_email: Some("not-an-email".into()),
            min_request_interval: Duration::from_millis(0),
            ..SourceHttpClientOptions::default()
        })
        .unwrap();
        assert!(!client.user_agent().contains("mailto"));
    }
}
