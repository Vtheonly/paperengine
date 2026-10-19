//! Versioned source-adapter contract and lifecycle (#7).
//!
//! This module defines the boundary every scholarly-source adapter must satisfy. Adapters live
//! in the outer I/O layer: they may perform network requests, but they must never bypass a
//! source's policy. HTTP 403/429, authentication demands, CAPTCHAs and explicit restrictions
//! are surfaced as typed [`SourceError`] values so callers stop or back off — they are never
//! treated as challenges to evade.
//!
//! The contract is versioned through [`SOURCE_ADAPTER_CONTRACT_VERSION`]. A registry refuses
//! adapters built against an incompatible contract revision instead of mis-calling them, and a
//! failing adapter returns an error rather than crashing the process.

use crate::sources::work_record::WorkRecord;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Contract revision every registered adapter must target.
pub const SOURCE_ADAPTER_CONTRACT_VERSION: u32 = 1;

/// Capabilities an adapter can declare. The registry and CLI surface use these to refuse
/// unsupported operations with a typed error instead of a surprise at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Discovery,
    DoiLookup,
    DirectPdfUrls,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Discovery => "discovery",
            Capability::DoiLookup => "doi_lookup",
            Capability::DirectPdfUrls => "direct_pdf_urls",
        }
    }
}

/// Static, network-free description of an adapter: identity, access basis, capabilities, and
/// the politeness policy the engine must honor for this source.
#[derive(Debug, Clone)]
pub struct SourceDescriptor {
    /// Stable machine identifier, e.g. `openalex`.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Adapter implementation version, independent of the contract version.
    pub adapter_version: u32,
    /// Contract revision this adapter was built against.
    pub contract_version: u32,
    /// Canonical API base URL (informational; used for the coverage registry).
    pub api_base: &'static str,
    /// Human-readable statement of why access is permitted and under what terms.
    pub access_basis: &'static str,
    pub capabilities: &'static [Capability],
    /// Rate policy the engine enforces on every request to this source.
    pub rate_policy: RatePolicy,
}

impl SourceDescriptor {
    pub fn supports(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// Politeness and bounding rules for one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatePolicy {
    /// Minimum spacing between two requests to this source.
    pub min_request_interval: Duration,
    /// Page size the adapter requests by default.
    pub default_page_size: u16,
    /// Largest page size the source documents.
    pub max_page_size: u16,
}

/// Typed failure taxonomy. Every variant maps to a documented caller reaction; none of them
/// authorize retry-storming or restriction bypass.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("network transport failed: {detail}")]
    Network { detail: String },
    #[error("source responded {status}; retry-after hint: {retry_after_seconds:?}s")]
    RateLimited {
        status: u16,
        retry_after_seconds: Option<u64>,
    },
    #[error("source explicitly refused access with HTTP {status}; this is a stop condition and is never bypassed")]
    Forbidden { status: u16 },
    #[error("source requires credentials that were not provided (HTTP {status})")]
    Unauthorized { status: u16 },
    #[error("requested resource does not exist (HTTP {status})")]
    NotFound { status: u16 },
    #[error("source returned a server error (HTTP {status})")]
    ServerError { status: u16 },
    #[error("source returned an unexpected HTTP status {status}")]
    UnexpectedStatus { status: u16 },
    #[error("response could not be parsed as documented: {detail}")]
    MalformedResponse { detail: String },
    #[error("source does not support capability: {0}")]
    Unsupported(Capability),
    #[error("request or page budget exhausted: {detail}")]
    BudgetExhausted { detail: String },
    #[error("adapter configuration is invalid: {detail}")]
    InvalidConfiguration { detail: String },
    #[error("writing discovery output failed: {detail}")]
    OutputWrite { detail: String },
}

impl std::fmt::Display for Capability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl SourceError {
    /// Classification used by reports to distinguish outage from implementation bug.
    pub fn class(&self) -> SourceErrorClass {
        match self {
            SourceError::Network { .. }
            | SourceError::ServerError { .. }
            | SourceError::RateLimited { .. } => SourceErrorClass::Transient,
            SourceError::Forbidden { .. } | SourceError::Unauthorized { .. } => {
                SourceErrorClass::AccessDenied
            }
            SourceError::NotFound { .. } => SourceErrorClass::Missing,
            SourceError::MalformedResponse { .. } | SourceError::UnexpectedStatus { .. } => {
                SourceErrorClass::Implementation
            }
            SourceError::Unsupported(_)
            | SourceError::BudgetExhausted { .. }
            | SourceError::InvalidConfiguration { .. }
            | SourceError::OutputWrite { .. } => SourceErrorClass::Configuration,
        }
    }
}

/// Coarse failure classification for reports and circuit breakers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceErrorClass {
    Transient,
    AccessDenied,
    Missing,
    Implementation,
    Configuration,
}

