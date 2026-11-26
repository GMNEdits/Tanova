use tanova_kernel::{
    Dag, EvalContext, KnobValue, MemoryGovernor, NodeId, PortId, ProjectFormat, ProxyScale,
    RenderEpoch,
};
use tanova_render::{BBox, Channel, Raster2D, TileCoord};

#[test]
fn proxy_scale_scales_generators_filters_and_spatial_nodes_consistently() {
    tanova_nodes::ensure_linked();
    let mut dag = Dag::new();
    for (id, kind) in [
        (1, "CheckerBoard2"),
        (2, "Defocus"),
        (3, "Blur"),
        (4, "Glow"),
        (5, "Constant"),
        (6, "Transform"),
        (7, "CornerPin2D"),
        (8, "Transform"),
    ] {
        dag.add_node(NodeId(id), kind).unwrap();
    }
    for (from, to) in [(1, 2), (1, 3), (1, 4), (5, 6), (6, 7), (6, 8)] {
        dag.connect(NodeId(from), NodeId(to), PortId(0)).unwrap();
    }
    for (id, knob, value) in [
        (1, "boxsize", KnobValue::Int(64)),
        (2, "defocus", KnobValue::Float(8.0)),
        (3, "size", KnobValue::Float(12.0)),
        (4, "size", KnobValue::Float(12.0)),
        (4, "tolerance", KnobValue::Float(0.2)),
        (5, "color", KnobValue::Color([0.25, 0.5, 0.75, 1.0])),
        (6, "translate", KnobValue::Color([64.0, 32.0, 0.0, 0.0])),
        (8, "translate", KnobValue::Color([-64.0, -32.0, 0.0, 0.0])),
        (7, "to1", KnobValue::Color([16.0, 8.0, 0.0, 0.0])),
        (7, "to2", KnobValue::Color([272.0, 8.0, 0.0, 0.0])),
        (7, "to3", KnobValue::Color([272.0, 264.0, 0.0, 0.0])),
        (7, "to4", KnobValue::Color([16.0, 264.0, 0.0, 0.0])),
    ] {
        dag.set_knob(NodeId(id), knob, value).unwrap();
    }
    let original_script = dag.export_to_tcl();
    let governor = MemoryGovernor::new(128 * 1024 * 1024);
    let epoch = RenderEpoch::new();
    let mut ctx = EvalContext::new();
    ctx.insert(ProjectFormat::new(256, 256));
    let mut evaluate = |id, scale| {
        ctx.insert(ProxyScale(scale));
        dag.evaluate_pull(NodeId(id), &ctx, &governor, &epoch, epoch.current())
            .unwrap()
            .get::<Raster2D>()
            .unwrap()
            .clone()
    };
    for (id, radius) in [(1, 0), (2, 8), (3, 12), (4, 12)] {
        let full = evaluate(id, 1.0);
        let half = evaluate(id, 0.5);
        assert_eq!((half.format.width, half.format.height), (128, 128));
        assert_eq!(full.bbox, BBox::new(0, 0, 256, 256).pad(radius));
        assert_eq!(half.bbox, BBox::new(0, 0, 128, 128).pad(radius / 2));
        // Compare plateau samples well clear of filter discretization at edges.
        for (x, y) in [(32, 32), (96, 32), (160, 160)] {
            for (a, b) in full
                .sample_rgba(x, y)
                .into_iter()
                .zip(half.sample_rgba(x / 2, y / 2))
            {
                assert!((a - b).abs() < 1e-5, "node {id} at ({x}, {y}): {a} != {b}");
            }
        }
    }
    let full = evaluate(7, 1.0);
    let half = evaluate(7, 0.5);
    assert_eq!((half.format.width, half.format.height), (128, 128));
    assert_eq!(half.format.logical_size(0.5), (256, 256));
    for (x, y, alpha) in [(72, 96, 0.0), (96, 32, 0.0), (96, 96, 1.0)] {
        assert_eq!(full.sample_rgba(x, y)[3], alpha);
        assert_eq!(full.sample_rgba(x, y), half.sample_rgba(x / 2, y / 2));
    }
    let constant = evaluate(5, 0.5);
    let chained = evaluate(8, 0.5);
    let tile = TileCoord::new(0, 0);
    let red = |raster: &Raster2D| {
        raster
            .channel(&Channel::RgbaRed)
            .unwrap()
            .get(&tile)
            .unwrap()
            .clone()
    };
    assert!(std::sync::Arc::ptr_eq(&red(&constant), &red(&chained)));
    assert_eq!(red(&constant).governed_bytes(), 0);
    assert_eq!(
        original_script,
        dag.export_to_tcl(),
        "evaluation must not rewrite knobs"
    );
}

#[test]
fn quarter_proxy_keeps_odd_logical_sizes_through_ramp_crop_reformat_and_merge() {
    tanova_nodes::ensure_linked();
    let mut dag = Dag::new();
    for (id, kind) in [(1, "Ramp"), (2, "Crop"), (3, "Reformat"), (4, "Merge2")] {
        dag.add_node(NodeId(id), kind).unwrap();
    }
    dag.connect(NodeId(1), NodeId(2), PortId(0)).unwrap();
    dag.connect(NodeId(2), NodeId(3), PortId(0)).unwrap();
    dag.connect(NodeId(3), NodeId(4), PortId(0)).unwrap();
    dag.connect(NodeId(3), NodeId(4), PortId(1)).unwrap();
    dag.set_knob(NodeId(1), "p1", KnobValue::Color([64.0, 0.0, 0.0, 0.0]))
        .unwrap();
    dag.set_knob(NodeId(2), "reformat", KnobValue::Bool(true))
        .unwrap();
    let mut ctx = EvalContext::new();
    ctx.insert(ProjectFormat::new(65, 35));
    ctx.insert(ProxyScale(0.25));
    let governor = MemoryGovernor::new(16 * 1024 * 1024);
    let epoch = RenderEpoch::new();
    for id in 1..=4 {
        let packet = dag
            .evaluate_pull(NodeId(id), &ctx, &governor, &epoch, 0)
            .unwrap();
        let raster = packet.get::<Raster2D>().unwrap();
        assert_eq!(raster.bbox, BBox::new(0, 0, 16, 9), "node {id}");
        assert_eq!(raster.format.logical_size(0.25), (65, 35), "node {id}");
        assert_eq!(
            raster.sample_rgba(7, 3),
            [30.0 / 64.0, 30.0 / 64.0, 30.0 / 64.0, 1.0]
        );
    }
}
