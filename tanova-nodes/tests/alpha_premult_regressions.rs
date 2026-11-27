//! Regression coverage for the Milestone 28 alpha / premultiplication audit.
//!
//! These tests pin the four behaviours that were wrong before the audit:
//! `Blur` filtering alpha, `Glow` leaving alpha alone, grade-style operators
//! never leaking RGB onto transparent pixels, `Merge2` multiply on two
//! super-blacks, and resamplers blending the outer half-pixel strip.

use std::sync::Arc;

use tanova_kernel::{EvalContext, KnobTable, KnobValue, NodeTrait, Packet};
use tanova_nodes::color_correct::ColorCorrectOp;
use tanova_nodes::{
    BlurNode, GlowNode, GradeNode, Merge2Node, PremultNode, ReformatNode, SaturationNode,
    UnpremultNode, ensure_linked,
};
use tanova_render::{
    BBox, Channel, ChannelPlane, EdgeMode, Format, GradeOp, Mat3x3, Raster2D, TileBuffer,
    TileCoord, transform_gathered_rgba,
};

const RGBA: [Channel; 4] = [
    Channel::RgbaRed,
    Channel::RgbaGreen,
    Channel::RgbaBlue,
    Channel::RgbaAlpha,
];

fn raster(width: u32, height: u32) -> Raster2D {
    let bbox = BBox::new(0, 0, width as i32, height as i32);
    let mut raster = Raster2D::new(
        Format::new("test", width, height, 1.0),
        bbox,
        EdgeMode::BlackOutside,
    );
    for channel in RGBA {
        let mut plane = ChannelPlane::new();
        plane.insert(TileCoord::new(0, 0), Arc::new(TileBuffer::new_uniform(0.0)));
        raster.insert_channel(channel, plane);
    }
    raster
}

fn set(raster: &mut Raster2D, x: i32, y: i32, rgba: [f32; 4]) {
    for (channel, value) in RGBA.into_iter().zip(rgba) {
        raster
            .set_pixel(channel, x, y, value, None)
            .expect("set pixel");
    }
}

fn packet(raster: Raster2D) -> Packet {
    let mut packet = Packet::new();
    packet.insert(raster);
    packet
}

fn evaluate(node: &dyn NodeTrait, inputs: &[Packet]) -> Raster2D {
    let output = node
        .evaluate(&EvalContext::new(), inputs)
        .expect("evaluate");
    output.get::<Raster2D>().expect("raster output").clone()
}

fn knobs_with(base: KnobTable, entries: &[(&str, KnobValue)]) -> KnobTable {
    let mut knobs = base;
    for (name, value) in entries {
        knobs.insert(*name, value.clone());
    }
    knobs
}

