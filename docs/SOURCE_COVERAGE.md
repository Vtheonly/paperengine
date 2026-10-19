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

## Newly implemented and live-verified (issue #12, 2026-10-19)

The five sources below were implemented in the second adapter wave and verified
against their live endpoints on 2026-10-19. Their previous "not implemented"
entries have been retired; the recorded access mechanisms turned out accurate.

### Europe PMC

- **Endpoint:** `https://www.ebi.ac.uk/europepmc/webservices/rest/search` (REST,
  JSON; `page`/`pageSize`); JATS full text at `/rest/<PMCID>/fullTextXML`
- **Adapter:** `src/sources/europepmc.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested and live-verified (L1+L2) on 2026-10-19
- **Access basis:** public REST API; no key or account required.
- **Capabilities:** discovery (`query=` search), DOI lookup via `DOI:"..."`
  fielded search, paper-type filtering (articles and PPR preprints by default).
- **Identifiers provided:** accession id, PMID, PMCID, DOI.
- **Full text:** **no direct PDF.** The site's `ptpmcrender.fcgi` render link is
  undocumented and answered 403 when probed (2026-10-19); availability flags
  (`isOpenAccess`, `inEPMC`, `hasPDF`) are retained verbatim in `raw` and never
  turned into a claimed PDF. Documented full-text form is JATS XML, reserved for
  the extraction slice (#31).
- **Rate policy:** >=1 s between requests.
- **Tests:** fixture tests in `src/sources/europepmc.rs` (`europepmc_search.json`);
  live tests `live_europepmc_discovery_returns_metadata_without_pdf_links` and
  `live_europepmc_doi_lookup_resolves_the_deseq2_paper`.
- **Last live result (2026-10-19):** discovery returned 3 records for "DESeq2";
  DOI lookup resolved `10.1186/s13059-014-0550-8` to PMID 25516281 / PMC4302049
  with no PDF claim.
- **Limitations / next step:** JATS XML ingestion (#31); no reference expansion.

### PubMed Central (NCBI)

- **Endpoint:** `https://eutils.ncbi.nlm.nih.gov/entrez/eutils` (`esearch` XML +
  `esummary` JSON, `db=pmc`)
- **Adapter:** `src/sources/pmc.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested and live-verified (L1+L2) on 2026-10-19
- **Access basis:** public E-utilities; NCBI documents up to 3 rps anonymous and
  the adapter is stricter (>=1 s). Two spaced requests per discovery page.
- **Capabilities:** discovery (`term=` with `retstart` paging), DOI lookup via
  the `[PID]` field with exact re-verification from `articleids`.
- **Identifiers provided:** PMCID, PMID, DOI, PII.
- **Full text:** **no direct PDF.** The official bulk route, the OA utility
  (`https://www.ncbi.nlm.nih.gov/pmc/utils/oa/oa.fcgi`), answered 404 from this
  environment when probed (2026-10-19, with and without an identifying
  User-Agent). Recorded as a live blocker; not bypassed or worked around.
- **Rate policy:** >=1 s between requests.
- **Tests:** fixture tests in `src/sources/pmc.rs` (`pmc_esearch.xml`,
  `pmc_esearch_doi.xml`, `pmc_esummary.json`, `pmc_esummary_deseq2.json`); live
  tests `live_pmc_discovery_returns_metadata_without_pdf_links` and
  `live_pmc_doi_lookup_resolves_the_deseq2_paper`.
- **Last live result (2026-10-19):** discovery returned 3 records for
  `DESeq2[title]`; DOI lookup resolved `10.1186/s13059-014-0550-8` to
  PMC4302049 with no PDF claim.
- **Limitations / next step:** re-probe `oa.fcgi` from other networks before any
  full-text claim; treat 404 as source-side until proven otherwise.

### DOAJ

- **Endpoint:** `https://doaj.org/api/search/articles/<query>` (REST, JSON;
  `page`/`pageSize`)
