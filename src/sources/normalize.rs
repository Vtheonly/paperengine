//! Identifier and metadata normalization (#8).
//!
//! Deterministic, versioned canonicalization of scholarly identifiers. Equivalent surface
//! forms (`https://doi.org/10.1/x`, `doi:10.1/X`, bare `10.1/x`) normalize to one canonical
//! value so downstream deduplication and conflict reconciliation compare like with like.
//! Malformed inputs are rejected with `None`; they are never silently coerced.

use crate::sources::work_record::IdentifierKind;

/// Normalize a DOI. Accepts bare DOIs, `doi:` URNs, and `https?://(dx.)?doi.org/` URL forms.
/// Returns the bare, lowercased DOI or `None` when the input is not a plausible DOI.
pub fn normalize_doi(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("https://doi.org/")
        .or_else(|| trimmed.strip_prefix("http://doi.org/"))
        .or_else(|| trimmed.strip_prefix("https://dx.doi.org/"))
        .or_else(|| trimmed.strip_prefix("http://dx.doi.org/"))
        .or_else(|| trimmed.strip_prefix("doi:"))
        .or_else(|| trimmed.strip_prefix("DOI:"))
        .unwrap_or(trimmed);

    let doi = stripped.trim();
    // A DOI has a registrant prefix and a suffix separated by a slash, both non-empty,
    // and must not contain whitespace or another scheme.
    if doi.is_empty() || !doi.contains('/') {
        return None;
    }
    let (prefix, suffix) = doi.split_once('/')?;
    if prefix.is_empty() || suffix.is_empty() {
        return None;
    }
    if doi.chars().any(char::is_whitespace) {
        return None;
    }
    // Registrant codes are digits (possibly with dots for sub-registrants).
    if !prefix.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    if prefix.starts_with('.') || prefix.ends_with('.') {
        return None;
    }
    Some(doi.to_ascii_lowercase())
}

/// Normalize an arXiv identifier. Accepts modern (`2401.12345`, `2401.12345v2`),
/// legacy (`hep-th/9901001`), abs-page URLs (`https://arxiv.org/abs/2401.12345v2`) and the
/// `arXiv:` prefix. The returned value keeps the version suffix when present.
pub fn normalize_arxiv_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("https://arxiv.org/abs/")
        .or_else(|| trimmed.strip_prefix("http://arxiv.org/abs/"))
        .or_else(|| trimmed.strip_prefix("https://export.arxiv.org/abs/"))
        .or_else(|| trimmed.strip_prefix("http://export.arxiv.org/abs/"))
        .or_else(|| trimmed.strip_prefix("arxiv:"))
        .or_else(|| trimmed.strip_prefix("arXiv:"))
        .unwrap_or(trimmed);

    let candidate = stripped.trim();
    if candidate.is_empty() || candidate.chars().any(char::is_whitespace) {
        return None;
    }

    let (base, version) = match candidate.split_once('v') {
        Some((base, version)) if !version.is_empty() => {
            if !version.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (base, Some(version))
        }
        Some((_, "")) | None => (candidate, None),
        Some(_) => return None,
    };

    let valid_base = is_modern_arxiv_base(base) || is_legacy_arxiv_base(base);
    if !valid_base {
        return None;
    }
    Some(match version {
        Some(version) => format!("{base}v{version}"),
        None => base.to_owned(),
    })
}

fn is_modern_arxiv_base(base: &str) -> bool {
    // YYMM.NNNNN with one to seven digits (arXiv currently issues up to five).
    let Some((year_month, number)) = base.split_once('.') else {
        return false;
    };
    if year_month.len() != 4 || !year_month.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let year: Option<u16> = year_month[..2].parse().ok();
    let month: Option<u8> = year_month[2..].parse().ok();
    match (year, month) {
        (Some(year), Some(month)) if month > 0 && month <= 12 && year <= 99 => {}
        _ => return false,
    }
    (1..=7).contains(&number.len()) && number.chars().all(|c| c.is_ascii_digit())
}

fn is_legacy_arxiv_base(base: &str) -> bool {
    // Legacy identifiers are `archive/YYMMNNN` where the archive may be a dotted subject
    // path, e.g. `hep-th/9901001` or `math.GT/0309136`.
    let Some((archive, number)) = base.split_once('/') else {
        return false;
    };
    let valid_archive = !archive.is_empty()
        && archive
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !archive.starts_with('.')
        && !archive.ends_with('.')
        && !archive.contains("..");
    valid_archive && number.len() == 7 && number.chars().all(|c| c.is_ascii_digit())
}

/// Normalize a PubMed id: bare digits only.
pub fn normalize_pmid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let stripped = trimmed.trim_start_matches('0');
    if stripped.is_empty() {
        return None;
    }
    Some(stripped.to_owned())
}

/// Normalize a PubMed Central id (PMCID). Accepts `PMC4302049`, any case of the prefix
/// (`pmc…`, `Pmc…`), and the bare accession form `4302049`; leading zeros are stripped.
/// All forms canonicalize to `PMC4302049`. Malformed inputs are rejected.
pub fn normalize_pmcid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("PMC")
        .or_else(|| trimmed.strip_prefix("pmc"))
        .or_else(|| trimmed.strip_prefix("Pmc"))
        .unwrap_or(trimmed);
    if stripped.is_empty() || !stripped.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let without_leading_zeros = stripped.trim_start_matches('0');
    if without_leading_zeros.is_empty() {
        return None;
    }
    Some(format!("PMC{without_leading_zeros}"))
}

