//! Proves that a downstream crate sees the `Dummy` cartridge purely through
//! the `inventory` registry, with no kernel dependency on `tanova-nodes`.

use tanova_kernel::{
    Dag, EvalContext, KnobValue, MemoryGovernor, NodeId, RenderEpoch, UndoableCommand,
    find_cartridge, registered_cartridge_kinds,
};
use tanova_nodes::{DummyPayload, ensure_linked};

#[test]
fn dummy_cartridge_is_discovered_and_evaluated() {
    ensure_linked();

    let descriptor = find_cartridge("Dummy").expect("Dummy should be registered");
    let defaults = (descriptor.default_knobs)();
    assert_eq!(
        defaults.get("message"),
        Some(&KnobValue::Text("hello from dummy".to_string()))
    );
    assert!(registered_cartridge_kinds().contains(&"Dummy"));

    let mut dag = Dag::new();
    dag.apply_command(&UndoableCommand::AddNode {
        id: NodeId(1),
        kind: "Dummy".to_string(),
    })
    .expect("AddNode should apply");

    let node = dag.node(NodeId(1)).expect("node should exist");
    assert!(
        node.implementation.is_some(),
        "cartridge should be auto-wired from the registry"
    );
    assert_eq!(
        node.knobs.get("message"),
        Some(&KnobValue::Text("hello from dummy".to_string())),
        "default knobs should be populated"
    );

    let packet = dag
        .evaluate_pull(
            NodeId(1),
            &EvalContext::new(),
            &MemoryGovernor::new(usize::MAX),
            &RenderEpoch::new(),
            0,
        )
        .expect("dummy pull should succeed");

    assert_eq!(
        packet.get::<DummyPayload>(),
        Some(&DummyPayload("hello from dummy".to_string()))
    );
}
