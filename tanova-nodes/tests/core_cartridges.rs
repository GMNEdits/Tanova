//! Integration coverage for the three core VFX cartridges.

use std::sync::Arc;

use tanova_kernel::{
    Dag, Edge, EvalContext, KnobValue, MemoryGovernor, NodeId, NodeTrait, PortId, RenderEpoch,
    registered_cartridge_kinds,
};
use tanova_nodes::{GradeNode, Merge2Node, ensure_linked};
use tanova_render::{
    BBox, Channel, ChannelPlane, Format, Raster2D, TILE_SIZE, TileBuffer, TileCoord,
};

fn packet(raster: Raster2D) -> tanova_kernel::Packet {
    let mut packet = tanova_kernel::Packet::new();
    packet.insert(raster);
    packet
}

fn single_tile_raster(color: [f32; 4]) -> Raster2D {
    let bbox = BBox::new(0, 0, TILE_SIZE as i32, TILE_SIZE as i32);
    let mut raster = Raster2D::new(
        Format::new("tile", TILE_SIZE, TILE_SIZE, 1.0),
        bbox,
        Default::default(),
    );
    for (channel, value) in [
        (Channel::RgbaRed, color[0]),
        (Channel::RgbaGreen, color[1]),
        (Channel::RgbaBlue, color[2]),
        (Channel::RgbaAlpha, color[3]),
    ] {
        let mut plane = ChannelPlane::new();
        plane.insert(
            TileCoord::new(0, 0),
            Arc::new(TileBuffer::new_uniform(value)),
        );
        raster.insert_channel(channel, plane);
    }
    raster
}

#[test]
fn registry_links_every_core_cartridge() {
    ensure_linked();
    let kinds = registered_cartridge_kinds();
    for expected in ["Constant", "Dummy", "Grade", "Merge2"] {
        assert!(
            kinds.contains(&expected),
            "cartridge {expected} was not linked; got {kinds:?}"
        );
    }
}

#[test]
fn constant_grade_merge_dag_stays_zero_allocation() {
    ensure_linked();

    let mut dag = Dag::new();
    dag.add_node(NodeId(1), "Constant").unwrap();
    dag.add_node(NodeId(2), "Grade").unwrap();
    dag.add_node(NodeId(3), "Merge2").unwrap();

    // B = Constant, A = Grade(Constant), no mask.
    dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
        .unwrap();
    dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(3), PortId(0)))
        .unwrap();
    dag.connect_edge(Edge::new(NodeId(2), PortId(0), NodeId(3), PortId(1)))
        .unwrap();

    let governor = MemoryGovernor::new(64 * 1024 * 1024);
    let output = dag
        .evaluate_pull(
            NodeId(3),
            &EvalContext::new(),
            &governor,
            &RenderEpoch::new(),
            0,
        )
        .expect("DAG should evaluate");
    let raster = output.get::<Raster2D>().expect("raster");

    assert_eq!(raster.bbox, BBox::new(0, 0, 1920, 1080));
    assert_eq!(
        governor.allocated_bytes(),
        0,
        "a fully uniform Constant -> Grade -> Merge2 chain must not reserve dense tiles"
    );

    for channel in [
        Channel::RgbaRed,
        Channel::RgbaGreen,
        Channel::RgbaBlue,
        Channel::RgbaAlpha,
    ] {
        let plane = raster.channel(&channel).expect("rgba plane");
        assert!(!plane.is_empty(), "{channel:?} should cover the format");
        for (tile, buffer) in plane.iter() {
            assert!(
                buffer.is_uniform().is_some(),
                "{channel:?} tile {tile:?} unexpectedly allocated dense memory"
            );
        }
    }
}

#[test]
fn merge2_preserves_the_b_pipe_depth_plane_by_arc() {
    let mut background = single_tile_raster([0.0, 0.0, 0.0, 1.0]);
    let mut depth = ChannelPlane::new();
    depth.insert(
        TileCoord::new(0, 0),
        Arc::new(TileBuffer::new_uniform(42.0)),
    );
    background.insert_channel(Channel::DepthZ, depth);

    let depth_before = Arc::clone(
        background
            .channel(&Channel::DepthZ)
            .and_then(|plane| plane.get(&TileCoord::new(0, 0)))
            .expect("B depth tile"),
    );

    let foreground = single_tile_raster([0.5, 0.25, 0.0, 1.0]);
    let output = Merge2Node::new()
        .evaluate(
            &EvalContext::new(),
            &[packet(background), packet(foreground)],
        )
        .expect("merge should evaluate");
    let raster = output.get::<Raster2D>().expect("raster");

    let depth_after = raster
        .channel(&Channel::DepthZ)
        .and_then(|plane| plane.get(&TileCoord::new(0, 0)))
        .expect("output depth tile");

    assert!(
        Arc::ptr_eq(&depth_before, depth_after),
        "Merge2 must clone B's DepthZ plane, not copy it"
    );
    assert_eq!(raster.sample(&Channel::DepthZ, 0, 0), 42.0);
    assert_eq!(raster.sample(&Channel::RgbaRed, 0, 0), 0.5);
}

#[test]
fn grade_unpremult_keeps_alpha_and_never_produces_nan() {
    let mut knobs = tanova_nodes::grade::default_knobs();
    knobs.insert("unpremult", KnobValue::Bool(true));
    knobs.insert("gamma", KnobValue::Color([2.2; 4]));
    knobs.insert("black_clamp", KnobValue::Bool(false));
    let node = GradeNode::from_knobs(knobs);

    // Premultiplied half-transparent grey plus a negative super-black.
    let mut input = single_tile_raster([0.25, 0.25, 0.25, 0.5]);
    let mut red = ChannelPlane::new();
    red.insert(
        TileCoord::new(0, 0),
        Arc::new(TileBuffer::new_uniform(-0.5)),
    );
    input.insert_channel(Channel::RgbaRed, red);

    let output = node
        .evaluate(&EvalContext::new(), &[packet(input)])
        .expect("grade should evaluate");
    let raster = output.get::<Raster2D>().expect("raster");

    assert_eq!(raster.sample(&Channel::RgbaAlpha, 0, 0), 0.5);
    assert_eq!(raster.sample(&Channel::RgbaRed, 0, 0), -0.5);
    for channel in [
        Channel::RgbaRed,
        Channel::RgbaGreen,
        Channel::RgbaBlue,
        Channel::RgbaAlpha,
    ] {
        let value = raster.sample(&channel, 0, 0);
        assert!(value.is_finite(), "{channel:?} became {value}");
    }
}