/// Query for discovery. `text` is a free-form scholarly query; adapters translate it into the
/// source's documented query syntax.
#[derive(Debug, Clone)]
pub struct DiscoveryQuery {
    pub text: String,
    pub page_size: u16,
}

impl DiscoveryQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            page_size: 0,
        }
    }
}

/// One page of discovery results plus the opaque cursor for the next page, if any.
#[derive(Debug, Clone)]
pub struct DiscoveryPage {
    pub records: Vec<WorkRecord>,
    pub next_cursor: Option<String>,
}

/// Offline self-test describing what an adapter can do with its current configuration. It
/// performs no network I/O; a live health check belongs to the opt-in live test suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfTestReport {
    pub source_id: String,
    pub adapter_version: u32,
    pub contract_version: u32,
    pub capabilities: Vec<String>,
    pub configured: bool,
    pub notes: Vec<String>,
}

/// The versioned source-adapter contract. Default methods return typed
/// [`SourceError::Unsupported`] responses so partial adapters stay honest about capability.
#[async_trait]
pub trait SourceAdapter: Send + Sync {
    fn descriptor(&self) -> &SourceDescriptor;

    fn self_test(&self) -> SelfTestReport;

    async fn discover(
        &self,
        _query: &DiscoveryQuery,
        _cursor: Option<&str>,
    ) -> Result<DiscoveryPage, SourceError> {
        Err(SourceError::Unsupported(Capability::Discovery))
    }

    async fn lookup_doi(&self, _doi: &str) -> Result<Option<WorkRecord>, SourceError> {
        Err(SourceError::Unsupported(Capability::DoiLookup))
    }
}

/// Failure modes of registration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("adapter `{0}` targets contract version {1}, but this engine supports {SOURCE_ADAPTER_CONTRACT_VERSION}")]
    IncompatibleContract(String, u32),
    #[error("an adapter with id `{0}` is already registered")]
    DuplicateId(String),
}

/// Registry holding compatible adapters. Incompatible plugins are refused with a typed error
/// instead of being called through a mismatched vtable.
#[derive(Default)]
pub struct SourceRegistry {
    adapters: Vec<Box<dyn SourceAdapter>>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an adapter after checking its contract version.
    pub fn register(&mut self, adapter: Box<dyn SourceAdapter>) -> Result<(), RegistrationError> {
        let descriptor = adapter.descriptor();
        if descriptor.contract_version != SOURCE_ADAPTER_CONTRACT_VERSION {
            return Err(RegistrationError::IncompatibleContract(
                descriptor.id.to_owned(),
                descriptor.contract_version,
            ));
        }
        if self.get(descriptor.id).is_some() {
            return Err(RegistrationError::DuplicateId(descriptor.id.to_owned()));
        }
        self.adapters.push(adapter);
        Ok(())
    }

    pub fn get(&self, source_id: &str) -> Option<&dyn SourceAdapter> {
        self.adapters
            .iter()
            .map(|adapter| adapter.as_ref())
            .find(|adapter| adapter.descriptor().id == source_id)
    }

    /// Register an adapter, skipping it cleanly when it targets an incompatible contract.
    /// Returns a diagnostic note for each skipped plugin.
    pub fn register_compatible(
        &mut self,
        adapter: Box<dyn SourceAdapter>,
    ) -> Result<Option<String>, RegistrationError> {
        let descriptor = adapter.descriptor();
        if descriptor.contract_version != SOURCE_ADAPTER_CONTRACT_VERSION {
            return Ok(Some(format!(
                "skipped source `{}` targeting incompatible contract version {}",
                descriptor.id, descriptor.contract_version
            )));
        }
        if self.get(descriptor.id).is_some() {
            return Err(RegistrationError::DuplicateId(descriptor.id.to_owned()));
        }
        self.adapters.push(adapter);
        Ok(None)
    }

    pub fn source_ids(&self) -> Vec<&'static str> {
        self.adapters
            .iter()
            .map(|adapter| adapter.descriptor().id)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    pub fn self_tests(&self) -> Vec<SelfTestReport> {
        self.adapters.iter().map(|a| a.self_test()).collect()
    }
}

/// Enforces per-source request spacing. The engine waits for a turn instead of hammering a
/// source; spacing is applied by the adapter's shared HTTP client.
#[derive(Debug)]
pub struct RequestPacer {
    min_interval: Duration,
    last_release: Mutex<Option<Instant>>,
}