#[test]
fn blur_blurs_alpha_and_glow_preserves_alpha_on_antialiased_edge() {
    ensure_linked();

    // A 8x8 antialiased ramp: RGB and alpha carry the same coverage.
    let mut ramp = raster(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            let coverage = x as f32 / 7.0;
            set(&mut ramp, x, y, [coverage, coverage, coverage, coverage]);
        }
    }

    let blurred = evaluate(
        &BlurNode::from_knobs(knobs_with(
            tanova_nodes::blur::default_knobs(),
            &[("size", KnobValue::Float(2.0))],
        )),
        &[packet(ramp.clone())],
    );

    // Blur must filter alpha exactly like RGB: identical inputs give identical
    // outputs, including at the outer edge where energy bleeds in from inside.
    let edge = blurred.sample_rgba(0, 3);
    assert!(
        edge[3] > 0.0,
        "blurred alpha must pick up coverage across the edge: {edge:?}"
    );
    for y in 0..8 {
        for x in 0..8 {
            let rgba = blurred.sample_rgba(x, y);
            assert!(
                (rgba[3] - rgba[0]).abs() < 1e-6,
                "alpha must blur identically to RGB at {x},{y}: {rgba:?}"
            );
        }
    }
    // The interior ramp is no longer the original untouched alpha.
    assert!(
        (blurred.sample_rgba(7, 3)[3] - ramp.sample_rgba(7, 3)[3]).abs() > 1e-3,
        "alpha must actually change under Blur"
    );

    // Glow blooms RGB only. The checkerboard makes the highlight tile dense, so
    // the bloom path really runs through the separable blur.
    let mut base = raster(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            let value = if (x + y) % 2 == 0 { 0.6 } else { 0.4 };
            let alpha = (x + 1) as f32 / 8.0;
            set(&mut base, x, y, [value, value, value, alpha]);
        }
    }
    let glowed = evaluate(
        &GlowNode::from_knobs(knobs_with(
            tanova_nodes::glow::default_knobs(),
            &[("size", KnobValue::Float(1.5))],
        )),
        &[packet(base.clone())],
    );
    for y in 0..8 {
        for x in 0..8 {
            let before = base.sample_rgba(x, y);
            let after = glowed.sample_rgba(x, y);
            assert_eq!(
                after[3], before[3],
                "Glow must never alter base alpha at {x},{y}"
            );
        }
    }

    // Zero highlights with `effect_only`: the mask-aware composite must still
    // run, preserving alpha (mask 0 = original, mask 0.5 = halfway to black).
    let mut dark = raster(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            let alpha = (x + 1) as f32 / 8.0;
            set(&mut dark, x, y, [0.2, 0.2, 0.2, alpha]);
        }
    }
    let mut mask = raster(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            let value = if (x, y) == (2, 2) { 0.5 } else { 0.0 };
            set(&mut mask, x, y, [value, value, value, value]);
        }
    }
    let glowed = evaluate(
        &GlowNode::from_knobs(knobs_with(
            tanova_nodes::glow::default_knobs(),
            &[
                ("effect_only", KnobValue::Bool(true)),
                ("size", KnobValue::Float(1.0)),
            ],
        )),
        &[packet(dark.clone()), packet(mask)],
    );
    let masked = glowed.sample_rgba(2, 2);
    let unmasked = glowed.sample_rgba(3, 3);
    let source = dark.sample_rgba(2, 2);
    assert_eq!(unmasked, dark.sample_rgba(3, 3), "mask 0 returns the input");
    assert_eq!(masked[3], source[3], "effect_only preserves alpha");
    assert!(
        (masked[0] - source[0] * 0.5).abs() < 1e-6,
        "mask 0.5 blends halfway toward black: {masked:?}"
    );
}

