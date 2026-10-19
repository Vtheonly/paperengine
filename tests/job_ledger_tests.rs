//! Restart and retry tests for the durable acquisition job ledger (#6).
//!
//! Each test drives the production `run_acquisition` pipeline against the local fixture
//! server with an injected clock, then inspects the durable job records under
//! `library/jobs/` — exactly what a restart would see. Covered: exponential backoff
//! with due-retry recovery, bounded attempts with terminal failure and explicit
//! re-entry, abandoned in-flight recovery after lease expiry, live-lease exclusion,
//! target-change reopening, and ledger round-trips.

mod support;

use paperengine::jobs::{JobLedger, JobPolicy, JobRecord, JobState, JOBS_SCHEMA_VERSION};
use paperengine::library::{
    run_acquisition, AcquisitionOutcome, AcquisitionReportEntry, LibraryLayout,
};
use std::fs;
use support::{
    acquisition_config, downloader_for, record_with_pdf, small_pdf_bytes, FixtureServer, Route,
};

fn push_entry(
    entries: &mut Vec<String>,
) -> impl FnMut(&AcquisitionReportEntry) -> std::io::Result<()> + '_ {
    move |entry: &AcquisitionReportEntry| {
        entries.push(serde_json::to_string(entry).unwrap());
        Ok(())
    }
}

fn parse_entry(line: &str) -> AcquisitionReportEntry {
    serde_json::from_str(line).unwrap()
}

fn fast_policy() -> JobPolicy {
    JobPolicy {
        max_attempts: 3,
        retry_min_delay_seconds: 60,
        refetch: false,
    }
}

fn pdf_route(path: &'static str, body: Vec<u8>) -> Route {
    Route {
        path,
        status: "200 OK",
        content_type: "application/pdf",
        headers: vec![("ETag", "\"jobs-v1\"".to_owned())],
        body,
    }
}

fn server_error_route(path: &'static str) -> Route {
    Route {
        path,
        status: "500 Internal Server Error",
        content_type: "text/plain",
        headers: Vec::new(),
        body: b"upstream exploded".to_vec(),
    }
}

/// A transient server error backs off, refuses an early retry, and recovers once the
/// retry deadline passes — all visible in the durable job record.
#[tokio::test]
async fn retryable_failure_backs_off_then_recovers_when_due() {
    let pdf = small_pdf_bytes();
    let record_id = "2401.01001";
    let url = "/paper.pdf";

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);
    let policy = fast_policy();
    let ledger = JobLedger::new(&library);

    // One server, one stable URL: the first request fails, the second succeeds.
    let server = FixtureServer::start(vec![server_error_route(url), pdf_route(url, pdf.clone())]);
    let record = record_with_pdf("arxiv", record_id, format!("{}{}", server.base_url, url));

    // Run 1 (t=1000): the source fails with a server error.
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &policy,
        1_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.failed, 1);
    let entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Failed { detail, .. } = entry.outcome else {
        panic!("expected failure, got {:?}", entry.outcome)
    };
    assert!(detail.contains("retryable"), "{detail}");
    assert!(detail.contains("next attempt after"), "{detail}");

    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.state, JobState::FailedRetryable);
    assert_eq!(job.attempts, 1);
    assert_eq!(job.next_retry_unix_seconds, Some(1_000 + 60));

    // Run 2 (t=1010): the backoff deadline has not passed; the job must not re-fetch.
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &policy,
        1_010,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.skipped, 1);
    assert_eq!(
        server.request_count(),
        1,
        "backoff must prevent the request"
    );
    let entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Skipped { reason } = entry.outcome else {
        panic!("expected skip, got {:?}", entry.outcome)
    };
    assert!(reason.contains("retry not due until 1060"), "{reason}");

    // Run 3 (t=2000, past the deadline): the source recovered; the job acquires.
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &policy,
        2_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    assert_eq!(server.request_count(), 2);

    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Acquired);
    assert_eq!(job.attempts, 2, "the retry is the second scheduled attempt");
    assert!(job.acquired_sha256_hex.is_some());
    assert!(job.lease.is_none(), "terminal jobs hold no lease");
}

