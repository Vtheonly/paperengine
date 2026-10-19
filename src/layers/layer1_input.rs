//! Normalize adapter-owned primitive values at the edge of the pure kernel.

use super::layer2_state::{FrontierNodeHot, KNOWN_FLAGS};
use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandidateInput {
    pub paper_id: u64,
    pub citation_count: u32,
    pub venue_rank: u16,
    pub year: u16,
    pub estimated_size_kib: u32,
    pub source_id: u32,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputError {
    MissingPaperId,
    PublicationYearTooFarInFuture { year: u16, reference_year: u16 },
    UnknownFlags { bits: u32 },
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPaperId => write!(f, "paper_id must be non-zero"),
            Self::PublicationYearTooFarInFuture {
                year,
                reference_year,
            } => write!(
                f,
                "year {year} exceeds reference year {reference_year} plus one"
            ),
            Self::UnknownFlags { bits } => write!(f, "unknown candidate flag bits: {bits:#x}"),
        }
    }
}
impl std::error::Error for InputError {}

pub fn normalize_candidate(
    input: CandidateInput,
    reference_year: u16,
) -> Result<FrontierNodeHot, InputError> {
    if input.paper_id == 0 {
        return Err(InputError::MissingPaperId);
    }
    let unknown_flags = input.flags & !KNOWN_FLAGS;
    if unknown_flags != 0 {
        return Err(InputError::UnknownFlags {
            bits: unknown_flags,
        });
    }
    if input.year != 0 && input.year > reference_year.saturating_add(1) {
        return Err(InputError::PublicationYearTooFarInFuture {
            year: input.year,
            reference_year,
        });
    }
    Ok(FrontierNodeHot {
        paper_id: input.paper_id,
        citation_count: input.citation_count,
        venue_rank: input.venue_rank,
        year: input.year,
        estimated_size_kib: input.estimated_size_kib,
        source_id: input.source_id,
        flags: input.flags,
        _reserved: [0; 36],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::layer2_state::FLAG_HAS_METADATA;

    fn sample() -> CandidateInput {
        CandidateInput {
            paper_id: 9,
            citation_count: 10,
            venue_rank: 2,
            year: 2024,
            estimated_size_kib: 32,
            source_id: 1,
            flags: FLAG_HAS_METADATA,
        }
    }

    #[test]
    fn creates_compact_hot_record() {
        let node = normalize_candidate(sample(), 2026).unwrap();
        assert_eq!(core::mem::size_of_val(&node), 64);
        assert_eq!(node.paper_id, 9);
    }

    #[test]
    fn rejects_zero_identity_and_unknown_bits() {
        let mut input = sample();
        input.paper_id = 0;
        assert_eq!(
            normalize_candidate(input, 2026),
            Err(InputError::MissingPaperId)
        );
        input = sample();
        input.flags |= 1 << 31;
        assert_eq!(
            normalize_candidate(input, 2026),
            Err(InputError::UnknownFlags { bits: 1 << 31 })
        );
    }
}