impl RequestPacer {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_release: Mutex::new(None),
        }
    }

    /// Sleep until at least `min_interval` has elapsed since the previous release.
    pub async fn wait_turn(&self) {
        let wait = {
            let mut last = self.last_release.lock().expect("pacer lock poisoned");
            let now = Instant::now();
            let ready_at = match *last {
                Some(previous) => previous + self.min_interval,
                None => now,
            };
            if ready_at <= now {
                *last = Some(now);
                return;
            }
            *last = Some(ready_at);
            ready_at - now
        };
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubAdapter {
        descriptor: SourceDescriptor,
    }

    impl StubAdapter {
        fn new(id: &'static str, contract_version: u32) -> Self {
            Self {
                descriptor: SourceDescriptor {
                    id,
                    display_name: id,
                    adapter_version: 1,
                    contract_version,
                    api_base: "https://example.org/api",
                    access_basis: "public API",
                    capabilities: &[Capability::Discovery],
                    rate_policy: RatePolicy {
                        min_request_interval: Duration::from_millis(1),
                        default_page_size: 10,
                        max_page_size: 100,
                    },
                },
            }
        }
    }

    #[async_trait]
    impl SourceAdapter for StubAdapter {
        fn descriptor(&self) -> &SourceDescriptor {
            &self.descriptor
        }

        fn self_test(&self) -> SelfTestReport {
            SelfTestReport {
                source_id: self.descriptor.id.to_owned(),
                adapter_version: self.descriptor.adapter_version,
                contract_version: self.descriptor.contract_version,
                capabilities: vec![Capability::Discovery.as_str().to_owned()],
                configured: true,
                notes: Vec::new(),
            }
        }
    }

    #[test]
    fn registry_refuses_incompatible_contract_version() {
        let mut registry = SourceRegistry::new();
        let error = registry
            .register(Box::new(StubAdapter::new("future", 99)))
            .unwrap_err();
        assert_eq!(
            error,
            RegistrationError::IncompatibleContract("future".to_owned(), 99)
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn register_compatible_skips_incompatible_plugin_cleanly() {
        let mut registry = SourceRegistry::new();
        let note = registry
            .register_compatible(Box::new(StubAdapter::new("future", 2)))
            .unwrap()
            .expect("incompatible plugin must produce a skip note");
        assert!(note.contains("incompatible"));
        assert!(registry.get("future").is_none());

        assert!(registry
            .register_compatible(Box::new(StubAdapter::new("present", 1)))
            .unwrap()
            .is_none());
        assert!(registry.get("present").is_some());
    }

    #[test]
    fn registry_rejects_duplicate_ids() {
        let mut registry = SourceRegistry::new();
        registry
            .register(Box::new(StubAdapter::new("dup", 1)))
            .unwrap();
        assert_eq!(
            registry
                .register(Box::new(StubAdapter::new("dup", 1)))
                .unwrap_err(),
            RegistrationError::DuplicateId("dup".to_owned())
        );
    }

    #[tokio::test]
    async fn unsupported_capabilities_return_typed_errors() {
        struct MetadataOnly {
            descriptor: SourceDescriptor,
        }

        #[async_trait]
        impl SourceAdapter for MetadataOnly {
            fn descriptor(&self) -> &SourceDescriptor {
                &self.descriptor
            }

            fn self_test(&self) -> SelfTestReport {
                SelfTestReport {
                    source_id: "meta".into(),
                    adapter_version: 1,
                    contract_version: 1,
                    capabilities: vec![],
                    configured: true,
                    notes: vec![],
                }
            }
        }

        let adapter = MetadataOnly {
            descriptor: SourceDescriptor {
                id: "meta",
                display_name: "Meta",
                adapter_version: 1,
                contract_version: 1,
                api_base: "https://example.org",
                access_basis: "public",
                capabilities: &[],
                rate_policy: RatePolicy {
                    min_request_interval: Duration::from_millis(1),
                    default_page_size: 1,
                    max_page_size: 1,
                },
            },
        };

        let error = adapter
            .discover(&DiscoveryQuery::new("query"), None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SourceError::Unsupported(Capability::Discovery)
        ));
        assert_eq!(error.class(), SourceErrorClass::Configuration);

        let error = adapter.lookup_doi("10.1/x").await.unwrap_err();
        assert!(matches!(
            error,
            SourceError::Unsupported(Capability::DoiLookup)
        ));
    }

    #[test]
    fn error_classes_distinguish_outage_access_and_bugs() {
        assert_eq!(
            SourceError::RateLimited {
                status: 429,
                retry_after_seconds: Some(2)
            }
            .class(),
            SourceErrorClass::Transient
        );
        assert_eq!(
            SourceError::Forbidden { status: 403 }.class(),
            SourceErrorClass::AccessDenied
        );
        assert_eq!(
            SourceError::MalformedResponse {
                detail: "bad json".into()
            }
            .class(),
            SourceErrorClass::Implementation
        );
    }

    #[tokio::test]
    async fn pacer_spaces_consecutive_turns() {
        let pacer = RequestPacer::new(Duration::from_millis(50));
        let start = Instant::now();
        pacer.wait_turn().await;
        pacer.wait_turn().await;
        assert!(start.elapsed() >= Duration::from_millis(45));
    }
}
