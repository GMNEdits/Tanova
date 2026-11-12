//! Knob mirroring tests: the properties panel reads `GraphMirror`, so every
//! `AddNode` / `SetKnob` / `Undo` / `Redo` event must keep its knob table exact.

use tanova_kernel::{
    KernelEvent, KnobValue, NodeId, UndoableCommand, find_cartridge, registered_cartridge_kinds,
};
use tanova_ui::GraphMirror;

fn set_color(node: NodeId, value: [f32; 4], previous: [f32; 4]) -> UndoableCommand {
    UndoableCommand::SetKnob {
        node,
        knob: "color".to_string(),
        value: KnobValue::Color(value),
        previous: KnobValue::Color(previous),
    }
}

fn color_of(graph: &GraphMirror, node: NodeId) -> [f32; 4] {
    match graph.node(node).unwrap().knobs.get("color") {
        Some(KnobValue::Color(color)) => *color,
        other => panic!("expected a color knob, found {other:?}"),
    }
}

#[test]
fn knob_table_follows_add_set_undo_and_redo() {
    tanova_nodes::ensure_linked();
    assert!(
        registered_cartridge_kinds().contains(&"Constant"),
        "the Constant cartridge must be linked"
    );

    let mut graph = GraphMirror::default();
    let node = NodeId(1);

    graph.apply_event(&KernelEvent::Applied(UndoableCommand::AddNode {
        id: node,
        kind: "Constant".to_string(),
        x: 0.0,
        y: 0.0,
    }));

    let expected = (find_cartridge("Constant")
        .expect("Constant cartridge")
        .default_knobs)();
    let mirrored = &graph.node(node).expect("node mirrored").knobs;
    assert_eq!(mirrored.len(), expected.len());
    for (name, value) in expected.iter() {
        assert_eq!(mirrored.get(name), Some(value), "knob {name} mismatch");
    }

    let original = color_of(&graph, node);
    let graded = [0.18, 0.35, 0.65, 1.0];
    let command = set_color(node, graded, original);

    graph.apply_event(&KernelEvent::Applied(command.clone()));
    assert_eq!(color_of(&graph, node), graded);

    graph.apply_event(&KernelEvent::Undone(command.clone()));
    assert_eq!(color_of(&graph, node), original);

    graph.apply_event(&KernelEvent::Redone(command));
    assert_eq!(color_of(&graph, node), graded);
}

#[test]
fn set_knob_on_an_unknown_node_is_ignored() {
    let mut graph = GraphMirror::default();
    graph.apply_event(&KernelEvent::Applied(set_color(
        NodeId(9),
        [1.0, 0.0, 0.0, 1.0],
        [0.0, 0.0, 0.0, 1.0],
    )));
    assert!(graph.node(NodeId(9)).is_none());
}
