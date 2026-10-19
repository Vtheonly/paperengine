//! Fixed-size SoA scoring with an optional runtime-gated AVX2 path.

use super::layer2_state::{
    FrontierNodeHot, FLAG_AUTHORIZED_FULL_TEXT, FLAG_HAS_METADATA,
    FLAG_JATS_AVAILABLE, FLAG_PDF_SIGNATURE_VERIFIED,
};

pub const SCORE_BATCH_SIZE: usize = 16;
const CITATION_WEIGHT: i32 = 4;
const VENUE_WEIGHT: i32 = 128;
const RECENCY_WEIGHT: i32 = 64;
const AUTHORIZED_BONUS: i32 = 120_000;
const PDF_BONUS: i32 = 240_000;
const JATS_BONUS: i32 = 30_000;
const METADATA_BONUS: i32 = 6_000;

#[inline]
pub fn avx2_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    { std::is_x86_feature_detected!("avx2") }
    #[cfg(not(target_arch = "x86_64"))]
    { false }
}

#[inline]
fn recency(year: u16, reference_year: u16) -> i32 {
    if year == 0 || year > reference_year { return 0; }
    (100 - reference_year.saturating_sub(year).min(100) as i32) * RECENCY_WEIGHT
}

#[inline]
fn bonuses(node: &FrontierNodeHot) -> i32 {
    let mut score = 0;
    if node.has_flag(FLAG_AUTHORIZED_FULL_TEXT) { score += AUTHORIZED_BONUS; }
    if node.has_flag(FLAG_PDF_SIGNATURE_VERIFIED) { score += PDF_BONUS; }
    if node.has_flag(FLAG_JATS_AVAILABLE) { score += JATS_BONUS; }
    if node.has_flag(FLAG_HAS_METADATA) { score += METADATA_BONUS; }
    score
}

/// Score at most 16 candidates using fixed stack arrays; unused lanes remain zero.
pub fn score_candidates_batch(
    candidates: &[FrontierNodeHot],
    reference_year: u16,
    prefer_simd: bool,
) -> [i32; SCORE_BATCH_SIZE] {
    let mut citations = [0_i32; SCORE_BATCH_SIZE];
    let mut venues = [0_i32; SCORE_BATCH_SIZE];
    let mut years = [0_i32; SCORE_BATCH_SIZE];
    let mut sizes = [0_i32; SCORE_BATCH_SIZE];
    let mut extras = [0_i32; SCORE_BATCH_SIZE];
    let mut scores = [0_i32; SCORE_BATCH_SIZE];
    let count = candidates.len().min(SCORE_BATCH_SIZE);

    for (i, node) in candidates.iter().take(count).enumerate() {
        citations[i] = node.citation_count.min(100_000) as i32;
        venues[i] = node.venue_rank.min(1_000) as i32;
        years[i] = recency(node.year, reference_year);
        sizes[i] = node.estimated_size_kib.min(1_000_000) as i32;
        extras[i] = bonuses(node);
    }

    #[cfg(target_arch = "x86_64")]
    if prefer_simd && count == SCORE_BATCH_SIZE && avx2_available() {
        // SAFETY: runtime detection confirms AVX2 and all arrays contain 16 initialized i32s.
        unsafe { score_avx2(&citations, &venues, &years, &sizes, &extras, &mut scores) };
        return scores;
    }

    for (i, score) in scores.iter_mut().enumerate().take(count) {
        *score = citations[i] * CITATION_WEIGHT
            + venues[i] * VENUE_WEIGHT
            + years[i]
            - (sizes[i] >> 2)
            + extras[i];
    }
    scores
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
/// # Safety
/// The caller must verify AVX2 support and provide initialized arrays of 16 i32 elements.
unsafe fn score_avx2(
    citations: &[i32; SCORE_BATCH_SIZE],
    venues: &[i32; SCORE_BATCH_SIZE],
    years: &[i32; SCORE_BATCH_SIZE],
    sizes: &[i32; SCORE_BATCH_SIZE],
    extras: &[i32; SCORE_BATCH_SIZE],
    output: &mut [i32; SCORE_BATCH_SIZE],
) {
    use std::arch::x86_64::{
        _mm256_add_epi32, _mm256_loadu_si256, _mm256_mullo_epi32, _mm256_set1_epi32,
        _mm256_srli_epi32, _mm256_storeu_si256, _mm256_sub_epi32,
    };
    let cw = _mm256_set1_epi32(CITATION_WEIGHT);
    let vw = _mm256_set1_epi32(VENUE_WEIGHT);
    for offset in [0_usize, 8] {
        // SAFETY: each pointer spans eight i32 elements and unaligned operations are used.
        let c = unsafe { _mm256_loadu_si256(citations.as_ptr().add(offset).cast()) };
        let v = unsafe { _mm256_loadu_si256(venues.as_ptr().add(offset).cast()) };
        let y = unsafe { _mm256_loadu_si256(years.as_ptr().add(offset).cast()) };
        let s = unsafe { _mm256_loadu_si256(sizes.as_ptr().add(offset).cast()) };
        let e = unsafe { _mm256_loadu_si256(extras.as_ptr().add(offset).cast()) };
        let score = _mm256_add_epi32(_mm256_mullo_epi32(c, cw), _mm256_mullo_epi32(v, vw));
        let score = _mm256_add_epi32(score, y);
        let score = _mm256_sub_epi32(score, _mm256_srli_epi32(s, 2));
        let score = _mm256_add_epi32(score, e);
        unsafe { _mm256_storeu_si256(output.as_mut_ptr().add(offset).cast(), score) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::layer2_state::{
        FLAG_AUTHORIZED_FULL_TEXT, FLAG_HAS_METADATA, FLAG_PDF_SIGNATURE_VERIFIED,
    };

    #[test]
    fn simd_dispatch_matches_scalar_formula() {
        let nodes: Vec<_> = (1..=SCORE_BATCH_SIZE).map(|i| FrontierNodeHot {
            paper_id: i as u64,
            citation_count: (i * 500) as u32,
            venue_rank: i as u16,
            year: 1990 + i as u16,
            estimated_size_kib: (i * 1024) as u32,
            flags: if i % 2 == 0 {
                FLAG_AUTHORIZED_FULL_TEXT | FLAG_PDF_SIGNATURE_VERIFIED | FLAG_HAS_METADATA
            } else { FLAG_HAS_METADATA },
            ..FrontierNodeHot::default()
        }).collect();
        assert_eq!(
            score_candidates_batch(&nodes, 2026, false),
            score_candidates_batch(&nodes, 2026, true)
        );
    }
}