#[test]
fn grade_color_correct_and_saturation_unpremult_never_leak_rgb_at_zero_alpha_or_drift_at_endpoints()
{
    // Grade: transparent pixels must stay transparent even with a positive
    // offset / lift, and the mix endpoints must be exact.
    let grade = GradeOp {
        unpremult: true,
        offset: [0.5, 0.5, 0.5, 0.0],
        lift: [0.25, 0.25, 0.25, 0.0],
        mix: 1.0,
        ..GradeOp::default()
    };
    let transparent = grade.apply_pixel([0.0, 0.0, 0.0, 0.0], 1.0);
    assert_eq!(
        transparent,
        [0.0, 0.0, 0.0, 0.0],
        "offset/lift must not leak RGB onto alpha 0"
    );

    let original = [0.123_456_7, 0.765_432_1, 0.333_333_3, 0.5];
    let mut zero_mix = grade.clone();
    zero_mix.mix = 0.0;
    assert_eq!(
        zero_mix.apply_pixel(original, 1.0),
        original,
        "mix 0 must return the original bits"
    );
    assert_eq!(
        grade.apply_pixel(original, 0.0),
        original,
        "mask 0 must return the original bits"
    );

    // ColorCorrect has the same contract.
    let mut knobs = tanova_nodes::color_correct::default_knobs();
    knobs.insert("unpremult", KnobValue::Bool(true));
    knobs.insert("offset", KnobValue::Float(0.5));
    let color_correct = ColorCorrectOp::from_knobs(&knobs);
    assert_eq!(
        color_correct.apply_pixel([0.0, 0.0, 0.0, 0.0], 1.0),
        [0.0, 0.0, 0.0, 0.0],
        "ColorCorrect offset must not leak onto alpha 0"
    );
    assert_eq!(
        color_correct.apply_pixel(original, 0.0),
        original,
        "ColorCorrect mask 0 must return the original bits"
    );

    // Saturation with unpremult must not divide by a tiny alpha into inf/NaN.
    let saturation = SaturationNode::from_knobs(knobs_with(
        tanova_nodes::saturation::default_knobs(),
        &[
            ("saturation", KnobValue::Float(0.0)),
            ("unpremult", KnobValue::Bool(true)),
        ],
    ));
    let mut tiny = raster(8, 8);
    set(&mut tiny, 1, 1, [1e-20, 2e-20, 3e-20, 1e-20]);
    set(&mut tiny, 2, 2, [0.0, 0.0, 0.0, 0.0]);
    let saturated = evaluate(&saturation, &[packet(tiny)]);
    let tiny_out = saturated.sample_rgba(1, 1);
    assert!(
        tiny_out.iter().all(|value| value.is_finite()),
        "saturation produced non-finite output for tiny alpha: {tiny_out:?}"
    );
    assert_eq!(tiny_out[3], 1e-20, "saturation must preserve tiny alpha");
    let zero_out = saturated.sample_rgba(2, 2);
    assert_eq!(
        zero_out, [0.0; 4],
        "alpha 0 stays fully transparent after unpremult saturation"
    );

    // Node-level Grade endpoint check keeps the same policy: mix 0 returns the
    // original bits exactly, even with a non-trivial offset configured.
    let mut graded_input = raster(8, 8);
    set(&mut graded_input, 3, 3, [0.125, 0.25, 0.5, 0.75]);
    let graded = evaluate(
        &GradeNode::from_knobs(knobs_with(
            tanova_nodes::grade::default_knobs(),
            &[
                ("unpremult", KnobValue::Bool(true)),
                ("offset", KnobValue::Color([0.5, 0.5, 0.5, 0.0])),
                ("mix", KnobValue::Float(0.0)),
            ],
        )),
        &[packet(graded_input)],
    );
    assert_eq!(graded.sample_rgba(3, 3), [0.125, 0.25, 0.5, 0.75]);
}

#[test]
fn merge_multiply_two_negatives_and_missing_alpha_consistency() {
    use tanova_render::ops::{MergeOp, merge_pixel};

    // Two super-blacks must stay negative instead of flipping positive.
    let merged = merge_pixel(
        MergeOp::Multiply,
        [-0.5, -0.25, 0.5, 1.0],
        [-0.5, 0.25, 0.5, 1.0],
        1.0,
        1.0,
    );
    assert_eq!(
        merged,
        [-0.5, -0.0625, 0.25, 1.0],
        "two negatives keep the foreground, mixed signs multiply"
    );

    // Exact endpoints.
    let background = [0.1, 0.2, 0.3, 0.4];
    let foreground = [0.9, 0.8, 0.7, 0.6];
    assert_eq!(
        merge_pixel(MergeOp::Over, background, foreground, 1.0, 0.0),
        background
    );
    assert_eq!(
        merge_pixel(MergeOp::Over, background, foreground, 1.0, 1.0),
        MergeOp::Over.apply(foreground, background)
    );

    // Merge2 with a raster missing its alpha plane treats it as opaque inside
    // the bbox and transparent outside.
    let mut missing = raster(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            set(&mut missing, x, y, [0.5, 0.5, 0.5, 0.0]);
        }
    }
    missing.channels.shift_remove(&Channel::RgbaAlpha);
    assert_eq!(
        missing.sample_rgba(3, 3),
        [0.5, 0.5, 0.5, 1.0],
        "missing alpha is opaque inside the bbox"
    );

    let mut background = raster(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            set(&mut background, x, y, [0.25, 0.25, 0.25, 1.0]);
        }
    }
    let mut merge_knobs = tanova_nodes::merge::default_knobs();
    merge_knobs.insert("operation", KnobValue::Text("over".into()));
    let merged = evaluate(
        &Merge2Node::from_knobs(merge_knobs),
        &[packet(background), packet(missing)],
    );
    assert_eq!(
        merged.sample_rgba(3, 3),
        [0.5, 0.5, 0.5, 1.0],
        "missing alpha contributes as opaque inside the bbox"
    );
}