/// Attempts are bounded; exhaustion is a terminal failure that only `--refetch`
/// re-enters, and re-entry restarts the attempt series.
#[tokio::test]
async fn attempts_are_bounded_and_terminal_failure_requires_refetch() {
    let pdf = small_pdf_bytes();
    let record_id = "2401.01002";
    let url = "/paper.pdf";
    let policy = JobPolicy {
        max_attempts: 2,
        retry_min_delay_seconds: 60,
        refetch: false,
    };

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);
    let ledger = JobLedger::new(&library);

    // One server, one stable URL: two failures then a success on the third request.
    let server = FixtureServer::start(vec![
        server_error_route(url),
        server_error_route(url),
        pdf_route(url, pdf.clone()),
    ]);
    let record = record_with_pdf("arxiv", record_id, format!("{}{}", server.base_url, url));

    // Attempt 1 (t=1000): retryable failure.
    let mut entries = Vec::new();
    run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &policy,
        1_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.state, JobState::FailedRetryable);
    assert_eq!(job.attempts, 1);

    // Attempt 2 (t=5000, due): the budget is exhausted → terminal.
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &policy,
        5_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.failed, 1);
    let entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Failed { detail, .. } = entry.outcome else {
        panic!("expected failure, got {:?}", entry.outcome)
    };
    assert!(
        detail.contains("retry budget exhausted after 2 attempts"),
        "{detail}"
    );

    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.state, JobState::FailedPermanent);
    assert_eq!(job.next_retry_unix_seconds, None);

    // Run 3 (t=6000): terminal failure refuses to re-enter without --refetch.
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &policy,
        6_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.skipped, 1);
    assert_eq!(server.request_count(), 2);
    let entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Skipped { reason } = entry.outcome else {
        panic!("expected skip, got {:?}", entry.outcome)
    };
    assert!(reason.contains("--refetch"), "{reason}");

    // Run 4 (t=7000) with --refetch: the attempt series restarts and can succeed.
    let refetch_policy = JobPolicy {
        refetch: true,
        ..policy
    };
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &refetch_policy,
        7_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    assert_eq!(server.request_count(), 3);
    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Acquired);
    assert_eq!(
        job.attempts, 1,
        "explicit re-entry restarts the attempt series"
    );
}

/// A crash mid-flight leaves the job in an in-flight state with a lapsed lease; the
/// restart recovers it (without counting a new scheduled attempt) and finalizes.
#[tokio::test]
async fn restart_recovers_abandoned_in_flight_job_after_lease_expiry() {
    let pdf = small_pdf_bytes();
    let record_id = "2401.01003";
    let url = "/paper.pdf";

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);
    let ledger = JobLedger::new(&library);

    // Restart target: the record URL is fixed before planting the abandoned job so the
    // recovery path is exercised with an unchanged target.
    let server = FixtureServer::start(vec![pdf_route(url, pdf.clone())]);
    let record = record_with_pdf("arxiv", record_id, format!("{}{}", server.base_url, url));

    // Simulate the abandoned job: in-flight, one attempt made, lease long expired,
    // and a torn scratch payload left behind.
    let mut job = JobRecord::new(
        "arxiv",
        record_id,
        record.oa_pdf_url.as_deref().unwrap(),
        1_000,
    );
    job.state = JobState::Fetching;
    job.attempts = 1;
    job.lease = Some(paperengine::jobs::JobLease {
        owner: "acquire:pid:999:1000".into(),
        expires_unix_seconds: 1_500,
    });
    ledger.store(&job).unwrap();
    library.create().unwrap();
    fs::write(
        library.tmp_dir().join("arxiv-2401.01003.pdf"),
        b"%PDF-1.4 torn bytes from the crashed run",
    )
    .unwrap();

    // Restart at t=5000: the lease has lapsed, so recovery proceeds.
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &fast_policy(),
        5_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    assert_eq!(server.request_count(), 1);

    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Acquired);
    assert_eq!(
        job.attempts, 1,
        "restart recovery is not a new scheduled attempt"
    );
    // The stale scratch was cleared by the recovering run.
    assert!(library
        .tmp_dir()
        .read_dir()
        .map(|mut d| d.next().is_none())
        .unwrap());
}

/// A live lease held by another run excludes this one: no request is made.
#[tokio::test]
async fn live_lease_blocks_a_second_run() {
    let record_id = "2401.01004";
    let url = "/paper.pdf";

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);
    let ledger = JobLedger::new(&library);

    let target = format!("https://arxiv.example{url}");
    let mut job = JobRecord::new("arxiv", record_id, &target, 1_000);
    job.state = JobState::Fetching;
    job.attempts = 1;
    job.lease = Some(paperengine::jobs::JobLease {
        owner: "acquire:pid:999:1000".into(),
        expires_unix_seconds: 9_000,
    });
    ledger.store(&job).unwrap();

    let server = FixtureServer::start(vec![pdf_route(url, small_pdf_bytes())]);
    let record = record_with_pdf("arxiv", record_id, format!("{}{}", server.base_url, url));
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &fast_policy(),
        5_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.skipped, 1);
    assert_eq!(
        server.request_count(),
        0,
        "a live lease must block the request"
    );
    let entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Skipped { reason } = entry.outcome else {
        panic!("expected skip, got {:?}", entry.outcome)
    };
    assert!(reason.contains("live lease"), "{reason}");
    // The lease itself was not disturbed.
    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.lease.unwrap().owner, "acquire:pid:999:1000");
}