/// Collapse interior whitespace runs and trim. Used for titles before comparison or storage.
pub fn normalize_title(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a four-digit publication year within the plausible scholarly range.
pub fn normalize_year(raw: u64) -> Option<u16> {
    (1665..=2100).contains(&raw).then_some(raw as u16)
}

/// Extract the bare OpenAlex numeric identifier from a `https://openalex.org/W…` URI.
pub fn normalize_openalex_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("https://openalex.org/")
        .unwrap_or(trimmed);
    if stripped.starts_with('W') && stripped[1..].chars().all(|c| c.is_ascii_digit()) {
        Some(stripped.to_owned())
    } else {
        None
    }
}

/// Normalize any identifier of the given kind.
pub fn normalize_identifier(kind: IdentifierKind, raw: &str) -> Option<String> {
    match kind {
        IdentifierKind::Doi => normalize_doi(raw),
        IdentifierKind::Arxiv => normalize_arxiv_id(raw),
        IdentifierKind::Pmid => normalize_pmid(raw),
        IdentifierKind::Pmcid => normalize_pmcid(raw),
        IdentifierKind::Openalex => normalize_openalex_id(raw),
        IdentifierKind::Crossref => {
            let doi = normalize_doi(raw)?;
            Some(doi)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doi_equivalent_forms_normalize_identically() {
        let canonical = "10.1038/s41586-020-2649-2";
        for form in [
            "10.1038/S41586-020-2649-2",
            "https://doi.org/10.1038/s41586-020-2649-2",
            "http://dx.doi.org/10.1038/s41586-020-2649-2",
            "doi:10.1038/s41586-020-2649-2",
            "  10.1038/s41586-020-2649-2  ",
        ] {
            assert_eq!(
                normalize_doi(form).as_deref(),
                Some(canonical),
                "form: {form}"
            );
        }
    }

    #[test]
    fn doi_malformed_inputs_are_rejected() {
        for bad in [
            "",
            "not-a-doi",
            "10.",
            "/10.1038/x",
            "10.1038/",
            "10.1038/space inside",
            "https://example.org/10.1038/x",
            "abc/def",
            ".1/xyz",
        ] {
            assert!(normalize_doi(bad).is_none(), "input: {bad}");
        }
    }

    #[test]
    fn arxiv_modern_ids_keep_version_suffixes() {
        assert_eq!(
            normalize_arxiv_id("https://arxiv.org/abs/2401.12345v2").as_deref(),
            Some("2401.12345v2")
        );
        assert_eq!(
            normalize_arxiv_id("arXiv:2401.12345").as_deref(),
            Some("2401.12345")
        );
        assert_eq!(
            normalize_arxiv_id("2401.12345v1").as_deref(),
            Some("2401.12345v1")
        );
    }

    #[test]
    fn arxiv_legacy_ids_are_accepted() {
        assert_eq!(
            normalize_arxiv_id("hep-th/9901001").as_deref(),
            Some("hep-th/9901001")
        );
        assert_eq!(
            normalize_arxiv_id("https://arxiv.org/abs/math.GT/0309136").as_deref(),
            Some("math.GT/0309136")
        );
        assert_eq!(
            normalize_arxiv_id("astro-ph/9711233v1").as_deref(),
            Some("astro-ph/9711233v1")
        );
    }

    #[test]
    fn arxiv_malformed_inputs_are_rejected() {
        for bad in [
            "",
            "2401",
            "2401.",
            "2401.12345v",
            "2401.12345vx",
            "13xx.12345",
            "2401.12345 extra",
            "2413.12345",
            "/9901001",
            "hep-th/99010",
            "hep-th/9901001x",
        ] {
            assert!(normalize_arxiv_id(bad).is_none(), "input: {bad}");
        }
    }

    #[test]
    fn pmid_normalization_strips_leading_zeros() {
        assert_eq!(normalize_pmid(" 00318904 ").as_deref(), Some("318904"));
        assert!(normalize_pmid("PMC318904").is_none());
        assert!(normalize_pmid("").is_none());
        assert!(normalize_pmid("0").is_none());
    }

    #[test]
    fn pmcid_forms_canonicalize_with_prefix() {
        assert_eq!(normalize_pmcid("PMC4302049").as_deref(), Some("PMC4302049"));
        assert_eq!(normalize_pmcid("pmc4302049").as_deref(), Some("PMC4302049"));
        assert_eq!(normalize_pmcid("4302049").as_deref(), Some("PMC4302049"));
        assert_eq!(
            normalize_pmcid("PMC004302049").as_deref(),
            Some("PMC4302049")
        );
        assert!(normalize_pmcid("PMC").is_none());
        assert!(normalize_pmcid("0").is_none());
        assert!(normalize_pmcid("PMC43020x49").is_none());
        assert!(normalize_pmcid("").is_none());
    }

    #[test]
    fn title_whitespace_is_collapsed() {
        assert_eq!(
            normalize_title("  Deep    learning\n\tfor  climate  "),
            "Deep learning for climate"
        );
        assert_eq!(normalize_title("   "), "");
    }

    #[test]
    fn years_outside_plausible_range_are_dropped() {
        assert_eq!(normalize_year(2024), Some(2024));
        assert_eq!(normalize_year(1665), Some(1665));
        assert_eq!(normalize_year(123), None);
        assert_eq!(normalize_year(3000), None);
    }

    #[test]
    fn openalex_ids_are_trimmed_to_bare_form() {
        assert_eq!(
            normalize_openalex_id("https://openalex.org/W2741809807").as_deref(),
            Some("W2741809807")
        );
        assert!(normalize_openalex_id("https://openalex.org/C123").is_none());
        assert!(normalize_openalex_id("W2741809807").is_some());
    }
}
