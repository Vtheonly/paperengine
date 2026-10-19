# Source Coverage Registry

This registry is the authoritative, honest inventory of every scholarly source named in
this repository's scope (README, docs, and the issue backlog). A source is **supported**
only when an adapter implements the documented access mechanism and the claimed capability
has been tested. Metadata-only sources are never advertised as full-text sources.

Adapter status values:

| Status | Meaning |
|---|---|
| **not implemented** | Named in scope; no adapter code exists yet |
| **implemented** | Adapter exists, covered by offline fixture tests |
| **fixture-tested** | Implemented and tested against recorded real API responses |
| **live-verified** | Exercised against the live endpoint on the recorded date |

Verification layers: **L1** = offline fixture test; **L2** = live endpoint smoke test;
**L3** = real end-to-end PDF acquisition through the production pipeline.

Live-test command (opt-in, bounded):

```bash
PAPERENGINE_LIVE_TESTS=1 cargo test --test live_sources -- --ignored --nocapture
# Layer 3 (downloads exactly one small arXiv PDF):
PAPERENGINE_LIVE_ACQUIRE=1 PAPERENGINE_LIVE_TESTS=1 cargo test --test live_sources -- --ignored --nocapture
```

## Implemented sources

### OpenAlex

- **Endpoint:** `https://api.openalex.org/works` (REST, JSON; cursor pagination)
- **Adapter:** `src/sources/openalex.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested; DOI lookup live-verified 2026-10-19; search currently
  **blocked** from this network by the shared free-tier budget (see below)
- **Access basis:** public REST API with a polite contact e-mail. Since 2026 OpenAlex
  shares one free daily budget per IP for key-less requests and offers free per-key
  budgets; the adapter supports an optional `openalex_api_key`.
- **Capabilities:** discovery (search, cursor pagination), DOI lookup, paper-type
  filtering. `best_oa_location.pdf_url` is recorded as `oa_pdf_url` only when OpenAlex
  reports one; `pdf_url: null` never becomes a fabricated link.
- **Identifiers provided:** OpenAlex work id (`W…`), DOI.
- **Full text:** OpenAlex itself serves metadata only; the PDF URLs it reports point to
  third-party repositories and publishers. Treat OpenAlex as an OA-link resolver.
- **Rate policy:** ≥1 s between requests (polite pool); 429 surfaces as
  `RateLimited` with the `Retry-After` hint and is never retried past refusal.
- **Tests:** fixture tests in `src/sources/openalex.rs`
  (`tests/fixtures/openalex_search.json`, `openalex_work.json` are real recorded
  responses; `openalex_search_synthetic.json` is a clearly-labeled schema-accurate
  fixture covering the `pdf_url` path the recorded responses leave `null`); live tests
  `live_openalex_doi_lookup_resolves_a_work` and
  `live_openalex_discovery_or_blocked_by_free_tier_budget` in `tests/live_sources.rs`.
- **Last live result (2026-10-19):** DOI lookup of `10.7717/peerj.4375` returned
  W2741809807 ("The state of OA…"). Search returned HTTP 429 ("free daily budget shared
  by everyone on your network's IP address… is used up") — recorded as BLOCKED, not as
  failure; an API key lifts this limit and is a documented configuration option.
- **Limitations / next step:** live search verification pending an API key or budget
  reset; add `from_publication_date` filters and incremental ingestion (issue #11
  remainder).

### arXiv

- **Endpoint:** `https://export.arxiv.org/api/query` (Atom XML; `start`/`max_results`
  pagination)
- **Adapter:** `src/sources/arxiv.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested and **live-verified (L1+L2+L3)** on 2026-10-19
- **Access basis:** public API for modest, rate-limited queries; arXiv asks for ≥3 s
  between requests. Bulk harvesting belongs to arXiv's requester-pays S3 buckets, which
  this adapter deliberately does not use.
- **Capabilities:** discovery via `all:"phrase"` search, direct OA PDF links
  (`link[rel=related][type=application/pdf]`, upgraded to https), arXiv-id
  normalization for modern (`2401.12345`) and legacy (`hep-th/9901001`,
  `math.GT/0309136`) forms, optional `arxiv:doi` capture.
- **Identifiers provided:** arXiv id (with version), DOI when the entry carries one.
- **Full text:** yes — every entry exposes a direct, openly accessible PDF URL.
- **Rate policy:** 3 s minimum interval enforced by the engine.
- **Tests:** fixture tests in `src/sources/arxiv.rs` (recorded Atom feed); live tests
  `live_arxiv_discovery_returns_parseable_records_with_pdf_links` and
  `live_arxiv_end_to_end_pdf_acquisition` in `tests/live_sources.rs`.
- **Last live result (2026-10-19):** discovery returned 3 records for the phrase
  "attention is all you need"; end-to-end acquisition of
  `https://arxiv.org/pdf/2604.21816v1` produced a 447,412-byte, 21-page PDF,
  SHA-256 `f2441d3de469aa1e7c04b7e2412361cdc0b429b7ac5fbfd05196a5909bd94aeb`,
  stored content-addressed and re-opened by the structural parser (lopdf).
- **Limitations / next step:** no full-text search field selection (`ti:`, `abs:`),
  no incremental sync; those remain issue #12 remainder.

### Crossref