/// A new discovery observation with a different target URL re-opens a terminal job
/// without the operator flag, dedups against the shared object, and records the new
/// target.
#[tokio::test]
async fn changed_target_url_reopens_acquired_job() {
    let pdf = small_pdf_bytes();
    let record_id = "2401.01005";

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);
    let ledger = JobLedger::new(&library);
    let policy = fast_policy();

    // First target acquires.
    let first_server = FixtureServer::start(vec![pdf_route("/a.pdf", pdf.clone())]);
    let record = record_with_pdf(
        "arxiv",
        record_id,
        format!("{}/a.pdf", first_server.base_url),
    );
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &policy,
        1_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Acquired);

    // The same work re-observed at a different URL re-opens the job: same bytes, so
    // the object dedups, and the job records the new target.
    let second_server = FixtureServer::start(vec![pdf_route("/b.pdf", pdf.clone())]);
    let record = record_with_pdf(
        "arxiv",
        record_id,
        format!("{}/b.pdf", second_server.base_url),
    );
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &policy,
        2_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    assert_eq!(summary.deduplicated, 1);
    let entry = parse_entry(&entries[0]);
    assert!(matches!(
        entry.outcome,
        AcquisitionOutcome::Acquired {
            deduplicated: true,
            ..
        }
    ));

    let job = ledger.load("arxiv", record_id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Acquired);
    assert_eq!(job.target_url, format!("{}/b.pdf", second_server.base_url));
    assert_eq!(job.attempts, 1, "re-targeting restarts the attempt series");
    assert_eq!(library.list_objects().unwrap().len(), 1);
}

/// The ledger round-trips records atomically, summarizes counts, rejects unknown
/// schema versions, and ignores `.pending` siblings.
#[tokio::test]
async fn ledger_round_trips_and_reports_counts() {
    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let ledger = JobLedger::new(&library);

    assert!(ledger.load("arxiv", "missing").unwrap().is_none());

    let mut first = JobRecord::new("arxiv", "2401.02001", "https://a.example/1.pdf", 100);
    first.state = JobState::Acquired;
    first.acquired_sha256_hex = Some("ab".repeat(32));
    ledger.store(&first).unwrap();

    let mut second = JobRecord::new("openalex", "W2", "https://a.example/2.pdf", 100);
    second.state = JobState::FailedRetryable;
    second.attempts = 1;
    second.next_retry_unix_seconds = Some(160);
    ledger.store(&second).unwrap();

    let reloaded = ledger.load("arxiv", "2401.02001").unwrap().unwrap();
    assert_eq!(reloaded, first);
    assert_eq!(reloaded.schema_version, JOBS_SCHEMA_VERSION);

    let counts = ledger.counts_by_state().unwrap();
    assert_eq!(counts.get("acquired"), Some(&1));
    assert_eq!(counts.get("failed_retryable"), Some(&1));

    // An unsupported schema version is a clear, named error — never a silent skip.
    let mut future = first.clone();
    future.schema_version = JOBS_SCHEMA_VERSION + 1;
    let path = library.jobs_dir().join(format!(
        "{}.json",
        paperengine::library::sanitize_stem("arxiv-2401.02001")
    ));
    fs::write(&path, serde_json::to_vec_pretty(&future).unwrap()).unwrap();
    let error = ledger.load("arxiv", "2401.02001").unwrap_err();
    assert!(error.to_string().contains("schema version"), "{error}");

    // Restore the valid record, then verify a stale pending sibling is ignored.
    ledger.store(&first).unwrap();
    fs::write(library.jobs_dir().join("stale.json.pending"), b"{ torn").unwrap();
    assert_eq!(ledger.list().unwrap().len(), 2);
    // Exactly two .json files: no publication remnants.
    let json_count = fs::read_dir(library.jobs_dir())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .and_then(|e| e.to_str())
                == Some("json")
        })
        .count();
    assert_eq!(json_count, 2);
}
