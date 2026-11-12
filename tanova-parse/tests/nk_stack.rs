//! `.nk` stack-machine, passthrough, and round-trip tests.

use tanova_kernel::{
    Dag, EvalContext, KnobValue, MemoryGovernor, NodeId, PortId, RenderEpoch, Time,
};
use tanova_parse::{load_nk_into_dag, parse_nk_script};
use tanova_render::Raster2D;

const BRANCHED: &str = r#"
# Constant -> Grade feeds A; the Constant itself feeds B.
Constant {
 inputs 0
 name Constant1
 color {1 0 0 1}
 format_width 2
 format_height 2
}
set N1 [stack 0]
Grade {
 inputs 1
 name Grade1
 multiply 0.5
}
set N2 [stack 0]
push 0
push $N2
push $N1
Merge2 {
 inputs 2+1
 name Merge1
}
set N3 [stack 0]
"#;

fn render(dag: &mut Dag, target: NodeId) -> Raster2D {
    let mut ctx = EvalContext::new();
    ctx.insert(Time(1.0));
    let packet = dag
        .evaluate_pull(
            target,
            &ctx,
            &MemoryGovernor::new(64 * 1024 * 1024),
            &RenderEpoch::new(),
            0,
        )
        .expect("target should evaluate");
    packet
        .get::<Raster2D>()
        .cloned()
        .expect("target should produce a Raster2D")
}

#[test]
fn stack_branching_and_push_zero_wire_b_a_and_mask() {
    tanova_nodes::ensure_linked();
    let mut dag = Dag::new();
    let parsed = load_nk_into_dag(BRANCHED, &mut dag).expect("script should load");

    let merge = parsed.names["Merge1"];
    let constant = parsed.names["Constant1"];
    let grade = parsed.names["Grade1"];

    let merge_node = parsed
        .nodes
        .iter()
        .find(|node| node.id == merge)
        .expect("merge parsed");
    assert_eq!(merge_node.inputs, 3, "inputs 2+1 means three ports");

    let mut incoming: Vec<_> = dag
        .edges()
        .filter(|edge| edge.to_node == merge)
        .map(|edge| (edge.to_port, edge.from_node))
        .collect();
    incoming.sort_by_key(|(port, _)| *port);

    assert_eq!(
        incoming,
        vec![
            (PortId(0), constant), // B
            (PortId(1), grade),    // A
        ],
        "B and A must land on ports 0 and 1; the mask stays disconnected"
    );
    assert_eq!(parsed.stack_top, Some(merge), "Merge2 must be on top");
}

#[test]
fn unknown_node_preserves_knobs_and_passes_pixels_through() {
    tanova_nodes::ensure_linked();
    let script = r#"
Constant {
 inputs 0
 name Constant1
 color {0.2 0.4 0.6 1}
 format_width 2
 format_height 2
}
set N1 [stack 0]
UnknownProprietaryNode {
 inputs 1
 name Secret1
 secret_knob 42
}
Grade {
 inputs 1
 name Grade1
 multiply 2
}
"#;

    let mut dag = Dag::new();
    let parsed = load_nk_into_dag(script, &mut dag).expect("script should load");

    let secret = parsed.names["Secret1"];
    let node = dag.node(secret).expect("unknown node exists");
    assert_eq!(
        node.knobs.get("secret_knob"),
        Some(&KnobValue::Int(42)),
        "unknown knobs must be preserved"
    );
    assert!(
        node.implementation.is_some(),
        "unknown nodes must receive the passthrough cartridge"
    );

    let grade = parsed.names["Grade1"];
    let raster = render(&mut dag, grade);
    let pixel = raster.sample_rgba(0, 0);
    assert!((pixel[0] - 0.4).abs() < 1e-4, "red was {}", pixel[0]);
    assert!((pixel[1] - 0.8).abs() < 1e-4, "green was {}", pixel[1]);
    assert!((pixel[2] - 1.2).abs() < 1e-4, "blue was {}", pixel[2]);
    assert!((pixel[3] - 1.0).abs() < 1e-4, "alpha was {}", pixel[3]);
}

#[test]
fn export_round_trips_unknown_nodes_and_knobs() {
    tanova_nodes::ensure_linked();
    let script = r#"
Constant {
 inputs 0
 name Constant1
 color {0.2 0.4 0.6 1}
 format_width 2
 format_height 2
}
set N1 [stack 0]
Foundry_CameraShake {
 inputs 1
 name Shake1
 secret_knob 42
 jitter 0.25
}
Grade {
 inputs 1
 name Grade1
 multiply 2
}
"#;

    let mut dag = Dag::new();
    let _parsed = load_nk_into_dag(script, &mut dag).expect("script should load");
    let exported = dag.export_to_tcl();

    assert!(exported.contains("Foundry_CameraShake"));
    assert!(exported.contains("secret_knob 42"));
    assert!(exported.contains("jitter 0.25"));
    assert!(exported.contains("push $N1"));

    let mut round = Dag::new();
    let reparsed = load_nk_into_dag(&exported, &mut round).expect("export must reparse");
    assert_eq!(round.node_count(), dag.node_count());
    assert_eq!(round.edge_count(), dag.edge_count());

    let shake = reparsed.names["Shake1"];
    let node = round.node(shake).expect("round-tripped unknown node");
    assert_eq!(node.knobs.get("secret_knob"), Some(&KnobValue::Int(42)));
    assert_eq!(node.knobs.get("jitter"), Some(&KnobValue::Float(0.25)));

    // The round-tripped graph still renders the same pixels.
    let grade = reparsed.names["Grade1"];
    let raster = render(&mut round, grade);
    assert!((raster.sample_rgba(0, 0)[0] - 0.4).abs() < 1e-4);
}

