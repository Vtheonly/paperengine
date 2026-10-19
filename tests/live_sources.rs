//! Opt-in live-source tests (Layers 2 and 3).
//!
//! These tests contact real scholarly APIs and, for sources with permitted direct PDFs,
//! run the actual acquisition pipeline. They are excluded from normal runs in two ways:
//!
//! 1. Every test carries `#[ignore]`, so `cargo test` skips them unless `--ignored` is
//!    passed explicitly.
//! 2. Even when run with `--ignored`, every test returns early unless
//!    `PAPERENGINE_LIVE_TESTS=1` is set in the environment.
//!
//! Budgets are deliberately tiny (one page, a handful of records, at most one PDF) and the
//! adapters' rate policies space requests politely. A source that answers 429 or 403 is
//! reported as BLOCKED on stderr rather than hammered; that is a stop condition, never a
//! challenge. Run with:
//!
//! ```text
//! PAPERENGINE_LIVE_TESTS=1 cargo test --test live_sources -- --ignored --nocapture
//! ```
//!
//! To additionally exercise a real PDF acquisition (Layer 3), set
//! `PAPERENGINE_LIVE_ACQUIRE=1`; it downloads exactly one small open-access paper from
//! arXiv through the production pipeline.

use paperengine::library::{
    run_acquisition, AcquisitionConfig, AcquisitionOutcome, AcquisitionReportEntry, LibraryLayout,
};
use paperengine::sources::arxiv::{ArxivAdapter, ArxivConfig};
use paperengine::sources::crossref::{CrossrefAdapter, CrossrefConfig};
use paperengine::sources::openalex::{OpenAlexAdapter, OpenAlexConfig};
use paperengine::sources::unpaywall::{UnpaywallAdapter, UnpaywallConfig};
use paperengine::sources::{DiscoveryQuery, SourceAdapter, SourceError};
use std::time::Duration;

fn live_tests_enabled() -> bool {
    if std::env::var("PAPERENGINE_LIVE_TESTS").ok().as_deref() == Some("1") {
        true
    } else {
        eprintln!(
            "skipping live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored to enable"
        );
        false
    }
}

fn live_acquire_enabled() -> bool {
    if std::env::var("PAPERENGINE_LIVE_ACQUIRE").ok().as_deref() == Some("1") {
        true
    } else {
        eprintln!(
            "skipping live acquisition: set PAPERENGINE_LIVE_ACQUIRE=1 to download one real PDF"
        );
        false
    }
}