- **Adapter:** `src/sources/doaj.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested and live-verified (L1+L2+L3) on 2026-10-19
- **Access basis:** public application API; no key.
- **Capabilities:** discovery, DOI lookup (`doi:<doi>` search), direct PDF links
  from publisher-deposited `bibjson.link` entries (`type=fulltext,
  content_type=pdf`, http upgraded to https). HTML-only links become landing
  pages; no link, no claim.
- **Identifiers provided:** DOAJ record id, DOI.
- **Full text:** yes for a subset of records - publisher-deposited direct PDFs
  for fully OA journals.
- **Rate policy:** >=1 s between requests.
- **Tests:** fixture tests in `src/sources/doaj.rs` (`doaj_search.json`,
  `doaj_doi.json`); live tests
  `live_doaj_discovery_returns_records_with_publisher_pdf_links`,
  `live_doaj_doi_lookup_resolves_the_jis_article`, and
  `live_doaj_end_to_end_pdf_acquisition`.
- **Last live result (2026-10-19):** discovery returned 5 records (1 with a
  direct PDF link); end-to-end acquisition of
  `https://jis.uk.ac.ir/article_2666_aef0585c84c617658c4d44807222bfbc.pdf`
  produced a 1,457,506-byte, 16-page PDF, SHA-256
  `ccdda17512101bce0864c54679bf073511148f78af6e1e9679d95fb669947fed`,
  stored content-addressed with manifest provenance.
- **Limitations / next step:** not every OA deposit includes a direct PDF link -
  those records stay metadata-only by design.

### Zenodo

- **Endpoint:** `https://zenodo.org/api/records` (REST, JSON; `page`/`size`)
- **Adapter:** `src/sources/zenodo.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested and live-verified (L1+L2+L3) on 2026-10-19
- **Access basis:** public records API; no key.
- **Capabilities:** discovery (`q=` with quoted phrases), DOI lookup
  (`doi:"..."` quoted search), direct PDF from the first `.pdf` file of
  `publication`-typed records (`/api/records/<id>/files/<key>/content`).
  Datasets, software, and images are excluded by the default paper-only policy.
- **Identifiers provided:** record id, DOI (`10.5281/zenodo.<id>`).
- **Full text:** yes - direct file downloads with per-file MD5 checksums
  (retained verbatim in `raw`).
- **Rate policy:** >=1 s between requests. Zenodo answers 403 to clients
  without a User-Agent; the downloader now identifies the engine (fixed
  2026-10-19).
- **Tests:** fixture tests in `src/sources/zenodo.rs` (`zenodo_search.json`,
  `zenodo_doi.json`); live tests
  `live_zenodo_discovery_returns_publication_records_with_pdf_links`,
  `live_zenodo_doi_lookup_resolves_a_publication`, and
  `live_zenodo_end_to_end_pdf_acquisition`.
- **Last live result (2026-10-19):** discovery returned 3 records (3 with direct
  PDF links); end-to-end acquisition of
  `https://zenodo.org/api/records/18289747/files/2026-doebeli-honegger-attention-is-all-you-need.pdf/content`
  produced an 86,360-byte, 1-page PDF, SHA-256
  `365ea2d4da0adbe6d7fa6ef1e13c1ad2c4a8ec9c00120684bf17f0c32d5e1acb`.
- **Limitations / next step:** legacy records lack typed publication metadata;
  the `.pdf`-file-plus-`publication`-type rule is the honest filter.

### OSF Preprints

- **Endpoint:** `https://api.osf.io/v2/preprints/` (JSON:API,
  `application/vnd.api+json`; `page`/`page[size]`)
- **Adapter:** `src/sources/osf.rs` (contract version 1, adapter version 1)
- **Status:** fixture-tested and live-verified (L1+L2+L3) on 2026-10-19
- **Access basis:** public API; no account needed for open preprints.
- **Capabilities:** discovery via `filter[title]=` substring filter; direct PDF
  from the resolved `primary_file` of published preprints (only `.pdf` primary
  files become `oa_pdf_url`). Resolution costs one extra request per record,
  spaced by the rate policy. No documented DOI filter - DOI lookup is declared
  unsupported.
- **Identifiers provided:** preprint guid, DOI when minted.
- **Full text:** yes for published preprints with a PDF primary file.
- **Rate policy:** >=1 s between requests.
- **Tests:** fixture tests in `src/sources/osf.rs` (`osf_preprints.json`,
  `osf_primary_file.json`); live tests
  `live_osf_discovery_resolves_primary_files` and
  `live_osf_end_to_end_pdf_acquisition`.
- **Last live result (2026-10-19):** discovery returned 3 preprints (3 with
  resolved primary-file PDFs); end-to-end acquisition of
  `https://osf.io/download/6abeeb3c4eb5b6d31a39da8a/` produced a 408,201-byte,
  13-page PDF, SHA-256
  `5ca368518f4e76a8f9bff9c90bf4d4089ec96016c60cbcf8025485033e3c2de7`.
- **Limitations / next step:** per-record file resolution is N+1 by API design
  (`include=primary_file` was tested and returns no compound documents).

## Named in scope but not implemented

| Source | Canonical endpoint / mechanism | What it can provide | Next step |
|---|---|---|---|
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
