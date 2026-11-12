//! Small helpers shared by the core cartridges.
//!
//! Every helper reads from the live [`KnobTable`] injected into `EvalContext`,
//! falling back to a caller-supplied default when the knob is missing or has the
//! wrong type.

use tanova_kernel::{KnobTable, KnobValue, Packet};
use tanova_render::Raster2D;

/// Reads an RGBA color knob.
pub fn color_knob(knobs: &KnobTable, name: &str, default: [f32; 4]) -> [f32; 4] {
    match knobs.get(name) {
        Some(KnobValue::Color(color)) => *color,
        _ => default,
    }
}

/// Reads a boolean knob.
pub fn bool_knob(knobs: &KnobTable, name: &str, default: bool) -> bool {
    match knobs.get(name) {
        Some(KnobValue::Bool(value)) => *value,
        _ => default,
    }
}

/// Reads a float knob.
pub fn float_knob(knobs: &KnobTable, name: &str, default: f32) -> f32 {
    match knobs.get(name) {
        Some(KnobValue::Float(value)) => *value as f32,
        Some(KnobValue::Int(value)) => *value as f32,
        _ => default,
    }
}

/// Reads an integer knob.
pub fn int_knob(knobs: &KnobTable, name: &str, default: i64) -> i64 {
    match knobs.get(name) {
        Some(KnobValue::Int(value)) => *value,
        Some(KnobValue::Float(value)) => *value as i64,
        _ => default,
    }
}

/// Reads a text knob.
pub fn text_knob(knobs: &KnobTable, name: &str, default: &str) -> String {
    match knobs.get(name) {
        Some(KnobValue::Text(value)) => value.clone(),
        _ => default.to_string(),
    }
}

/// Borrows the raster on `port`, if the upstream packet carried one.
pub fn raster_input(inputs: &[Packet], port: usize) -> Option<&Raster2D> {
    inputs.get(port).and_then(|packet| packet.get::<Raster2D>())
}

/// Formats an RGBA color for TCL serialization.
pub fn format_color(color: [f32; 4]) -> String {
    format!("{} {} {} {}", color[0], color[1], color[2], color[3])
}

/// Formats a float knob for TCL serialization.
pub fn format_float(value: f32) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}
