use paperengine::engine_architecture::EngineArchitecture;
use paperengine::layers::layer2_state::{FrontierNodeHot, FLAG_HAS_METADATA};
use paperengine::{AcquisitionEngine, PolicyConfig};

#[test]
fn formal_engine_trait_runs_until_terminal_and_projects_a_decision() {
    let frontier = [
        FrontierNodeHot {
            paper_id: 11,
            citation_count: 20,
            year: 2025,
            flags: FLAG_HAS_METADATA,
            ..FrontierNodeHot::default()
        },
        FrontierNodeHot {
            paper_id: 12,
            citation_count: 100,
            year: 2024,
            flags: FLAG_HAS_METADATA,
            ..FrontierNodeHot::default()
        },
    ];
    let engine = AcquisitionEngine::new(PolicyConfig::new(2026, 1).unwrap()).unwrap();
    let context = ();
    let mut state = engine.initialize_state(&frontier, &context).unwrap();

    while !engine.is_terminal(&state, &context) {
        engine.transition(&mut state, &frontier, &context, 1).unwrap();
    }

    let decision = engine.project_result(&state, &frontier, &context).unwrap().unwrap();
    assert_eq!(decision.id.paper_id, 12);
    assert_eq!(decision.candidate_index, 1);
}
