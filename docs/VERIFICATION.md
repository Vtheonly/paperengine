# Verification Report — 2026-10-19

Environment: Linux x86-64, rustc 1.99.1 (5f94df478 2026-08-27), cargo 1.99.1, Ubuntu-class
container, 2026-10-19 (UTC). All commands below were executed as written; nothing was
simulated or mocked unless explicitly labeled.

## Static verification (final source commit 82362f4)

```bash
cargo fmt --all -- --check          # pass
cargo clippy --all-targets --all-features -- -D warnings   # pass
cargo test --all-features           # 109 passed, 0 failed, 7 ignored (live, opt-in)
cargo build --release               # pass
```

Test breakdown: 90 unit tests (lib), 3 downloader integration, 1 architecture contract,
5 PDF validation integration, 10 source-pipeline integration, 7 ignored live tests.

CI on the exact commit: run
[39233243396](https://github.com/Vtheonly/paperengine/actions/runs/39233243396)
conclusion **success** for `82362f4e4b349881118dd5b7176b621a8f86b33c`.

## Live source smoke tests (Layer 2) — 2026-10-19

Command:

```bash
PAPERENGINE_LIVE_TESTS=1 cargo test --test live_sources -- --ignored --nocapture
```

Result: **7 passed, 0 failed** (including one reported BLOCKED, see OpenAlex below).

| Test | Source | Observed result |
|---|---|---|
| `live_arxiv_discovery_returns_parseable_records_with_pdf_links` | arXiv | 3 records for phrase `"attention is all you need"`; first id `2604.21816v1`; every record carries an https PDF link and a publication year |
| `live_arxiv_end_to_end_pdf_acquisition` | arXiv | see Layer 3 below |
| `live_crossref_discovery_returns_metadata_without_pdf_links` | Crossref | 3 records, each with a normalized DOI, no OA PDF claims, landing page recorded |
| `live_crossref_doi_lookup_resolves_a_stable_doi` | Crossref | `10.1371/journal.pone.0000219` resolved to "High-Throughput Sequencing of Arabidopsis microRNAs…" |
| `live_unpaywall_resolves_an_oa_doi_with_a_direct_pdf` | Unpaywall | `is_oa: true`; direct PDF `https://journals.plos.org/plosone/article/file?id=10.1371/journal.pone.0000219&type=printable` (cc-by) |
| `live_openalex_doi_lookup_resolves_a_work` | OpenAlex | `10.7717/peerj.4375` → W2741809807 "The state of OA…" |
| `live_openalex_discovery_or_blocked_by_free_tier_budget` | OpenAlex | HTTP 429: free-tier daily budget shared per IP exhausted; recorded as **BLOCKED** (source availability, not a code failure); optional API key is the documented remedy |

## Real end-to-end PDF acquisition (Layer 3) — 2026-10-19

### Via the live test (production pipeline, temp library)

```bash
PAPERENGINE_LIVE_TESTS=1 PAPERENGINE_LIVE_ACQUIRE=1 \
  cargo test --test live_sources live_arxiv_end_to_end_pdf_acquisition -- --ignored --nocapture
```

Evidence line emitted by the test:

```text
ARXIV ACQUISITION EVIDENCE: id=2604.21816v1 url=https://arxiv.org/pdf/2604.21816v1
bytes=447412 sha256=f2441d3de469aa1e7c04b7e2412361cdc0b429b7ac5fbfd05196a5909bd94aeb
pages=21 object=…/library/objects/f2/f2441d3de469aa1e7c04b7e2412361cdc0b429b7ac5fbfd05196a5909bd94aeb.pdf
```

The finalized object was re-opened by the structural parser (lopdf) and the provenance
manifest was verified to identify source, work id, retrieval time, and both checksums.

### Via the documented CLI workflow

```bash
paperengine discover --source arxiv --query "attention is all you need" \
  --max-records 5 --email <contact> --out discovery.jsonl
paperengine acquire --from discovery.jsonl --library ./research-library --max-downloads 2
paperengine inspect --library ./research-library
```

Observed (full JSONL in `reports/`, excerpted):

| Work | URL | Bytes | SHA-256 | Pages | Outcome |
|---|---|---|---|---|---|
| arXiv `2604.21816v1` "Tool Attention Is All You Need…" | `https://arxiv.org/pdf/2604.21816v1` | 447,412 | `f2441d3de469…bd94aeb` | 21 | acquired |
| arXiv `1706.03762v7` "Attention Is All You Need" | `https://arxiv.org/pdf/1706.03762v7` | 2,215,244 | `bdfaa68d8984…2df697` | 15 | acquired |
| arXiv `2104.04692v3`, `2501.05730v1`, `2510.19861v1` | — | — | — | — | skipped: run budget exhausted (2-download cap) |

Independent check performed outside the engine:

```bash
sha256sum research-library/objects/bd/bdfaa68d…pdf
# bdfaa68d8984f0dc02beaca527b76f207d99b666d31d1da728ee0728182df697  (matches)
```

Both objects are content-addressed under `objects/<sha256[0..2]>/<sha256>.pdf`, carry
provenance manifests under `manifests/`, and the run report is preserved under `reports/`.

## Negative-path verification (offline, fixture-driven)

Covered by `tests/source_pipeline_tests.rs` and `tests/pdf_validation_tests.rs`:

- HTML masquerading as PDF is rejected by the downloader's signature gate; no partial
  files survive; the run records a typed failure and never finalizes an object.
- Truncated PDFs (missing `%%EOF`), corrupt object graphs, encrypted documents, checksum
  mismatches, and changed-during-hash files are quarantined with typed reasons and
  redacted provenance manifests.
- `http://` URLs are policy-rejected unless explicitly allowed; URLs with embedded
  credentials are always rejected; URLs without a host are rejected.
- Download and byte budgets bound the run; records beyond the budget are reported as
  skipped with an explicit reason, and exactly the permitted number of transfers hit the
  wire (asserted against a request-counting fixture server).
- Re-acquiring an identical object deduplicates to the existing content-addressed file
  and does not create a second manifest.

## Clean-checkout deployment verification (Colab-equivalent) — 2026-10-19

`scripts/colab-equivalent-run.sh` replays the Colab notebook's shell cells (toolchain
setup → clone at pinned revision `82362f4e4b349881118dd5b7176b621a8f86b33c` → release
build → fmt/clippy/offline tests → configuration → live discovery → Unpaywall lookup →
bounded real acquisition → library inspection) in a fresh temporary directory on a plain
Linux box. Observed result: **SUCCESS**, reproducing the identical evidence values above:

- discovery: 5 arXiv records for `"attention is all you need"`;
- acquisition: `2604.21816v1` (447,412 bytes, 21 pages, SHA-256 `f2441d3d…bd94aeb`) and
  `1706.03762v7` "Attention Is All You Need" (2,215,244 bytes, 15 pages, SHA-256
  `bdfaa68d…2df697`) — byte-identical hashes to the earlier run, confirming
  reproducibility;
- 3 further records skipped by the 2-download budget with explicit reasons;
- Unpaywall resolved `10.1371/journal.pone.0000219` to the PLOS ONE direct PDF.

One earlier attempt of this script hit a transient arXiv connection timeout; the script
now retries network-touching steps exactly once after a pause, which is documented in
the script. The Colab-specific Google Drive mount (notebook cell 8) was **not** executed
here — it requires a real Colab runtime and remains outstanding live verification.

## Known limitations

- OpenAlex search could not be live-verified from this network (shared free-tier budget
  exhausted; HTTP 429). DOI lookup is live-verified; an API key is the documented
  unblocking step.
- Europe PMC OA PDF acquisition is blocked from datacenter networks: the europepmc.org
  website refuses the render URL with HTTP 403 (bot protection). The API at ebi.ac.uk is
  live-verified for discovery and DOI lookup. Acquisition must be verified from an
  unrestricted network or Colab; no bypass exists by policy.
- Unpaywall-resolved PDF URLs were verified to resolve at the metadata level (L2);
  a Layer-3 download through a Unpaywall-resolved link was not separately recorded in
  this run — the acquisition path itself is source-agnostic and already exercised
  end-to-end via arXiv.