#[test]
fn resamplers_blend_outer_half_pixel_strip_consistently_and_unpremult_round_trips_tiny_alpha() {
    // A gathered 4x4 white region translated by half a pixel: output (0,0)
    // lands at -0.5, which is half a pixel before the first center and must
    // blend 0.5 against transparent black in every channel.
    let gathered = vec![1.0f32; 4 * 4 * 4];
    let half = transform_gathered_rgba(&gathered, 4, 4, &Mat3x3::translate(-0.5, 0.0));
    for (channel, value) in half[..4].iter().enumerate() {
        assert!(
            (value - 0.5).abs() < 1e-6,
            "half-pixel strip must blend toward zero, channel {channel}: {value}"
        );
    }
    // One pixel in, the sample is exact again.
    for value in &half[4..8] {
        assert!((value - 1.0).abs() < 1e-6);
    }

    // Reformat has the same boundary policy: at the last destination pixel the
    // source sample is 3.5, halfway past the final source center.
    let mut source = raster(4, 4);
    for y in 0..4 {
        for x in 0..4 {
            set(&mut source, x, y, [1.0, 1.0, 1.0, 1.0]);
        }
    }
    let reformatted = evaluate(
        &ReformatNode::from_knobs(knobs_with(
            tanova_nodes::reformat::default_knobs(),
            &[
                ("format_width", KnobValue::Int(8)),
                ("format_height", KnobValue::Int(8)),
                ("resize", KnobValue::Text("distort".into())),
            ],
        )),
        &[packet(source)],
    );
    let corner = reformatted.sample_rgba(7, 7);
    assert!(
        (corner[0] - 0.25).abs() < 1e-6 && (corner[3] - 0.25).abs() < 1e-6,
        "reformat must fade the outer half-pixel strip in RGB and alpha: {corner:?}"
    );

    // Transform and CornerPin share `transform_gathered_rgba`, so the gathered
    // half-pixel case above covers their boundary policy; Unpremult -> Premult
    // round-trips a tiny signed alpha without an absolute cutoff swallowing the
    // correction.
    for alpha in [1e-6f32, -1e-6] {
        let mut tiny = raster(8, 8);
        let premultiplied = [alpha, 2.0 * alpha, 3.0 * alpha, alpha];
        set(&mut tiny, 3, 3, premultiplied);
        let unpremultiplied = evaluate(&UnpremultNode::new(), &[packet(tiny)]);
        let straight = unpremultiplied.sample_rgba(3, 3);
        assert!(
            (straight[0] - 1.0).abs() < 1e-3
                && (straight[1] - 2.0).abs() < 1e-3
                && (straight[2] - 3.0).abs() < 1e-3,
            "unpremult must divide through tiny alpha {alpha}: {straight:?}"
        );
        let round_trip = evaluate(&PremultNode::new(), &[packet(unpremultiplied)]);
        let back = round_trip.sample_rgba(3, 3);
        for channel in 0..4 {
            assert!(
                (back[channel] - premultiplied[channel]).abs() < 1e-9,
                "premult/unpremult round trip drifted at alpha {alpha}: {back:?}"
            );
        }
    }
}
