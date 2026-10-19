//! Validated configuration for the CLI (#4 partial).
//!
//! `paperengine.toml` is the single configuration file. Unknown keys are rejected so a
//! typo cannot silently disable a budget, and every numeric bound is validated before the
//! engine touches the network. CLI flags override file values; the contact e-mail can also
//! come from `PAPERENGINE_CONTACT_EMAIL`.

use crate::library::AcquisitionConfig;
use crate::sources::BuiltinSourcesConfig;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

pub const CONFIG_SCHEMA_VERSION: u16 = 1;
/// Default configuration file name looked up in the working directory.
pub const DEFAULT_CONFIG_FILE: &str = "paperengine.toml";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file read failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("configuration file parse failed: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperEngineConfig {
    pub schema_version: u16,
    #[serde(default)]
    pub contact_email: Option<String>,
    #[serde(default)]
    pub openalex_api_key: Option<String>,
    #[serde(default)]
    pub discovery: DiscoverySettings,
    #[serde(default)]
    pub acquisition: AcquisitionSettings,
}

impl Default for PaperEngineConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            contact_email: None,
            openalex_api_key: None,
            discovery: DiscoverySettings::default(),
            acquisition: AcquisitionSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiscoverySettings {
    pub default_page_size: u16,
    pub max_pages: u32,
    pub max_records: usize,
    pub request_timeout_seconds: u64,
}

impl Default for DiscoverySettings {
    fn default() -> Self {
        Self {
            default_page_size: 25,
            max_pages: 4,
            max_records: 25,
            request_timeout_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AcquisitionSettings {
    pub max_downloads: usize,
    pub max_bytes_per_file: u64,
    pub max_total_bytes: u64,
    pub request_timeout_seconds: u64,
    pub total_timeout_seconds: u64,
    pub inter_download_delay_seconds: u64,
    pub allow_http: bool,
}

impl Default for AcquisitionSettings {
    fn default() -> Self {
        Self {
            max_downloads: 5,
            max_bytes_per_file: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            request_timeout_seconds: 60,
            total_timeout_seconds: 600,
            inter_download_delay_seconds: 1,
            allow_http: false,
        }
    }
}

impl PaperEngineConfig {
    /// Load a config file, or return defaults when the path does not exist.
    pub fn load_optional(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        let config: PaperEngineConfig = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::Invalid(format!(
                "schema_version must be {CONFIG_SCHEMA_VERSION}, found {}",
                self.schema_version
            )));
        }
        if let Some(email) = self.contact_email.as_deref() {
            if !email.contains('@') || email.len() < 5 {
                return Err(ConfigError::Invalid(format!(
                    "contact_email `{email}` is not a plausible e-mail address"
                )));
            }
        }
        let d = &self.discovery;
        if d.default_page_size == 0 || d.default_page_size > 200 {
            return Err(ConfigError::Invalid(
                "discovery.default_page_size must be between 1 and 200".into(),
            ));
        }
        if d.max_pages == 0 || d.max_pages > 100 {
            return Err(ConfigError::Invalid(
                "discovery.max_pages must be between 1 and 100".into(),
            ));
        }
        if d.max_records == 0 || d.max_records > 10_000 {
            return Err(ConfigError::Invalid(
                "discovery.max_records must be between 1 and 10000".into(),
            ));
        }
        if d.request_timeout_seconds == 0 || d.request_timeout_seconds > 600 {
            return Err(ConfigError::Invalid(
                "discovery.request_timeout_seconds must be between 1 and 600".into(),
            ));
        }
        let a = &self.acquisition;
        if a.max_downloads == 0 || a.max_downloads > 1_000 {
            return Err(ConfigError::Invalid(
                "acquisition.max_downloads must be between 1 and 1000".into(),
            ));
        }
        if a.max_bytes_per_file < 1024 || a.max_bytes_per_file > 512 * 1024 * 1024 {
            return Err(ConfigError::Invalid(
                "acquisition.max_bytes_per_file must be between 1 KiB and 512 MiB".into(),
            ));
        }
        if a.max_total_bytes < a.max_bytes_per_file {
            return Err(ConfigError::Invalid(
                "acquisition.max_total_bytes must be at least max_bytes_per_file".into(),
            ));
        }
        if a.request_timeout_seconds == 0 || a.total_timeout_seconds < a.request_timeout_seconds {
            return Err(ConfigError::Invalid(
                "acquisition timeouts must be positive and total >= per-request".into(),
            ));
        }
        Ok(())
    }

    /// Effective contact e-mail: config value or environment override.
    pub fn effective_contact_email(&self) -> Option<String> {
        std::env::var("PAPERENGINE_CONTACT_EMAIL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| self.contact_email.clone())
    }

    pub fn builtin_sources_config(&self) -> BuiltinSourcesConfig {
        BuiltinSourcesConfig {
            contact_email: self.effective_contact_email(),
            openalex_api_key: self.openalex_api_key.clone(),
            min_request_interval: None,
            request_timeout: Some(Duration::from_secs(self.discovery.request_timeout_seconds)),
        }
    }

    pub fn acquisition_config(&self) -> AcquisitionConfig {
        AcquisitionConfig {
            max_downloads: self.acquisition.max_downloads,
            min_bytes_per_file: 1024,
            max_bytes_per_file: self.acquisition.max_bytes_per_file,
            max_total_bytes: self.acquisition.max_total_bytes,
            request_timeout: Duration::from_secs(self.acquisition.request_timeout_seconds),
            total_timeout: Duration::from_secs(self.acquisition.total_timeout_seconds),
            inter_download_delay: Duration::from_secs(
                self.acquisition.inter_download_delay_seconds,
            ),
            allow_http: self.acquisition.allow_http,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        assert!(PaperEngineConfig::default().validate().is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = toml::from_str::<PaperEngineConfig>(
            "schema_version = 1\ncontact_email = \"a@b.org\"\nbogus_key = true\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("bogus_key"));
    }

    #[test]
    fn wrong_schema_version_is_rejected() {
        let config: PaperEngineConfig = toml::from_str("schema_version = 2\n").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn unsafe_budgets_are_rejected_with_actionable_errors() {
        let config: PaperEngineConfig =
            toml::from_str("schema_version = 1\n[acquisition]\nmax_bytes_per_file = 10\n").unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("max_bytes_per_file"));

        let config: PaperEngineConfig =
            toml::from_str("schema_version = 1\n[discovery]\nmax_pages = 0\n").unwrap();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_pages"));
    }

    #[test]
    fn valid_file_parses_and_maps_to_runtime_configs() {
        let config: PaperEngineConfig = toml::from_str(
            r#"
            schema_version = 1
            contact_email = "researcher@example.org"
            [discovery]
            max_records = 50
            [acquisition]
            max_downloads = 3
            max_bytes_per_file = 8388608
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.effective_contact_email().as_deref(),
            Some("researcher@example.org")
        );
        let acquisition = config.acquisition_config();
        assert_eq!(acquisition.max_downloads, 3);
        assert_eq!(acquisition.max_bytes_per_file, 8_388_608);
        assert!(!acquisition.allow_http);
        let sources = config.builtin_sources_config();
        assert!(sources.contact_email.is_some());
    }

    #[test]
    fn env_var_overrides_contact_email() {
        let config = PaperEngineConfig::default();
        // No env var set in tests by default; verify the fallback order is stable.
        std::env::set_var("PAPERENGINE_CONTACT_EMAIL", "env@example.org");
        assert_eq!(
            config.effective_contact_email().as_deref(),
            Some("env@example.org")
        );
        std::env::remove_var("PAPERENGINE_CONTACT_EMAIL");
    }
}