- **Endpoint:** `https://api.crossref.org/works` (REST, JSON; offset pagination)
- **Adapter:** `src/sources/crossref.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested and live-verified (L1+L2) on 2026-10-19
- **Access basis:** public REST API with a polite contact e-mail.
- **Capabilities:** discovery (`query=`) and DOI lookup. **Metadata only**: the
  `link` entries Crossref returns are publisher text-and-data-mining links that usually
  require a subscription; the adapter never records them as OA PDFs and leaves
  `oa_pdf_url` unset. Use the Unpaywall adapter to resolve open-access locations.
- **Identifiers provided:** DOI (normalized), Crossref landing URL.
- **Full text:** no — Crossref does not provide openly accessible full text.
- **Rate policy:** ≥1 s between requests (polite pool).
- **Tests:** fixture tests in `src/sources/crossref.rs` (recorded work-list response);
  live tests `live_crossref_discovery_returns_metadata_without_pdf_links` and
  `live_crossref_doi_lookup_resolves_a_stable_doi` in `tests/live_sources.rs`.
- **Last live result (2026-10-19):** discovery returned 3 records (first DOI
  `10.1007/978-3-031-84300-6_13`); DOI lookup resolved
  `10.1371/journal.pone.0000219`. Note: pre-1956 DOIs such as `10.1038/171737a`
  return 404 from the Crossref REST API — the adapter correctly maps this to
  "no record".
- **Limitations / next step:** no reference/citation expansion, no incremental
  ingestion (issue #11 remainder).

### Unpaywall

- **Endpoint:** `https://api.unpaywall.org/v2/{doi}?email=…` (REST, JSON)
- **Adapter:** `src/sources/unpaywall.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested and live-verified (L1+L2) on 2026-10-19
- **Access basis:** free public API; a contact e-mail is a hard requirement and its
  absence is a typed configuration error.
- **Capabilities:** DOI lookup resolving the best open-access location.
  `oa_pdf_url` is set only when `best_oa_location.url_for_pdf` is present; a
  repository landing page without a direct PDF is recorded as `landing_url` only.
- **Identifiers provided:** DOI.
- **Full text:** resolves direct PDF URLs for OA works (typically gold/green OA
  copies on publisher or repository hosts).
- **Rate policy:** ≥1 s between requests; documented free tier is generous
  (100k calls/day) and was never approached by these tests.
- **Tests:** fixture tests in `src/sources/unpaywall.rs` (recorded response for
  `10.1371/journal.pone.0000219`); live test
  `live_unpaywall_resolves_an_oa_doi_with_a_direct_pdf` in `tests/live_sources.rs`.
- **Last live result (2026-10-19):** lookup of `10.1371/journal.pone.0000219`
  returned `is_oa: true` with direct PDF
  `https://journals.plos.org/plosone/article/file?id=10.1371/journal.pone.0000219&type=printable`
  (cc-by).
- **Limitations / next step:** no batch DOI resolution, no oa_locations enumeration
  beyond the best location (issue #11 remainder).

## Named in scope but not implemented

These sources are explicitly named in issues #12 and #13. They have **no adapter code**;
the entries below record their documented access mechanisms so the next slice starts from
facts, not guesses. None of them may be claimed as supported until an adapter passes the
same verification layers.

| Source | Canonical endpoint / mechanism | What it can provide | Next step |
|---|---|---|---|
| **PubMed Central (PMC)** | NCBI E-utilities (`eutils.ncbi.nlm.nih.gov`, `efetch`/`esearch`) and OA service; bulk via NCBI OA file shares | Metadata and, for OA subsets, JATS XML full text; PDFs only where explicitly offered in the OA package | Implement E-utilities adapter with API key support (#12) |
| **Europe PMC** | REST API `https://www.ebi.ac.uk/europepmc/webservices/rest/search` and full-text XML for OA subset | Metadata; JATS XML full text for OA records | Implement adapter reusing the Unpaywall-style lookup shape (#12) |
| **DOAJ** | REST API `https://doaj.org/api/search/articles/{query}`; application metadata | Article metadata for fully OA journals; links to publisher copies (usually not direct PDFs) | Implement discovery adapter (#12) |
| **Zenodo** | REST API `https://zenodo.org/api/records` (Invenio) | Metadata and direct file download URLs for open uploads, with checksums | Implement adapter; verify file checksums from API metadata (#12) |
| **OSF** | REST API `https://api.osf.io/v2/` (JSON:API) | Project/file metadata; direct file downloads for public nodes | Implement adapter (#12) |
| **OAI-PMH repositories (generic)** | `Identify`/`ListRecords`/`ListIdentifiers` with `resumptionToken` flow | Harvestable metadata (Dublin Core and friends); full text only when the repository exposes content URLs | Implement the generic harvester behind the versioned source contract (#13) |

## What is deliberately not supported

- Bypassing paywalls, logins, CAPTCHAs, quotas, 403/429 responses, or any explicit
  source restriction. These are stop conditions surfaced as typed errors
  (`SourceError::Forbidden`, `SourceError::RateLimited`), never challenges.
- Aggressive crawling. Every adapter declares a rate policy the engine enforces, and
  discovery/acquisition runs carry explicit page, record, download, and byte budgets.
- HTML scraping where a documented API exists. The implemented adapters use official
  REST/Atom interfaces only.

## Verification evidence archive

Machine-readable evidence from the 2026-10-19 verification run is reproduced in
`docs/VERIFICATION.md`, including the exact commands, sizes, hashes, and storage paths.