#[test]
fn omitted_inputs_use_nuke_defaults() {
    tanova_nodes::ensure_linked();
    let parsed = parse_nk_script(
        r#"
Constant {
 name Constant1
}
Grade {
 name Grade1
}
Blur {
 name Blur1
}
Viewer {
 name Viewer1
}
"#,
    )
    .expect("script should parse");

    let by_name = |name: &str| {
        parsed
            .nodes
            .iter()
            .find(|node| node.name.as_deref() == Some(name))
            .expect(name)
    };

    assert_eq!(by_name("Constant1").inputs, 0, "Constant is a generator");
    assert_eq!(by_name("Grade1").inputs, 1, "Grade defaults to one input");
    assert_eq!(
        by_name("Blur1").inputs,
        1,
        "unknown classes default to one input"
    );
    assert_eq!(by_name("Viewer1").inputs, 1, "Viewer defaults to one input");
}

#[test]
fn add_user_knob_lines_become_custom_knobs() {
    tanova_nodes::ensure_linked();
    let parsed = parse_nk_script(
        r#"
Blur {
 inputs 1
 name Blur1
 addUserKnob {20 my_knob l "My Knob"}
}
"#,
    )
    .expect("script should parse");

    let node = parsed.nodes.first().expect("blur node");
    assert!(node.knobs.contains_key("my_knob"));
}

#[test]
fn viewer_block_is_a_real_node_and_becomes_the_render_target() {
    tanova_nodes::ensure_linked();
    let parsed = parse_nk_script(
        r#"
Constant {
 inputs 0
 name Constant1
}
Viewer {
 inputs 1
 name Viewer1
}
"#,
    )
    .expect("script should parse");

    let viewer = parsed.names["Viewer1"];
    assert_eq!(
        parsed.viewer_target,
        Some(viewer),
        "the Viewer node itself is the render target"
    );
    assert_eq!(
        parsed.stack_top,
        Some(viewer),
        "Viewer participates in the stack like a standard node"
    );
}

#[test]
fn viewer_is_added_to_the_dag_and_wired_to_its_input() {
    tanova_nodes::ensure_linked();
    let script = r#"
Constant {
 inputs 0
 name Constant1
 color {1 0 0 1}
 format_width 2
 format_height 2
}
Viewer {
 inputs 1
 name Viewer1
}
"#;

    let mut dag = Dag::new();
    let parsed = load_nk_into_dag(script, &mut dag).expect("Viewer should load");
    let constant = parsed.names["Constant1"];
    let viewer = parsed.names["Viewer1"];

    assert!(dag.node(viewer).is_some(), "Viewer must be in the DAG");
    let edge = dag
        .edges()
        .find(|edge| edge.to_node == viewer)
        .expect("Viewer input wire");
    assert_eq!(edge.from_node, constant);

    let raster = render(&mut dag, viewer);
    assert!((raster.sample_rgba(0, 0)[0] - 1.0).abs() < 1e-4);
}

#[test]
fn two_number_color_knobs_parse_as_floatx2() {
    tanova_nodes::ensure_linked();
    let parsed = parse_nk_script(
        r#"
Transform {
 inputs 1
 name Transform1
 translate {10 -5}
 center {3 4}
 rotate 45
 scale 2
}
"#,
    )
    .expect("script should parse");

    let node = parsed.nodes.first().expect("transform node");
    assert_eq!(
        node.knobs.get("translate"),
        Some(&KnobValue::Color([10.0, -5.0, 0.0, 1.0]))
    );
    assert_eq!(
        node.knobs.get("center"),
        Some(&KnobValue::Color([3.0, 4.0, 0.0, 1.0]))
    );
    assert_eq!(node.knobs.get("rotate"), Some(&KnobValue::Float(45.0)));
}

#[test]
fn read_knobs_parse_and_round_trip() {
    tanova_nodes::ensure_linked();
    let script = r#"
Read {
 inputs 0
 name Read1
 file "/tmp/plate.%04d.exr"
 first 10
 last 20
 missing_frames "black"
}
"#;

    let mut dag = Dag::new();
    let parsed = load_nk_into_dag(script, &mut dag).expect("Read should load");
    let id = parsed.names["Read1"];
    let node = dag.node(id).expect("Read node");
    assert_eq!(
        node.knobs.get("file"),
        Some(&KnobValue::Text("/tmp/plate.%04d.exr".to_string()))
    );
    assert_eq!(node.knobs.get("first"), Some(&KnobValue::Int(10)));
    assert_eq!(node.knobs.get("last"), Some(&KnobValue::Int(20)));
    assert_eq!(
        node.knobs.get("missing_frames"),
        Some(&KnobValue::Text("black".to_string()))
    );

    let exported = dag.export_to_tcl();
    assert!(exported.contains("Read {"));

    let mut round = Dag::new();
    let reparsed = load_nk_into_dag(&exported, &mut round).expect("export must reparse");
    let round_node = round
        .node(reparsed.names["Read1"])
        .expect("round-tripped Read node");
    assert_eq!(
        round_node.knobs.get("file"),
        Some(&KnobValue::Text("/tmp/plate.%04d.exr".to_string()))
    );
}