const CONTACT_EMAIL: &str = "paperengine@users.noreply.github.com";

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_arxiv_discovery_returns_parseable_records_with_pdf_links() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = ArxivAdapter::new(ArxivConfig::default()).unwrap();
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "attention is all you need".into(),
                page_size: 3,
            },
            None,
        )
        .await
        .expect("arXiv API should be reachable");
    assert!(!page.records.is_empty(), "arXiv should return records");
    for record in &page.records {
        assert!(
            record
                .source_record_id
                .starts_with(|c: char| c.is_ascii_digit())
                || record.source_record_id.contains('/'),
            "arXiv ids must parse, got {}",
            record.source_record_id
        );
        assert!(record
            .oa_pdf_url
            .as_deref()
            .unwrap_or("")
            .starts_with("https://arxiv.org/pdf/"));
        assert!(record.publication_year.is_some());
        assert!(!record.title.is_empty());
    }
    eprintln!(
        "arXiv discovery OK: {} records, first id {} ({})",
        page.records.len(),
        page.records[0].source_record_id,
        page.records[0].title
    );
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_arxiv_end_to_end_pdf_acquisition() {
    if !live_tests_enabled() || !live_acquire_enabled() {
        return;
    }
    let adapter = ArxivAdapter::new(ArxivConfig::default()).unwrap();
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "attention is all you need".into(),
                page_size: 3,
            },
            None,
        )
        .await
        .expect("arXiv API should be reachable");
    let record = page
        .records
        .iter()
        .find(|record| record.oa_pdf_url.is_some())
        .expect("arXiv records expose direct PDF links");

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = AcquisitionConfig {
        max_downloads: 1,
        min_bytes_per_file: 1024,
        max_bytes_per_file: 8 * 1024 * 1024,
        max_total_bytes: 8 * 1024 * 1024,
        request_timeout: Duration::from_secs(60),
        total_timeout: Duration::from_secs(180),
        inter_download_delay: Duration::from_secs(0),
        allow_http: false,
    };
    let downloader =
        paperengine::library::acquisition_downloader(&config).expect("downloader builds");

    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(record),
        &library,
        &config,
        &downloader,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        |entry: &AcquisitionReportEntry| {
            entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .expect("acquisition pipeline should run");

    assert_eq!(summary.acquired, 1, "entries: {entries:?}");
    let entry: AcquisitionReportEntry = serde_json::from_str(&entries[0]).unwrap();
    let AcquisitionOutcome::Acquired {
        byte_length,
        sha256_hex,
        page_count,
        object_path,
        ..
    } = entry.outcome
    else {
        panic!("live acquisition did not succeed: {:?}", entry.outcome)
    };
    assert!(byte_length > 0);
    assert!(page_count > 0);
    assert!(sha256_hex.len() == 64);
    assert!(object_path.contains("objects"));

    // Evidence line for the verification report.
    eprintln!(
        "ARXIV ACQUISITION EVIDENCE: id={} url={} bytes={} sha256={} pages={} object={}",
        record.source_record_id,
        record.oa_pdf_url.as_deref().unwrap_or("<none>"),
        byte_length,
        sha256_hex,
        page_count,
        object_path
    );

    // The finalized object re-opens through the structural parser.
    let manifests = library.list_manifests().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].work.source_id, "arxiv");
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_crossref_discovery_returns_metadata_without_pdf_links() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = CrossrefAdapter::new(CrossrefConfig {
        contact_email: Some(CONTACT_EMAIL.into()),
        ..CrossrefConfig::default()
    })
    .unwrap();
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "attention is all you need".into(),
                page_size: 3,
            },
            None,
        )
        .await
        .expect("Crossref API should be reachable");
    assert!(!page.records.is_empty(), "Crossref should return records");
    for record in &page.records {
        assert!(record.doi().is_some(), "every Crossref record has a DOI");
        assert!(
            record.oa_pdf_url.is_none(),
            "Crossref must never claim direct OA PDFs"
        );
        assert!(record.landing_url.is_some());
    }
    eprintln!(
        "Crossref discovery OK: {} records, first DOI {} ({})",
        page.records.len(),
        page.records[0].doi().unwrap_or("<none>"),
        page.records[0].title
    );
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_crossref_doi_lookup_resolves_a_stable_doi() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = CrossrefAdapter::new(CrossrefConfig {
        contact_email: Some(CONTACT_EMAIL.into()),
        ..CrossrefConfig::default()
    })
    .unwrap();
    // A deliberately stable, famous record: Watson & Crick (1953). Verified live
    // 2026-10-19: Nature re-registered the legacy record as 10.1038/171737a0 —
    // pre-1956 records ARE indexed under their `…a0` forms; the historically
    // cited `171737a` (no trailing zero) answers 404 through both Crossref and
    // doi.org and must not be used as test data.
    let record = adapter
        .lookup_doi("10.1038/171737a0")
        .await
        .expect("Crossref lookup should succeed")
        .expect("the DOI must resolve");
    assert!(record.title.to_lowercase().contains("structure"));
    assert_eq!(record.doi(), Some("10.1038/171737a0"));
    eprintln!("Crossref DOI lookup OK: {}", record.title);
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_unpaywall_resolves_an_oa_doi_with_a_direct_pdf() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = UnpaywallAdapter::new(UnpaywallConfig {
        contact_email: Some(CONTACT_EMAIL.into()),
        ..UnpaywallConfig::default()
    })
    .unwrap();
    // PLOS ONE fully open-access article (verified live on 2026-10-19).
    let record = adapter
        .lookup_doi("10.1371/journal.pone.0000219")
        .await
        .expect("Unpaywall lookup should succeed")
        .expect("the DOI must resolve");
    assert!(record.raw["is_oa"].as_bool().unwrap());
    assert!(
        record
            .oa_pdf_url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://")),
        "best OA location for this DOI carries url_for_pdf"
    );
    eprintln!(
        "Unpaywall lookup OK: {} -> {}",
        record.title,
        record.oa_pdf_url.as_deref().unwrap_or("<none>")
    );
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_openalex_doi_lookup_resolves_a_work() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = OpenAlexAdapter::new(OpenAlexConfig {
        contact_email: Some(CONTACT_EMAIL.into()),
        ..OpenAlexConfig::default()
    })
    .unwrap();
    let record = adapter
        .lookup_doi("10.7717/peerj.4375")
        .await
        .expect("OpenAlex lookup should succeed")
        .expect("the DOI must resolve");
    assert_eq!(record.source_record_id, "W2741809807");
    assert!(record.title.contains("state of OA"));
    eprintln!("OpenAlex DOI lookup OK: {}", record.title);
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_openalex_discovery_or_blocked_by_free_tier_budget() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = OpenAlexAdapter::new(OpenAlexConfig {
        contact_email: Some(CONTACT_EMAIL.into()),
        ..OpenAlexConfig::default()
    })
    .unwrap();
    match adapter
        .discover(
            &DiscoveryQuery {
                text: "open access".into(),
                page_size: 2,
            },
            None,
        )
        .await
    {
        Ok(page) => {
            assert!(!page.records.is_empty());
            eprintln!(
                "OpenAlex discovery OK: {} records, first {}",
                page.records.len(),
                page.records[0].source_record_id
            );
        }
        Err(SourceError::RateLimited { status, .. }) => {
            // The 2026 free tier shares a daily budget per IP. This is a source-side
            // availability limit, not an implementation failure; an API key lifts it.
            eprintln!(
                "BLOCKED: OpenAlex free-tier budget exhausted on this IP (HTTP {status}); \
                 configure openalex_api_key to verify discovery live"
            );
        }
        Err(error) => panic!("unexpected OpenAlex error: {error}"),
    }
}

