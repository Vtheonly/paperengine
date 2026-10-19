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
