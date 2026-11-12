//! Auto-generated properties bin for the selected node.
//!
//! The panel is stateless: it reads the UI-side [`GraphMirror`] and returns
//! `KernelMessage`s. Every edit carries the previous value so the kernel's undo
//! stack stays mechanical.

use egui::{Color32, ComboBox, DragValue, Slider, TextEdit, Ui};
use tanova_kernel::{KernelMessage, KnobValue, NodeId, UndoableCommand};

use crate::canvas::GraphMirror;

const VIEWER: Color32 = Color32::from_rgb(255, 190, 40);

const OPERATIONS: &[&str] = &[
    "over", "under", "plus", "from", "minus", "multiply", "stencil", "mask",
];
const BBOX_MODES: &[&str] = &["union", "B", "A", "intersection"];

/// Draws controls for every knob on the selected node.
pub struct PropertiesPanel;

impl PropertiesPanel {
    /// Renders the panel and returns the commands the user requested.
    pub fn show(
        ui: &mut Ui,
        graph: &GraphMirror,
        selected: Option<NodeId>,
        viewer: Option<NodeId>,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();

        let Some(node_id) = selected else {
            ui.label("No node selected");
            ui.separator();
            ui.weak("Select a node on the canvas, then press 1 to view it.");
            return messages;
        };
        let Some(node) = graph.node(node_id) else {
            ui.label(format!("Node #{} is not in the graph", node_id.0));
            return messages;
        };

        ui.horizontal(|ui| {
            ui.heading(format!("{} #{}", node.kind, node_id.0));
            ui.separator();
            if ui.button("View (1)").clicked() {
                messages.push(KernelMessage::SetViewerNode(Some(node_id)));
            }
            if viewer == Some(node_id) {
                ui.colored_label(VIEWER, "VIEWER 1");
            }
        });
        ui.separator();

        egui::ScrollArea::vertical().show(ui, |ui| {
            // Scope widget IDs by node id so switching selection between two
            // nodes of the same kind never leaks egui widget focus or drag
            // state.
            ui.push_id(node_id, |ui| {
                for (name, value) in node.knobs.iter() {
                    let previous = value.clone();
                    let mut next = previous.clone();
                    let mut changed = false;

                    ui.horizontal(|ui| {
                        ui.label(name);
                        match value {
                            KnobValue::Float(value) => {
                                let mut float = *value as f32;
                                let response = if name == "mix" {
                                    ui.add(Slider::new(&mut float, 0.0..=1.0))
                                } else {
                                    ui.add(DragValue::new(&mut float).speed(0.01))
                                };
                                if response.changed() {
                                    next = KnobValue::Float(float as f64);
                                    changed = true;
                                }
                            }
                            KnobValue::Int(value) => {
                                let mut integer = *value;
                                if ui.add(DragValue::new(&mut integer).speed(1.0)).changed() {
                                    next = KnobValue::Int(integer);
                                    changed = true;
                                }
                            }
                            KnobValue::Bool(value) => {
                                let mut boolean = *value;
                                if ui.checkbox(&mut boolean, "").changed() {
                                    next = KnobValue::Bool(boolean);
                                    changed = true;
                                }
                            }
                            KnobValue::Text(text) => {
                                let mut edited = text.clone();
                                let mut combo_changed = false;
                                let presets = match name {
                                    "operation" => Some(OPERATIONS),
                                    "bbox" => Some(BBOX_MODES),
                                    _ => None,
                                };
                                if let Some(presets) = presets {
                                    ComboBox::from_id_salt((node_id.0, name))
                                        .selected_text(edited.clone())
                                        .show_ui(ui, |ui| {
                                            for preset in presets {
                                                if ui
                                                    .selectable_value(
                                                        &mut edited,
                                                        (*preset).to_string(),
                                                        *preset,
                                                    )
                                                    .changed()
                                                {
                                                    combo_changed = true;
                                                }
                                            }
                                        });
                                }
                                let edit =
                                    ui.add(TextEdit::singleline(&mut edited).desired_width(150.0));
                                if combo_changed || edit.changed() {
                                    next = KnobValue::Text(edited);
                                    changed = true;
                                }
                            }
                            KnobValue::Color(channels)
                                if name == "translate" || name == "center" =>
                            {
                                // 2D spatial knobs keep `Color([x, y, z, w])`
                                // storage for cartridge compatibility but edit as
                                // coordinates, never as RGBA.
                                let mut color = *channels;
                                ui.label("X");
                                if ui.add(DragValue::new(&mut color[0]).speed(1.0)).changed() {
                                    changed = true;
                                }
                                ui.label("Y");
                                if ui.add(DragValue::new(&mut color[1]).speed(1.0)).changed() {
                                    changed = true;
                                }
                                if changed {
                                    next = KnobValue::Color(color);
                                }
                            }
                            KnobValue::Color(channels) => {
                                let mut color = *channels;
                                let mut srgba = Color32::from_rgba_unmultiplied(
                                    to_u8(color[0]),
                                    to_u8(color[1]),
                                    to_u8(color[2]),
                                    to_u8(color[3]),
                                );
                                if ui.color_edit_button_srgba(&mut srgba).changed() {
                                    color = [
                                        srgba.r() as f32 / 255.0,
                                        srgba.g() as f32 / 255.0,
                                        srgba.b() as f32 / 255.0,
                                        srgba.a() as f32 / 255.0,
                                    ];
                                    changed = true;
                                }
                                for (label, channel) in
                                    ["R", "G", "B", "A"].iter().zip(color.iter_mut())
                                {
                                    ui.label(*label);
                                    if ui.add(DragValue::new(channel).speed(0.01)).changed() {
                                        changed = true;
                                    }
                                }
                                if changed {
                                    next = KnobValue::Color(color);
                                }
                            }
                        }
                    });

                    if changed && next != previous {
                        messages.push(KernelMessage::Execute(UndoableCommand::SetKnob {
                            node: node_id,
                            knob: name.to_string(),
                            value: next,
                            previous,
                        }));
                    }
                }
            });
        });

        messages
    }
}

fn to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}