// ---------------------------------------------------------------------------
// Live tests for the second adapter wave (#12): Europe PMC, PMC, DOAJ, Zenodo,
// OSF. Same rules as above: opt-in through PAPERENGINE_LIVE_TESTS=1, bounded
// pages, one PDF at most per acquisition test through PAPERENGINE_LIVE_ACQUIRE=1.
// ---------------------------------------------------------------------------

use paperengine::sources::doaj::{DoajAdapter, DoajConfig};
use paperengine::sources::europepmc::{EuropePmcAdapter, EuropePmcConfig};
use paperengine::sources::osf::{OsfAdapter, OsfConfig};
use paperengine::sources::pmc::{PmcAdapter, PmcConfig};
use paperengine::sources::zenodo::{ZenodoAdapter, ZenodoConfig};

/// Run one real acquisition through the production pipeline and return the
/// evidence line. Shared by the per-source Layer 3 tests below.
async fn acquire_one_real_pdf(record: &paperengine::sources::WorkRecord, source_label: &str) {
    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = AcquisitionConfig {
        max_downloads: 1,
        min_bytes_per_file: 1024,
        max_bytes_per_file: 8 * 1024 * 1024,
        max_total_bytes: 8 * 1024 * 1024,
        request_timeout: Duration::from_secs(90),
        total_timeout: Duration::from_secs(240),
        inter_download_delay: Duration::from_secs(0),
        allow_http: false,
    };
    let downloader =
        paperengine::library::acquisition_downloader(&config).expect("downloader builds");
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(record),
        &library,
        &config,
        &downloader,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        |entry: &AcquisitionReportEntry| {
            entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .expect("acquisition pipeline should run");

    assert_eq!(summary.acquired, 1, "entries: {entries:?}");
    let entry: AcquisitionReportEntry = serde_json::from_str(&entries[0]).unwrap();
    let AcquisitionOutcome::Acquired {
        byte_length,
        sha256_hex,
        page_count,
        object_path,
        ..
    } = entry.outcome
    else {
        panic!(
            "{source_label} acquisition did not succeed: {:?}",
            entry.outcome
        )
    };
    assert!(byte_length > 0);
    assert!(page_count > 0, "structural PDF parse must report pages");
    assert!(sha256_hex.len() == 64);
    assert!(object_path.contains("objects"));
    let manifests = library.list_manifests().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].work.source_id, record.source_id);
    eprintln!(
        "{source_label} ACQUISITION EVIDENCE: id={} url={} bytes={} sha256={} pages={} object={}",
        record.source_record_id,
        record.oa_pdf_url.as_deref().unwrap_or("<none>"),
        byte_length,
        sha256_hex,
        page_count,
        object_path
    );
}