- The acquisition pipeline is sequential (one download at a time) by design; parallel
  acquisition with per-source quotas is future work (#25).
- No performance claims are made in this report; benchmarks are tracked by issue #36.

---

# Verification Report — 2026-10-19 (second run, after the #12 adapter wave)

Environment: Linux x86-64 (Debian 13 trixie container, 2 vCPU), rustc 1.99.1
(58cf8f9c3 2026-10-13), cargo 1.99.1. All commands below were executed as written.

## Static verification (source commit 4d81f0e)

```bash
cargo fmt --all -- --check          # pass
cargo clippy --all-targets --all-features -- -D warnings   # pass
cargo test --all-features           # 144 passed, 0 failed, 22 ignored (live, opt-in)
cargo build --release               # pass
```

CI on the exact commit: see the run list at
https://github.com/Vtheonly/paperengine/actions?query=branch:main — the row for
head `4d81f0e` must be green before this report is trusted.

## Live suite (Layers 2 and 3) — 22 tests, single-threaded

```bash
PAPERENGINE_LIVE_TESTS=1 PAPERENGINE_LIVE_ACQUIRE=1 \
  cargo test --test live_sources -- --ignored --nocapture --test-threads=1
```

Result: **22 passed, 0 failed** in ~30 s. Eight real PDFs were acquired through the
production pipeline (one small OA document per source), each structurally parsed,
content-addressed, and provenance-manifested:

| Source | Work | Bytes | Pages | SHA-256 |
|---|---|---|---|---|
| arXiv | `2604.21816v1` | 447,412 | 21 | `f2441d3d…95a9bd94aeb` |
| OpenAlex | W1997413633 (PLOS ONE `10.1371/journal.pone.0000219`) | 703,571 | 14 | `4f111c22…1edc22006` |
| Unpaywall | `10.1371/journal.pone.0000219` | 703,571 | 14 | `4f111c22…1edc22006` |
| DOAJ | JIS `10.48550/…` record `00457175…` | 1,457,506 | 16 | `ccdda175…669947fed` |
| Zenodo | record `18289747` | 86,360 | 1 | `365ea2d4…32d5e1acb` |
| OSF | MetaArXiv `pybrz_v1` | 408,201 | 13 | `5ca36851…0333e3c2de7` |

Cross-checks recorded for the first time in this run:

- OpenAlex and Unpaywall independently resolved the same PLOS ONE work to
  **byte-identical payloads** (same sha256), cross-validating both adapters'
  link resolution.
- A well-formed but unregistered DOI (`10.1038/paperengine-probe-doi-not-registered`)
  surfaced as `Ok(None)` — never an error, never an invented record
  (`live_crossref_doi_lookup_reports_a_missing_doi_as_none`).
- The acquisition helper now asserts that the provenance manifest's sha256 and
  byte length agree with the acquisition report before any evidence line is printed.
- Europe PMC and PubMed Central were verified again as metadata-only (no PDF
  claim); OpenAlex search remained blocked by the shared free-tier budget (HTTP
  429) and is recorded as BLOCKED, not as a failure.

Full per-source details and last-live-result entries are maintained in
`docs/SOURCE_COVERAGE.md`.

## Third run — OAI-PMH family added (final source commit ea1e827)

Command set (all executed as written on 2026-10-19):

```bash
PAPERENGINE_LIVE_TESTS=1 PAPERENGINE_LIVE_ACQUIRE=1 \
  cargo test --test live_sources -- --ignored --nocapture   # 25 passed, 0 failed
cargo fmt --all -- --check                                  # pass
cargo clippy --all-targets --all-features -- -D warnings    # pass
cargo test --all-features                                   # 134 unit + 19 integration, 0 failed
cargo build --release                                       # pass
```

New in this run (issue #13, generic OAI-PMH 2.0 harvester):

| Test | Source | Observed result |
|---|---|---|
| `live_zenodo_oaipmh_harvests_records_with_resumption_tokens` | zenodo-oaipmh (`https://zenodo.org/oai2d`) | 50-record window harvest (`from=2026-10-19`), paper filter 50 → 18, resumption token followed; every record carries a DOI and no PDF claim |
| `live_doaj_oaipmh_journal_records_are_filtered_by_default` | doaj-oaipmh (`https://doaj.org/oai`) | 274 journal records from `from=2026-09-01`, **all excluded** by the default paper-only policy (containers are not papers); permissive mode parses them with landing pages |
| `live_pmc_oaipmh_harvests_article_records_with_identifiers` | pmc-oaipmh (`https://pmc.ncbi.nlm.nih.gov/api/oai/v1/mh/`) | 10 article records from `from=2026-10-10`, each with a DOI and a PMCID extracted from the PMC article URL; resumption token followed |

Protocol properties verified in this run: `noRecordsMatch` is returned as an
empty page (not an error), a document without an OAI-PMH envelope is rejected as
a malformed response, deleted headers are skipped, and Dublin Core never yields
a PDF claim. The legacy PMC OAI path (`ncbi.nlm.nih.gov/pmc/oai/oai.cgi`)
301-redirects to the modern endpoint, which the adapter uses directly.

Deployment-path verification repeated at this revision:

- `scripts/setup-linux.sh` — SUCCESS (toolchain check, release build, offline
  tests, smoke `rank` run, printed next steps).
- `scripts/colab-equivalent-run.sh` at pinned revision — SUCCESS from a clean
  clone: build + offline verification, configuration, self-test across all
  twelve registered sources, live arXiv discovery, Unpaywall resolution, real
  acquisition of two PDFs (447,412 B / 21 pages and 2,215,244 B / 15 pages —
  the original "Attention Is All You Need"), library inspection, and report.

## Fourth run — Europe PMC unification (final source commit pending push)

Two independent Europe PMC implementations were reconciled (see the unification note
below). The unified tree was re-verified end to end on 2026-10-19; the numbers below
were re-measured on the final merged tree before the push.

```bash
cargo fmt --all -- --check          # pass
cargo clippy --all-targets --all-features -- -D warnings   # pass
cargo test --all-features           # 161 passed, 0 failed, 27 ignored (live, opt-in)
cargo build --release               # pass
PAPERENGINE_LIVE_TESTS=1 PAPERENGINE_LIVE_ACQUIRE=1 \
  cargo test --test live_sources -- --ignored --nocapture --test-threads=1
```

Result on the merged tree: **27 passed, 0 failed**, including six real PDF
acquisitions (arXiv 447,412 B; DOAJ 1,457,506 B; OpenAlex-resolved PLOS 703,571 B;
OSF 408,201 B; Unpaywall-resolved PLOS 703,571 B; Zenodo 86,360 B), three OAI-PMH
harvests with resumption tokens (DOAJ journals, PMC articles, Zenodo records), and
the two honest BLOCKED states (Europe PMC render 403, OpenAlex search 429).

New Europe PMC evidence in the unified tree:

| Test | Observed result |
|---|---|
| `live_europepmc_discovery_returns_parseable_records_with_oa_pdf_links` | 5 records for `"high-throughput sequencing" AND OPEN_ACCESS:Y`; 4 with OA render links; first: `MED/42796766` "A Validation Study of a High-Throughput Sequencing Workflow for the Detection of Grapevine Viruses and Viroids." One record (`PMC/PMC13638646`, in-process manuscript) legitimately reports no `pubYear` and no PDF — parsed as `None`, not fabricated. |
| `live_europepmc_doi_lookup_resolves_a_stable_doi` | `10.1371/journal.pone.0000219` → `MED/17299599`, PMCID `PMC1790633`, license `cc by`, OA render `https://europepmc.org/articles/PMC1790633?pdf=render` |
| `live_europepmc_doi_lookup_resolves_the_deseq2_paper` | `10.1186/s13059-014-0550-8` (DESeq2) → `MED/25516281`, PMCID `PMC4302049`, OA render `https://europepmc.org/articles/PMC4302049?pdf=render` |
| `live_europepmc_end_to_end_pdf_acquisition_or_blocked` | **BLOCKED (honest)**: `https://europepmc.org/articles/PMC13610249?pdf=render` refused with HTTP 403 by the europepmc.org website from this datacenter network; recorded as BLOCKED evidence, never retried past refusal or evaded. The metadata API (ebi.ac.uk) is fully live-verified. Run from an unrestricted network or Colab to complete L3. |

Regression evidence from the same command on the merged tree: arXiv discovery and
L3 acquisition reproduced byte-identically (447,412 B, 21 pages, SHA-256
`f2441d3de469…bd94aeb`); Crossref, DOAJ, Zenodo, OSF, and Unpaywall tests all
passed, with the DOAJ/OSF/Zenodo real acquisitions reproducing as well; PMC and
OpenAlex DOI lookups passed; OpenAlex search remained 429-BLOCKED on the shared
free-tier budget.

### Europe PMC unification note

Two independent implementations of the Europe PMC adapter were produced on
2026-10-19: one metadata-only (its probe of the undocumented `ptpmcrender.fcgi` render
form had answered 403) and one recording the API's own `fullTextUrlList` OA render
links (`https://europepmc.org/articles/{pmcid}?pdf=render`, returned by the documented
`resultType=core` search). They were unified on the second semantics — the source
itself reports these URLs for OA records — with the conservative guarantees kept:
lite responses without `fullTextUrlList` never fabricate a link, subscription entries
never become PDFs, and the datacenter 403 refusal is recorded as BLOCKED evidence
rather than retried or evaded. The identifier kind is unified as `pmcid`.

---

# Verification Report — 2026-10-19 (third run: deployment replays and a GitHub-runner live pass)

## Deployment paths (issue #38)

- `scripts/setup-linux.sh`: executed end-to-end on this machine (Debian 13,
  rustc 1.99.1). Tool checks, release build, fmt/clippy/offline tests, the
  `rank` smoke test, and the printed next-step commands all behaved as
  documented.
- `scripts/colab-equivalent-run.sh 2a60202`: executed on a clean checkout in a
  fresh temp workspace. Full replay of notebook cells 1–7: clone at the pinned
  revision, build, offline verification, config + self-test, live arXiv
  discovery (5 records), Unpaywall DOI resolution, real acquisition of two
  arXiv PDFs (447,412 + 2,215,244 bytes), budget exhaustion correctly skipping
  the remaining records with a recovery note, `inspect` output, and the run
  report. Library, provenance manifests, and report JSONL all persisted.

## Live-source workflow on GitHub infrastructure

The opt-in `live-sources` workflow was dispatched manually with real
acquisition enabled. Two runs on `main`:

- Run [39242803787](https://github.com/Vtheonly/paperengine/actions/runs/39242803787)
  (head 9a7709b) **failed**: from the runner's fresh IP, OpenAlex discovery
  succeeded, but `page_size=2` produced a page whose raw hits were both
  non-paper types; the post-filter page was empty and the happy-path assertion
  was unreachable. This path had only ever been exercised as the 429 BLOCKED
  branch. Fixed in 47eea43 by using the documented default page size (25).
- Run [39243606887](https://github.com/Vtheonly/paperengine/actions/runs/39243606887)
  (head 23352af) **passed**: 27/27 live tests, including
  `OpenAlex discovery OK: 21 records, first W1572277998` and the EPMC render
  link acquisition honestly reported BLOCKED (403 bot protection) — with the
  documented remedy of running from an unrestricted network such as Colab.

## Full local verification at head 23352af (merged tree)

```
cargo fmt --all -- --check          # pass
cargo clippy --all-targets --all-features -- -D warnings   # pass
cargo test --all-features           # 161 passed, 0 failed, 27 ignored (live, opt-in)
PAPERENGINE_LIVE_TESTS=1 PAPERENGINE_LIVE_ACQUIRE=1 \
  cargo test --test live_sources -- --ignored --nocapture --test-threads=1 \
                                    # 27 passed, 0 failed
cargo build --release               # pass
```

CI on the exact commit: [39243321210](https://github.com/Vtheonly/paperengine/actions/runs/39243321210)
success for 23352af.
## Storage finalization crash boundaries (issue #29) — 2026-10-19

Environment: Linux x86-64 (Debian 13), rustc 1.99.1, 2 vCPU. Finalization crosses three
boundaries: (1) payload scratch under `tmp/` (fsynced by the downloader before
`finalize_object` runs), (2) the atomic object rename into
`objects/<sha256[0..2]>/<sha256>.pdf` plus parent-directory sync, and (3) provenance
manifest publication. In this change, boundary (3) was hardened from a direct
`fs::write` to the pending-file → fsync → rename → directory-sync pattern (the same
idiom the quarantine path already used), and a crash between boundaries (2) and (3)
was made explicitly visible.

Commands (all executed, all passing):

```bash
cargo fmt --all -- --check                                  # pass
cargo clippy --all-targets --all-features -- -D warnings    # pass
cargo test --all-features                                   # 144 unit + 34 integration, 0 failed, 27 live ignored (final merged tree including the concurrent #31 extraction slice)
cargo build --release                                       # pass
cargo test --test storage_finalization_tests -- --nocapture # 6 passed, 0 failed
```

Boundary evidence in `tests/storage_finalization_tests.rs` (offline, local fixture
server; each test constructs the exact on-disk state a crash at that boundary leaves):

| Boundary simulated | Test | Observed result |
|---|---|---|
| Crash during transfer (stale scratch under `tmp/`, no object) | `crash_during_transfer_leaves_only_scratch_and_next_run_recovers` | next run clears the stale scratch, transfers fresh bytes, finalizes exactly one object matching the fixture sha256; `tmp/` empty afterwards; no unattributed objects |
| Crash between object rename and manifest write (object present, manifest absent) | `crash_between_object_rename_and_manifest_is_visible_and_recoverable` | the state is reported by `LibraryLayout::unattributed_objects` and by `paperengine inspect`; re-acquiring the record deduplicates against the existing object (summary `deduplicated=1`), rewrites the missing manifest, leaves object bytes unchanged, and clears the orphan report |
| Crash during manifest publication itself | `manifest_publication_is_atomic_and_pending_leftovers_are_ignored` | publication lands atomically (no `.pending` remnant, manifest parses); a stale `.pending` sibling is ignored by readers and cleared by the next publication; `manifests/` contains exactly one `.json` |
| Duplicate content across distinct works | `duplicate_content_across_distinct_works_shares_one_object` | two records with byte-identical payloads share one content-addressed object; two provenance manifests; second outcome reports `deduplicated=true`; summary `{acquired: 2, deduplicated: 1}`; no orphans |
| Finalized object reopenability | `finalized_object_reopens_with_real_parser_and_matches_its_address` | stored bytes hash to their content address; the production validator accepts the object from its final path; lopdf independently re-opens it |
| Externally corrupted manifest (not producible by the production path anymore) | `externally_corrupted_manifest_is_surfaced_with_its_path` | `list_manifests` fails with an error naming the offending file instead of silently skipping it |

CLI smoke (executed): a library containing an object without a manifest reports
`unattributed objects: 1` in both text and `--json` output of `paperengine inspect`.

## Durable job ledger and state machine (issue #6) — 2026-10-19

Environment: Linux x86-64 (Debian 13), rustc 1.99.1, 2 vCPU. Every acquisition job now has
a durable record under `library/jobs/<source>-<record>.json`, published atomically
(pending → fsync → rename → dir sync, the same pattern as provenance manifests). The
state machine is pure (`src/jobs.rs`); the ledger is outer I/O; the computational kernel
is untouched.

Commands (all executed, all passing):

```bash
cargo fmt --all -- --check                                  # pass
cargo clippy --all-targets --all-features -- -D warnings    # pass
cargo test --all-features                                   # 158 unit + 40 integration, 0 failed, 27 live ignored
cargo build --release                                       # pass
cargo test --test job_ledger_tests -- --nocapture           # 6 passed, 0 failed
```

State-machine evidence (unit tests in `src/jobs.rs`): the full legal lifecycle
(discovered → fetching → validating → storing → acquired → extracted → indexed, the last
two reserved for the extraction/indexing pipelines), failure paths from every in-flight
state, quarantine only from validation, due-retry and budget-exhaustion transitions,
invalid transitions rejected (stage skipping, backwards moves, terminal forward moves,
terminal self-transitions), terminal re-entry only via explicit consent or a changed
target URL, in-flight recovery requiring a lapsed lease, in-flight self-transition as a
lease refresh, attempt counting/reset semantics, exponential backoff with cap, error
classification of every `DownloadError` variant (429 → rate_limited, 404/410 →
not_found, 401/403 → auth_restricted, 5xx/transport/timeout → transient, size bounds →
permanent, invalid signature → corrupt), and gate behavior for terminal jobs, backoff
deadlines, live leases, and changed targets.

Restart/retry evidence (`tests/job_ledger_tests.rs`, offline, production pipeline with an
injected clock):

| Scenario | Test | Observed result |
|---|---|---|
| Transient failure → backoff → due retry recovers | `retryable_failure_backs_off_then_recovers_when_due` | run 1 (500) → `failed_retryable`, attempts=1, next_retry=+60s; immediate re-run skipped with `retry not due until 1060` and **zero** requests; run at t=2000 acquired, attempts=2, terminal `acquired`, no lease |
| Bounded attempts and explicit re-entry | `attempts_are_bounded_and_terminal_failure_requires_refetch` | with max_attempts=2: attempt 2 fails → `failed_permanent` with `retry budget exhausted after 2 attempts`; next run skips naming `--refetch` with zero requests; `--refetch` run restarts the attempt series (attempts=1) and acquires |
| Crash mid-flight, restart after lease expiry | `restart_recovers_abandoned_in_flight_job_after_lease_expiry` | planted in-flight job (lapsed lease, attempts=1) + torn scratch: restart acquires, clears the scratch, keeps attempts=1 (restart recovery is not a new scheduled attempt) |
| Live lease exclusion | `live_lease_blocks_a_second_run` | a job with a lease expiring in the future blocks a second run: skip names the owner, zero requests, lease undisturbed |
| Changed target URL re-opens a terminal job | `changed_target_url_reopens_acquired_job` | an `acquired` job re-observed at a new URL re-opens without the flag, dedups against the shared object, records the new target with a restarted attempt series |
| Ledger durability | `ledger_round_trips_and_reports_counts` | records round-trip byte-exact; counts-by-state; unsupported schema versions fail with a named error; stale `.pending` siblings ignored; exactly N `.json` files after publications |

CLI smoke (executed, release binary): a record whose target genuinely returned HTTP 404
(arxiv.org, single request) classified `not_found`/permanent and the next run refused
re-entry without `--refetch`; a connection-refused target classified `transient` with
`next attempt after <now+3600>` (configured floor); an immediate re-run skipped with
`retry not due`; `--refetch` overrode the backoff, consumed the second attempt, then
exhausted the budget to terminal failure; a further `--refetch` restarted the series.
`paperengine inspect` prints the job ledger counts by state.

---

---

# Independent run — parallel #13 slice (pre-merge tree), 2026-10-19

A second agent implemented the #13 OAI-PMH slice in parallel (the `arxiv-oaipmh`
instance with the `arXiv` raw format, the `harvest_page` protocol view,
`list_sets`, until/set support, and the CLI `--from-date/--until-date/--set`
flags). Both trees were live-verified independently before being merged into a
single adapter; the numbers below describe that pre-merge tree.

Environment: Linux x86-64 (container, 2 vCPU), rustc 1.99.1 (58cf8f9c3
2026-10-13), cargo 1.99.1. All commands below were executed as written.

## Static verification (source tree after the #13 slice, pre-push)

```bash
cargo fmt --all -- --check          # pass
cargo clippy --all-targets --all-features -- -D warnings   # pass
cargo test --all-features           # 164 passed, 0 failed, 25 ignored (live, opt-in)
cargo build --release               # pass
```

Test breakdown: 144 unit tests (lib, 19 of them new for `src/sources/oaipmh.rs`),
3 downloader integration, 1 architecture contract, 5 PDF validation, 11
source-pipeline integration (1 new: `oai_pmh_harvest_paginates_with_resumption_tokens_over_http`),
25 ignored live tests (3 new OAI-PMH).

## Live OAI-PMH verification (Layer 2) — 2026-10-19

Command (subset first, then the full suite):

```bash
PAPERENGINE_LIVE_TESTS=1 cargo test --test live_sources -- --ignored --nocapture --test-threads=1 oai
PAPERENGINE_LIVE_TESTS=1 PAPERENGINE_LIVE_ACQUIRE=1 \
  cargo test --test live_sources -- --ignored --nocapture --test-threads=1
```

Result: **25/25 passed in 50.7 s**. The three new OAI-PMH tests, against the real
endpoints, observed:

| Test | Source | Observed result |
|---|---|---|
| `live_doaj_oai_harvests_journal_records_with_deletions` | DOAJ OAI | `from=2025-01-01` page 1: **300 records, 26 deleted**, `completeListSize=37098`; resumption-token round-trip returned a disjoint page 2 (300 records, 7 deleted); work-type filter mapped all records to **0 work records** (journal registry — the honest result) |
| `live_pmc_oai_harvests_oa_article_metadata` | PMC OAI | `from=2026-10-01`: **10 records** (`dc:type=Text`); first `oai:pubmedcentral.nih.gov:6718227` with DOI `10.1002/14651858.cd013413`, PMCID `PMC6718227`, landing `https://pmc.ncbi.nlm.nih.gov/articles/PMC6718227/`; token round-trip returned a disjoint second page |
| `live_arxiv_oai_harvests_preprints_and_follows_resumption` | arXiv OAI | `from=2026-10-15 until=2026-10-16`: **1300-record page**; first `oai:arXiv.org:1509.00958`; the resumption token (literal value itself URL-encoded) round-tripped through our percent-encoding and returned a disjoint second page of 1300 records |

The full-suite run also re-verified all six real PDF acquisitions
(byte-identical SHA-256 values to the earlier runs: `f2441d3d…` arXiv,
`4f111c22…` OpenAlex + Unpaywall, `ccdda175…` DOAJ, `365ea2d4…` Zenodo,
`5ca36851…` OSF). OpenAlex search remained BLOCKED (HTTP 429, shared free-tier
budget) and is recorded as BLOCKED, not as a failure.

## CLI workflow verification (real binary) — 2026-10-19

Executed with `./target/release/paperengine`:

```bash
paperengine discover --source pmc-oai --from-date 2026-10-01 --max-records 5 --out /tmp/oai-discovery.jsonl
paperengine acquire --from /tmp/oai-discovery.jsonl --library /tmp/oai-library --max-downloads 2
```

Observed: 5 records written (first `oai:pubmedcentral.nih.gov:6718227`, DOI
`10.1002/14651858.cd013413`, PMCID `PMC6718227`, landing page recorded,
`oa_pdf_url: null`); the acquire run examined 5 records and skipped all 5 with
the typed reason "no direct OA PDF URL reported by the source" — no download was
attempted and none was fabricated. Error paths verified with the same binary: a
non-empty `--query` on a harvest source is refused with a typed configuration
error; `--from-date 2026/10/01` is refused with the expected-format message;
search sources still require `--query`.

## Offline OAI-PMH coverage (Layer 1)

Fixtures are recorded real responses (`tests/fixtures/doaj_oai_list_records.xml`
with one live journal record, one deleted tombstone, and the page's resumption
token; `doaj_oai_no_records.xml` — a complete real `noRecordsMatch` response;
`pmc_oai_list_records.xml`, `arxiv_oai_list_records.xml`, `doaj_oai_list_sets.xml`).
Unit tests cover: deleted-record parsing/counting/exclusion, `noRecordsMatch` →
empty page, other OAI errors → typed `MalformedResponse`, responses with neither
container nor error → typed rejection, malformed XML, token-exclusivity and
selective-harvest URL building, date-shape validation, PMCID extraction from PMC
article URLs, work-type policy behavior (DOAJ journals excluded under the
default policy, admitted with a widened policy), and the arXiv format's
`<id>`/`<license>`/`<journal-ref>` fields.

## Known limitations

- OAI-PMH instances are metadata-only by format; no Layer 3 acquisition exists
  or is claimed for them.
- DOAJ's OAI interface disseminates journal records only (verified live across
  two pages, 600/600 records); its harvests map to zero work records under the
  default paper-like policy.
- arXiv OAI pages are large (~1300 records, ~4 MB per day-window); live tests
  bound them with tight `from`/`until` windows and record budgets.
- Zenodo's OAI-PMH endpoints answered 404 from this network on 2026-10-19
  (probed `oai2`, `/oai2/`, `/api/oai2`, `/oai/`, `/oai-pmh`); recorded as a
  blocker — the REST adapter remains Zenodo's supported route.
- No performance claims are made in this report; benchmarks are tracked by #36.

---

# Verification Report — 2026-10-19 (fifth run: merged #13 tree)

Two parallel #13 implementations were reconciled into a single adapter (the
independent run above and the `ea1e827` tree): the merged adapter keeps the
`-oaipmh` instance family (`zenodo-oaipmh`, `doaj-oaipmh`, `pmc-oaipmh`) with
their live-verified paper-only dc:type policy and identifier mapping, and adds
the `arxiv-oaipmh` instance harvesting the `arXiv` raw format, the
`harvest_page` protocol view (records, deleted counts, completeListSize), and
`list_sets` with bounded continuation. Both harvest-window input forms are
supported: explicit `from`/`until`/`set` fields, or an ISO date in the query
text; a non-date non-empty query text is refused with a typed error. The
IdentifierKind variant for PMC accessions is `Pmcid` across the tree.

Environment: Linux x86-64 (container, 2 vCPU), rustc 1.99.1 (58cf8f9c
2026-10-13). All commands below were executed as written on the merged tree.

```bash
cargo fmt --all -- --check          # pass
cargo clippy --all-targets --all-features -- -D warnings   # pass
cargo test --all-features           # 193 passed, 0 failed, 29 ignored (live, opt-in)
cargo build --release               # pass
```

Test breakdown: 158 unit tests (lib, 25 of them for `src/sources/oaipmh.rs`
covering both fixture sets), 3 downloader, 1 architecture contract, 7 PDF
validation, 13 source-pipeline integration (including
`oai_pmh_harvest_paginates_with_resumption_tokens_over_http`), 6 storage
finalization, 5 extraction; 29 ignored live tests (5 OAI-PMH).

## Live suite on the merged tree (Layers 2 and 3)

```bash
PAPERENGINE_LIVE_TESTS=1 PAPERENGINE_LIVE_ACQUIRE=1 \
  cargo test --test live_sources -- --ignored --nocapture --test-threads=1
```

Result: **29/29 passed in 55.8 s**, including five OAI-PMH harvests with
resumption-token round-trips, six byte-identical real PDF acquisitions, and
two honest BLOCKED states (Europe PMC render 403 on this datacenter network;
OpenAlex search 429 free-tier). New OAI evidence observed in this run:

| Test | Source | Observed result |
|---|---|---|
| `live_zenodo_oaipmh_harvests_records_with_resumption_tokens` | zenodo-oaipmh (`https://zenodo.org/oai2d`) | 50-record window harvest, every record with a DOI, token follows |
| `live_doaj_oaipmh_journal_records_are_filtered_by_default` | doaj-oaipmh | 274 journal records, all excluded by the paper-only policy (containers are not papers) |
| `live_doaj_oaipmh_deleted_tombstones_are_counted_and_excluded` | doaj-oaipmh | page 1: 300 records / 26 deleted; token round-trip page 2: 300 / 7; `completeListSize=37098`; zero work records after the paper-only filter |
| `live_pmc_oaipmh_harvests_article_records_with_identifiers` | pmc-oaipmh | 10 article records, first `oai:pubmedcentral.nih.gov:6718227` with DOI and PMCID |
| `live_arxiv_oaipmh_harvests_preprints_and_follows_resumption` | arxiv-oaipmh | 1300-record page from the 2026-10-15..16 window; the token (literal value itself URL-encoded) round-tripped and returned a disjoint second page of 1300 records |

## CLI workflow verification (real binary, merged tree)

```bash
paperengine discover --source pmc-oaipmh --from-date 2026-10-01 --max-records 3 --out oa1.jsonl
paperengine discover --source pmc-oaipmh --query 2026-10-01 --max-records 3 --out oa2.jsonl
paperengine discover --source arxiv-oaipmh --from-date 2026-10-15 --until-date 2026-10-16 --max-records 3 --out oa3.jsonl
```

Observed: both harvest-window forms produce identical records (first
`oai:pubmedcentral.nih.gov:6718227`, DOI + PMCID identifiers, landing via the
DOI resolver fallback, `oa_pdf_url: null`); `arxiv-oaipmh` returns preprints
with normalized arXiv ids and no links; a non-date `--query` value on a harvest
source is refused with a typed configuration error; `paperengine self-test`
reports all four OAI-PMH adapters; search sources still require `--query`.

## Merge-correctness notes

- A CLI regression introduced by the merge (harvest detection keyed on the
  pre-merge `-oai` suffix while the merged ids end in `-oaipmh`) was caught by
  this verification pass and fixed; the fixed binary is what the numbers above
  were measured on.
- The PMC landing-page rule follows the merged mapping: identifier values are
  consumed as identifiers (DOI, PMCID) and the landing page falls back through
  relations to the DOI resolver; `oa_pdf_url` is never set.

## Known limitations

- OAI-PMH instances are metadata-only by format; no Layer 3 acquisition exists
  or is claimed for them.
- DOAJ's OAI interface disseminates journal records only; its harvests map to
  zero work records under the paper-only policy.
- arXiv OAI pages are large (~1300 records, ~4 MB per day-window); live tests
  bound them with tight windows and record budgets.
- Zenodo's OAI-PMH interface lives at `oai2d`; the other probed paths 404 —
  recorded in the coverage registry.
- Europe PMC's render URL refuses this datacenter network (403) and OpenAlex
  search remains 429-blocked on the shared free-tier budget; both are recorded
  as BLOCKED, never bypassed.
- No performance claims are made in this report; benchmarks are tracked by #36.

## Fifth run — extraction, indexing, and local search (#31, #32, #33)

Environment: Linux x86-64 (Debian trixie container, 2 cores), rustc 1.99.1
(58cf8f9c3 2026-10-13), cargo 1.99.1, 2026-10-19 (UTC).

### Static verification (source commit 5a39a31 for #31; #32/#33 in the follow-up commit)

```
cargo fmt --all -- --check          # pass
cargo clippy --all-targets --all-features -- -D warnings   # pass
cargo test --all-features           # 144+15 passed, 0 failed, 27 ignored (live, opt-in)
                                     # includes 7 extraction + 8 indexing acceptance tests
cargo build --release               # pass
```

### Real end-to-end extraction evidence (CLI, production pipeline) — 2026-10-19

Library: the real arXiv object acquired earlier in this report
(sha256 `f2441d3de469aa1e7c04b7e2412361cdc0b429b7ac5fbfd05196a5909bd94aeb`,
447,412 bytes, 21 pages).

```bash
./target/release/paperengine extract --library /tmp/research-library
```

Outcome: `extracted=1 failed=0`. The extraction record
(`extraction/f2441d3d….json`) records extractor `paperengine/lopdf-text` version 1,
paperengine 0.1.0, 48,982 characters of text from 19 of 21 pages, 49,425 text bytes,
text sha256 `524b42f4cac2cbbdc943a4a20cbdad03fdbe4d7d2783a25be23fecd425c88c39`,
PDF `/Info` title "Tool Attention Is All You Need: …", author "Anuj Sadani; Deepak
Kumar", creator "arXiv GenPDF (tex2pdf:a6404ea)", and two honest per-page notes
("page 4/6: failed parsing ToUnicode CMap") — failures recorded, not hidden. A second
`extract` run skipped the completed record; `--force` re-extraction produced
byte-identical text (asserted by `tests/extraction_tests.rs`).

### Real end-to-end indexing and search evidence (CLI) — 2026-10-19

A second real object was acquired through Unpaywall
(`10.1371/journal.pone.0000219`, PLOS ONE, cc-by): 703,571 bytes, 14 pages, sha256
`4f111c22feab76f1f8b5e33474a01933f97d67cfbf56d4fbd53e7721edc22006` — byte-identical
to the OpenAlex-acquired payload recorded earlier in this report, cross-checking both
resolvers again.

```bash
./target/release/paperengine extract --library /tmp/research-library   # extracted=1 skipped=1
./target/release/paperengine index  --library /tmp/research-library    # added=1 skipped=1
./target/release/paperengine search --library /tmp/research-library --query "Arabidopsis microRNAs" --limit 5
```

Search result (human output):

```text
library: /tmp/research-library (2 indexed documents)
 1. [unpaywall] High-Throughput Sequencing of Arabidopsis microRNAs: Evidence for
    Frequent Birth and Death of MIRNA Genes (2007) — Noah Fahlgren
    sha256=4f111c22feab doi:10.1371/journal.pone.0000219 pages=14 bytes=703571 text=yes
```

A body-text-only query (`--query "stateless eager schema injection"` — words that
appear only in the extracted body, not in any title or author) returned the arXiv
paper as the sole hit, proving the full-text path end to end. Filter queries
(`--source openalex` → no matches; `--source arxiv` → the arXiv paper) behaved as
specified.

Restart/rebuild parity, deletion, version separation, update-after-extraction, and
canonical-metadata immutability under failed runs are covered by the 8 acceptance
tests in `tests/indexing_tests.rs`; extraction determinism, idempotent skip, budget
truncation, corrupt-object retry, and content-address verification by the 7 tests in
`tests/extraction_tests.rs`.

### Limitations (extraction/indexing slice)

- lopdf's text extractor does not reconstruct layout and cannot decode every
  custom/CID font; per-page failures are recorded as bounded notes and those pages
  contribute no text. No OCR.
- Image-only (scanned) PDFs yield an honest `empty` status, not fabricated text.
- The index is derived state; a schema-incompatible index requires `--rebuild`.
- No collection management or saved-search views yet (#33 follow-up scope).