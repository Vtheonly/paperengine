//! Source adapters for scholarly metadata providers.
//!
//! This module is the outer I/O layer for discovery: versioned contracts, normalization,
//! per-source adapters, and a bounded discovery runner. It contains no downloader; payload
//! acquisition lives in [`crate::fetcher`] and [`crate::library`].

pub mod arxiv;
pub mod contract;
pub mod crossref;
pub mod doaj;
pub mod europepmc;
pub mod http;
pub mod normalize;
pub mod oaipmh;
pub mod openalex;
pub mod osf;
pub mod pmc;
pub mod runner;
pub mod unpaywall;
pub mod work_record;
pub mod zenodo;

pub use contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, RegistrationError, RequestPacer,
    SelfTestReport, SourceAdapter, SourceDescriptor, SourceError, SourceErrorClass, SourceRegistry,
    SOURCE_ADAPTER_CONTRACT_VERSION,
};
pub use runner::{run_discovery, DiscoveryBudget, DiscoveryRunSummary};
pub use work_record::{IdentifierKind, WorkIdentifier, WorkRecord, WORK_RECORD_SCHEMA_VERSION};

use crate::sources::arxiv::{ArxivAdapter, ArxivConfig};
use crate::sources::crossref::{CrossrefAdapter, CrossrefConfig};
use crate::sources::doaj::{DoajAdapter, DoajConfig};
use crate::sources::europepmc::{EuropePmcAdapter, EuropePmcConfig};
use crate::sources::oaipmh::{OaiPmhAdapter, OaiPmhConfig};
use crate::sources::openalex::{OpenAlexAdapter, OpenAlexConfig};
use crate::sources::osf::{OsfAdapter, OsfConfig};
use crate::sources::pmc::{PmcAdapter, PmcConfig};
use crate::sources::unpaywall::{UnpaywallAdapter, UnpaywallConfig};
use crate::sources::zenodo::{ZenodoAdapter, ZenodoConfig};

/// Shared contact configuration for the built-in adapters.
#[derive(Debug, Clone, Default)]
pub struct BuiltinSourcesConfig {
    pub contact_email: Option<String>,
    pub openalex_api_key: Option<String>,
    /// Per-source request override for tests (0 disables spacing).
    pub min_request_interval: Option<std::time::Duration>,
    pub request_timeout: Option<std::time::Duration>,
}

/// Build a registry with the built-in adapters for the sources implemented so far.
/// Adapters that cannot run with the given configuration (e.g. Unpaywall without an
/// e-mail) are skipped with a note instead of failing the whole registry.
pub fn builtin_registry(config: &BuiltinSourcesConfig) -> (SourceRegistry, Vec<String>) {
    let mut registry = SourceRegistry::new();
    let mut notes = Vec::new();

    let interval = config.min_request_interval;
    let timeout = config.request_timeout;

    let openalex = OpenAlexAdapter::new(OpenAlexConfig {
        contact_email: config.contact_email.clone(),
        api_key: config.openalex_api_key.clone(),
        min_request_interval: interval,
        request_timeout: timeout,
        ..OpenAlexConfig::default()
    })
    .expect("openalex adapter construction cannot fail");
    if let Err(error) = registry.register(Box::new(openalex)) {
        notes.push(format!("openalex: {error}"));
    }

    let arxiv = ArxivAdapter::new(ArxivConfig {
        min_request_interval: interval,
        request_timeout: timeout,
        ..ArxivConfig::default()
    })
    .expect("arxiv adapter construction cannot fail");
    if let Err(error) = registry.register(Box::new(arxiv)) {
        notes.push(format!("arxiv: {error}"));
    }

    let crossref = CrossrefAdapter::new(CrossrefConfig {
        contact_email: config.contact_email.clone(),
        min_request_interval: interval,
        request_timeout: timeout,
        ..CrossrefConfig::default()
    })
    .expect("crossref adapter construction cannot fail");
    if let Err(error) = registry.register(Box::new(crossref)) {
        notes.push(format!("crossref: {error}"));
    }

    match UnpaywallAdapter::new(UnpaywallConfig {
        contact_email: config.contact_email.clone(),
        min_request_interval: interval,
        request_timeout: timeout,
        ..UnpaywallConfig::default()
    }) {
        Ok(unpaywall) => {
            if let Err(error) = registry.register(Box::new(unpaywall)) {
                notes.push(format!("unpaywall: {error}"));
            }
        }
        Err(error) => notes.push(format!("unpaywall skipped: {error}")),
    }

    let europepmc = EuropePmcAdapter::new(EuropePmcConfig {
        min_request_interval: interval,
        request_timeout: timeout,
        ..EuropePmcConfig::default()
    })
    .expect("europepmc adapter construction cannot fail");
    if let Err(error) = registry.register(Box::new(europepmc)) {
        notes.push(format!("europepmc: {error}"));
    }

    let pmc = PmcAdapter::new(PmcConfig {
        min_request_interval: interval,
        request_timeout: timeout,
        ..PmcConfig::default()
    })
    .expect("pmc adapter construction cannot fail");
    if let Err(error) = registry.register(Box::new(pmc)) {
        notes.push(format!("pmc: {error}"));
    }

    let doaj = DoajAdapter::new(DoajConfig {
        min_request_interval: interval,
        request_timeout: timeout,
        ..DoajConfig::default()
    })
    .expect("doaj adapter construction cannot fail");
    if let Err(error) = registry.register(Box::new(doaj)) {
        notes.push(format!("doaj: {error}"));
    }

    let zenodo = ZenodoAdapter::new(ZenodoConfig {
        min_request_interval: interval,
        request_timeout: timeout,
        ..ZenodoConfig::default()
    })
    .expect("zenodo adapter construction cannot fail");
    if let Err(error) = registry.register(Box::new(zenodo)) {
        notes.push(format!("zenodo: {error}"));
    }

    let osf = OsfAdapter::new(OsfConfig {
        min_request_interval: interval,
        request_timeout: timeout,
        ..OsfConfig::default()
    })
    .expect("osf adapter construction cannot fail");
    if let Err(error) = registry.register(Box::new(osf)) {
        notes.push(format!("osf: {error}"));
    }

    for config in [
        OaiPmhConfig::zenodo(),
        OaiPmhConfig::doaj(),
        OaiPmhConfig::pmc(),
    ] {
        let id = config.source_id;
        let adapter = OaiPmhAdapter::new(config).expect("oaipmh adapter construction cannot fail");
        if let Err(error) = registry.register(Box::new(adapter)) {
            notes.push(format!("{id}: {error}"));
        }
    }

    (registry, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_skips_unpaywall_without_email() {
        let (registry, notes) = builtin_registry(&BuiltinSourcesConfig::default());
        assert!(registry.get("openalex").is_some());
        assert!(registry.get("arxiv").is_some());
        assert!(registry.get("crossref").is_some());
        assert!(registry.get("unpaywall").is_none());
        assert!(notes.iter().any(|note| note.contains("unpaywall skipped")));
    }

    #[test]
    fn builtin_registry_includes_unpaywall_with_email() {
        let (registry, notes) = builtin_registry(&BuiltinSourcesConfig {
            contact_email: Some("researcher@example.org".into()),
            ..BuiltinSourcesConfig::default()
        });
        assert!(registry.get("unpaywall").is_some());
        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
        assert_eq!(registry.source_ids().len(), 12);
    }
}