// --- Europe PMC -----------------------------------------------------------

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_europepmc_discovery_returns_metadata_without_pdf_links() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = EuropePmcAdapter::new(EuropePmcConfig::default()).unwrap();
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "DESeq2".into(),
                page_size: 3,
            },
            None,
        )
        .await
        .expect("Europe PMC API should be reachable");
    assert!(!page.records.is_empty(), "Europe PMC should return records");
    for record in &page.records {
        assert!(!record.title.is_empty());
        // Europe PMC has no documented public PDF endpoint; it must never claim one.
        assert!(record.oa_pdf_url.is_none(), "record {record:?}");
    }
    let first = &page.records[0];
    assert!(first
        .identifier(paperengine::sources::IdentifierKind::Pmc)
        .is_some());
    eprintln!(
        "Europe PMC discovery OK: {} records, first {} ({})",
        page.records.len(),
        first.source_record_id,
        first.title
    );
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_europepmc_doi_lookup_resolves_the_deseq2_paper() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = EuropePmcAdapter::new(EuropePmcConfig::default()).unwrap();
    let record = adapter
        .lookup_doi("10.1186/s13059-014-0550-8")
        .await
        .expect("Europe PMC lookup should succeed")
        .expect("the DOI must resolve");
    assert_eq!(record.source_record_id, "25516281");
    assert_eq!(
        record.identifier(paperengine::sources::IdentifierKind::Pmc),
        Some("PMC4302049")
    );
    assert!(record.title.contains("DESeq2"));
    eprintln!("Europe PMC DOI lookup OK: {}", record.title);
}

// --- PubMed Central --------------------------------------------------------

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_pmc_discovery_returns_metadata_without_pdf_links() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = PmcAdapter::new(PmcConfig::default()).unwrap();
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "DESeq2[title]".into(),
                page_size: 3,
            },
            None,
        )
        .await
        .expect("E-utilities should be reachable");
    assert!(!page.records.is_empty(), "PMC should return records");
    for record in &page.records {
        assert!(record
            .identifier(paperengine::sources::IdentifierKind::Pmc)
            .is_some());
        assert!(record.oa_pdf_url.is_none(), "PMC must never claim a PDF");
        assert!(!record.title.is_empty());
    }
    eprintln!(
        "PMC discovery OK: {} records, first {} ({})",
        page.records.len(),
        page.records[0].source_record_id,
        page.records[0].title
    );
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_pmc_doi_lookup_resolves_the_deseq2_paper() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = PmcAdapter::new(PmcConfig::default()).unwrap();
    let record = adapter
        .lookup_doi("10.1186/s13059-014-0550-8")
        .await
        .expect("PMC lookup should succeed")
        .expect("the DOI must resolve");
    assert_eq!(record.source_record_id, "PMC4302049");
    assert!(record.title.contains("DESeq2"));
    assert_eq!(record.venue.as_deref(), Some("Genome biology"));
    eprintln!("PMC DOI lookup OK: {}", record.title);
}

// --- DOAJ ------------------------------------------------------------------

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_doaj_discovery_returns_records_with_publisher_pdf_links() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = DoajAdapter::new(DoajConfig::default()).unwrap();
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "attention is all you need".into(),
                page_size: 5,
            },
            None,
        )
        .await
        .expect("DOAJ API should be reachable");
    assert!(!page.records.is_empty(), "DOAJ should return records");
    let with_pdf = page
        .records
        .iter()
        .filter(|record| record.oa_pdf_url.is_some())
        .count();
    eprintln!(
        "DOAJ discovery OK: {} records, {with_pdf} with publisher-deposited PDF links; first: {}",
        page.records.len(),
        page.records[0].title
    );
    // Not every OA deposit includes a direct PDF link; those without one are honest.
    for record in &page.records {
        if let Some(url) = record.oa_pdf_url.as_deref() {
            assert!(url.starts_with("https://"), "http must be upgraded: {url}");
        }
    }
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_doaj_doi_lookup_resolves_the_jis_article() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = DoajAdapter::new(DoajConfig::default()).unwrap();
    let record = adapter
        .lookup_doi("10.22103/jis.2019.13389.1903")
        .await
        .expect("DOAJ lookup should succeed")
        .expect("the DOI must resolve");
    assert!(record.oa_pdf_url.is_some());
    eprintln!(
        "DOAJ DOI lookup OK: {} -> {}",
        record.title,
        record.oa_pdf_url.as_deref().unwrap_or("<none>")
    );
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored; PAPERENGINE_LIVE_ACQUIRE=1 for the download"]
async fn live_doaj_end_to_end_pdf_acquisition() {
    if !live_tests_enabled() || !live_acquire_enabled() {
        return;
    }
    let adapter = DoajAdapter::new(DoajConfig::default()).unwrap();
    let record = adapter
        .lookup_doi("10.22103/jis.2019.13389.1903")
        .await
        .expect("DOAJ lookup should succeed")
        .expect("the DOI must resolve");
    assert!(record.oa_pdf_url.is_some());
    acquire_one_real_pdf(&record, "DOAJ").await;
}

