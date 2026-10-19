//! Pure candidate transition: score, exclude restricted work, select deterministically.

use super::layer2_state::{FrontierNodeHot, FLAG_RESTRICTED_BY_SOURCE};
use super::layer4_optimization::{score_candidates_batch, SCORE_BATCH_SIZE};
use super::layer6_output::{action_for_candidate, AcquisitionDecision, DecisionId};

pub(crate) fn transition_batch(
    candidates: &[FrontierNodeHot],
    first_index: usize,
    reference_year: u16,
    policy_version: u32,
    prefer_simd: bool,
    best: &mut Option<AcquisitionDecision>,
) {
    let scores = score_candidates_batch(candidates, reference_year, prefer_simd);
    for (offset, node) in candidates.iter().take(SCORE_BATCH_SIZE).enumerate() {
        if node.has_flag(FLAG_RESTRICTED_BY_SOURCE) {
            continue;
        }
        let next = AcquisitionDecision {
            id: DecisionId {
                paper_id: node.paper_id,
                policy_version,
            },
            candidate_index: first_index + offset,
            score: scores[offset],
            action: action_for_candidate(node),
        };
        let replace = match best {
            None => true,
            Some(current) => {
                next.score > current.score
                    || (next.score == current.score
                        && (next.id.paper_id < current.id.paper_id
                            || (next.id.paper_id == current.id.paper_id
                                && next.candidate_index < current.candidate_index)))
            }
        };
        if replace {
            *best = Some(next);
        }
    }
}