// --- Zenodo ----------------------------------------------------------------

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_zenodo_discovery_returns_publication_records_with_pdf_links() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = ZenodoAdapter::new(ZenodoConfig::default()).unwrap();
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "attention is all you need".into(),
                page_size: 3,
            },
            None,
        )
        .await
        .expect("Zenodo API should be reachable");
    assert!(!page.records.is_empty(), "Zenodo should return records");
    let with_pdf = page
        .records
        .iter()
        .filter(|record| record.oa_pdf_url.is_some())
        .count();
    assert!(
        with_pdf > 0,
        "quoted search should surface PDF publications"
    );
    for record in &page.records {
        if let Some(url) = record.oa_pdf_url.as_deref() {
            assert!(url.starts_with("https://zenodo.org/api/records/"));
        }
    }
    eprintln!(
        "Zenodo discovery OK: {} records, {with_pdf} with direct PDF links; first: {}",
        page.records.len(),
        page.records[0].title
    );
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_zenodo_doi_lookup_resolves_a_publication() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = ZenodoAdapter::new(ZenodoConfig::default()).unwrap();
    let record = adapter
        .lookup_doi("10.5281/zenodo.18289747")
        .await
        .expect("Zenodo lookup should succeed")
        .expect("the DOI must resolve");
    assert_eq!(record.source_record_id, "18289747");
    assert!(record.oa_pdf_url.is_some());
    eprintln!(
        "Zenodo DOI lookup OK: {} -> {}",
        record.title,
        record.oa_pdf_url.as_deref().unwrap_or("<none>")
    );
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored; PAPERENGINE_LIVE_ACQUIRE=1 for the download"]
async fn live_zenodo_end_to_end_pdf_acquisition() {
    if !live_tests_enabled() || !live_acquire_enabled() {
        return;
    }
    let adapter = ZenodoAdapter::new(ZenodoConfig::default()).unwrap();
    let record = adapter
        .lookup_doi("10.5281/zenodo.18289747")
        .await
        .expect("Zenodo lookup should succeed")
        .expect("the DOI must resolve");
    assert!(record.oa_pdf_url.is_some());
    acquire_one_real_pdf(&record, "ZENODO").await;
}

// --- OSF -------------------------------------------------------------------

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored"]
async fn live_osf_discovery_resolves_primary_files() {
    if !live_tests_enabled() {
        return;
    }
    let adapter = OsfAdapter::new(OsfConfig::default()).unwrap();
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "open access".into(),
                page_size: 3,
            },
            None,
        )
        .await
        .expect("OSF API should be reachable");
    assert!(!page.records.is_empty(), "OSF should return preprints");
    let with_pdf = page
        .records
        .iter()
        .filter(|record| record.oa_pdf_url.is_some())
        .count();
    eprintln!(
        "OSF discovery OK: {} preprints, {with_pdf} with resolved primary-file PDFs; first: {}",
        page.records.len(),
        page.records[0].title
    );
    for record in &page.records {
        if let Some(url) = record.oa_pdf_url.as_deref() {
            assert!(url.starts_with("https://osf.io/download/"));
        }
    }
}

#[tokio::test]
#[ignore = "live test: set PAPERENGINE_LIVE_TESTS=1 and run with --ignored; PAPERENGINE_LIVE_ACQUIRE=1 for the download"]
async fn live_osf_end_to_end_pdf_acquisition() {
    if !live_tests_enabled() || !live_acquire_enabled() {
        return;
    }
    let adapter = OsfAdapter::new(OsfConfig::default()).unwrap();
    // Stable published MetaArXiv preprint whose primary file was verified live.
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "Middle Way proposal towards an equitable open access".into(),
                page_size: 5,
            },
            None,
        )
        .await
        .expect("OSF API should be reachable");
    let record = page
        .records
        .iter()
        .find(|record| record.oa_pdf_url.is_some())
        .expect("the MetaArXiv preprint must expose a primary-file PDF");
    acquire_one_real_pdf(record, "OSF").await;
}
