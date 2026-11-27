//! Auto-generated properties bin for the selected node.
//!
//! The panel is stateless: it reads the UI-side [`GraphMirror`] and returns
//! `KernelMessage`s. Every edit carries the previous value so the kernel's undo
//! stack stays mechanical.

use egui::{Color32, ComboBox, DragValue, RichText, TextEdit, Ui};
use tanova_kernel::{
    AnimationCurve, InterpMode, KernelMessage, KnobValue, NodeId, UndoableCommand,
};

use crate::canvas::{CanvasNode, GraphMirror, corner_pin_format_knobs, infer_upstream_format};
use crate::theme::{StudioTheme, node_category, palette};

/// A request from the properties panel to open the EXR file browse modal
/// for a `Read` or `Write` node's `file` knob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBrowseRequest {
    pub node_id: NodeId,
    pub node_kind: String,
    pub current_path: String,
}

/// One knob edit session, latched in `egui` memory for as long as the artist is
/// typing into or dragging one knob.
///
/// A numeric field commits a value on every keystroke (`'0'` in `0.5` is a real
/// `0.0` for a moment), so the session remembers the value the artist actually
/// started from. Every `SetKnob` emitted during the session reports that value
/// as `previous`, which keeps the whole edit a single undo step.
#[derive(Clone, Debug)]
struct ActiveKnobEdit {
    node: NodeId,
    knob: String,
    initial_value: KnobValue,
}

/// `egui` memory key for the live [`ActiveKnobEdit`].
fn active_knob_edit_id() -> egui::Id {
    egui::Id::unique("tanova_active_knob_edit")
}

#[cfg(test)]
thread_local! {
    // Widget rects captured by the last `PropertiesPanel::show` call. Tests
    // use these to aim pointer events at a named knob without hard-coding the
    // panel's layout.
    static KNOB_WIDGET_RECTS: std::cell::RefCell<
        std::collections::HashMap<(NodeId, String), egui::Rect>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

const OPERATIONS: &[&str] = &[
    "over", "under", "plus", "from", "minus", "multiply", "stencil", "mask",
];
const BBOX_MODES: &[&str] = &["union", "B", "A", "intersection"];
/// Channel export modes for the `Write` node's `channels` knob.
const CHANNELS_MODES: &[&str] = &["rgba", "all"];
/// Alpha plane source modes for the `Premult` and `Unpremult` nodes' `alpha` knob.
const ALPHA_MODES: &[&str] = &["rgba.alpha", "depth.Z", "none"];
/// Channel modes for the `Invert` node's `channels` knob.
const INVERT_CHANNELS: &[&str] = &["rgb", "rgba", "alpha", "none"];
/// Luminance modes for the `Saturation` node's `mode` knob.
const SATURATION_MODES: &[&str] = &["Rec709", "Average", "Maximum"];
/// Resize modes for the `Reformat` node's `resize` knob.
const REFORMAT_MODES: &[&str] = &["width", "height", "fit", "fill", "distort", "none"];
/// Channel selectors for the `Shuffle` node's `red` / `green` / `blue` / `alpha`.
const SHUFFLE_SELECTORS: &[&str] = &["red", "green", "blue", "alpha", "depth.Z", "black", "white"];
/// Tint presets for a `BackdropNode`'s `color` knob.
const BACKDROP_COLORS: &[&str] = &[
    "slate", "blue", "teal", "amber", "purple", "crimson", "green",
];
/// Ramp interpolation modes for the `Ramp` node's `ramp_type` knob.
const RAMP_MODES: &[&str] = &["linear", "smooth", "plinear"];
/// Missing-frame policies for the `Read` node's `missing_frames` knob.
const MISSING_FRAMES_MODES: &[&str] = &["error", "black", "nearest"];
/// Nuke's keyframe blue: the knob has a key on the current frame.
const KEYFRAME_BLUE: Color32 = Color32::from_rgb(80, 175, 255);
/// Interpolated teal: the knob is animated but has no key on this frame.
const INTERPOLATED_TEAL: Color32 = Color32::from_rgb(70, 210, 190);
/// Default knob-label color.
const KNOB_LABEL: Color32 = Color32::from_rgb(200, 205, 215);
/// Travel of the `mix` slider rail, in points.
///
/// Wide enough that a 0..1 blend can be set precisely inside the narrow
/// properties column, and fixed so the rail never reflows mid-drag.
const MIX_SLIDER_WIDTH: f32 = 140.0;

/// Draws controls for every knob on the selected node.
pub struct PropertiesPanel {
    /// Active studio theme, synced from `NodeCanvasState.theme`.
    pub theme: StudioTheme,
    /// Current timeline frame, synced from `TanovaApp::timeline.current`.
    ///
    /// Drives the `[Key]` controls: a key is added or removed at this frame.
    pub current_frame: f64,
    /// A `Write` node waiting to render its sequence to disk, set when the
    /// artist clicks `[Render Sequence to Disk]` and consumed by the app.
    pub pending_write_render: Option<NodeId>,
    /// A `Read`/`Write` node waiting for an EXR file browse, set when the
    /// artist clicks `Browse...` on the `file` knob and consumed by the app.
    pub pending_file_browse: Option<FileBrowseRequest>,
    /// `true` while any knob widget owns an active pointer drag.
    ///
    /// The app uses this to keep stale kernel echoes from overwriting the
    /// optimistic mirror value mid-drag.
    knob_drag_active: bool,
}

impl Default for PropertiesPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl PropertiesPanel {
    /// Creates an empty panel at frame 1.0.
    pub fn new() -> Self {
        Self {
            theme: StudioTheme::default(),
            current_frame: 1.0,
            pending_write_render: None,
            pending_file_browse: None,
            knob_drag_active: false,
        }
    }
    /// Consumes and returns the pending write-render node, if any.
    pub fn take_pending_write_render(&mut self) -> Option<NodeId> {
        self.pending_write_render.take()
    }

    /// `true` while a knob widget is being dragged with the pointer.
    ///
    /// The app consults this so a kernel echo of an older drag step cannot
    /// snap the slider back while the artist is still holding the mouse.
    pub fn is_dragging_knob(&self) -> bool {
        self.knob_drag_active
    }

    /// Consumes and returns the pending file-browse request, if any.
    pub fn take_pending_file_browse(&mut self) -> Option<FileBrowseRequest> {
        self.pending_file_browse.take()
    }

    /// The value the panel currently shows for `knob_name` on `node`.
    ///
    /// Thin wrapper over [`effective_knob_value`] that uses the panel's
    /// `current_frame`, which the app syncs from the timeline every frame.
    pub fn effective_knob_value(
        &self,
        node: &CanvasNode,
        knob_name: &str,
        base_val: &KnobValue,
    ) -> KnobValue {
        effective_knob_value(node, knob_name, base_val, self.current_frame)
    }

    /// Renders the panel and returns the commands the user requested.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        graph: &GraphMirror,
        selected: Option<NodeId>,
        _viewer: Option<NodeId>,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();

        #[cfg(test)]
        KNOB_WIDGET_RECTS.with(|rects| rects.borrow_mut().clear());

        // Recompute each pass: a drag ends the moment the pointer lifts.
        self.knob_drag_active = false;

        // Comfortable slider travel inside the 2-column properties grid.
        ui.spacing_mut().slider_width = MIX_SLIDER_WIDTH;

        let pal = palette(self.theme);

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

        // Node category header banner: full-width rounded band filled with the
        // category color. Left: kind + id. Right: bypass toggle or VIEWER tag.
        let cat = node_category(&node.kind);
        let banner_color = if node.disabled {
            pal.category_color(cat).linear_multiply(0.7)
        } else {
            pal.category_color(cat)
        };

        egui::Frame::new()
            .fill(banner_color)
            .corner_radius(pal.card_corner_radius)
            .inner_margin(egui::Margin::symmetric(12, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{}  #{}", node.kind, node_id.0))
                            .strong()
                            .color(Color32::WHITE),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if node.kind != "Viewer"
                            && node.kind != "BackdropNode"
                            && node.kind != "Dot"
                        {
                            let bypass_label = if node.disabled {
                                "[Bypassed]"
                            } else {
                                "[Bypass (D)]"
                            };
                            if ui.button(bypass_label).clicked() {
                                messages.push(KernelMessage::Execute(
                                    UndoableCommand::SetDisabled {
                                        node: node_id,
                                        disabled: !node.disabled,
                                    },
                                ));
                            }
                        } else {
                            ui.colored_label(pal.viewer_badge_accent, "VIEWER");
                        }
                    });
                });
            });
        ui.add_space(6.0);

        // A session belongs to the node that was being edited. Switching
        // selection ends it, so a later edit never inherits a stale origin.
        ui.ctx().data_mut(|data| {
            if let Some(active) = data.get_temp::<ActiveKnobEdit>(active_knob_edit_id()) {
                if active.node != node_id {
                    data.remove::<ActiveKnobEdit>(active_knob_edit_id());
                }
            }
        });

        // A `Write` node offers a one-click render to disk. Kept above the
        // scroll area so the action never scrolls out of reach.
        if node.kind == "Write" {
            ui.separator();
            if ui.button("[Render Sequence to Disk]").clicked() {
                self.pending_write_render = Some(node_id);
            }
        }

        // The banner stays pinned above this scroll area, so the node identity
        // and bypass control are always reachable while the grid scrolls.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            // Slider drags must never be mistaken for a scroll gesture: a
            // horizontal knob drag inside the bin would stall the moment the
            // pointer left the thin rail. The wheel and scrollbar still work.
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .show(ui, |ui| {
                egui::Grid::new(("tanova_props_grid", node_id.0))
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (name, value) in node.knobs.iter() {
                            // Resolve animation at the current frame so widgets show
                            // the live interpolated value instead of the static base.
                            let effective =
                                effective_knob_value(node, name, value, self.current_frame);
                            // Value the artist saw before this frame. It becomes the
                            // session origin if this frame starts a new edit.
                            let pre_edit_value = effective.clone();
                            let mut next = pre_edit_value.clone();
                            let mut changed = false;
                            let mut row_active = false;
                            let mut user_edited = false;
                            let mut drag_started_here = false;
                            let mut widget_rect: Option<egui::Rect> = None;
                            let mut key_toggle = false;

                            // Pointer state sampled before any widget runs, so
                            // the interaction check below never borrows `ui`.
                            let pointer_down = ui.input(|input| input.pointer.any_down());
                            let repaint_ctx = ui.ctx().clone();

                            // Tracks the knob's interactive area and whether it
                            // still owns keyboard focus or an active pointer drag.
                            let mut note = |response: &egui::Response| {
                                widget_rect = Some(
                                    widget_rect
                                        .map_or(response.rect, |rect| rect.union(response.rect)),
                                );
                                row_active |= response.has_focus() || response.dragged();
                                // Only real pointer/keyboard interaction may
                                // author a knob edit. Merely having focus (or a
                                // frame change under the playhead) must never
                                // auto-key a value.
                                let edited = response.dragged()
                                    || response.drag_started()
                                    || response.drag_stopped()
                                    || response.lost_focus()
                                    || response.has_focus()
                                    || (pointer_down && response.hovered());
                                user_edited |= edited;
                                if response.dragged() {
                                    drag_started_here = true;
                                    // Keep the thumb glued to the cursor even
                                    // when the rest of the app is idle.
                                    repaint_ctx.request_repaint();
                                }
                            };

                            // Animation state drives both the label tint and the
                            // `[Key]` / `[+K]` toggle. Mirrors Nuke: blue means a key
                            // sits on this frame, teal means interpolated.
                            let animatable = matches!(
                                value,
                                KnobValue::Float(_) | KnobValue::Int(_) | KnobValue::Color(_)
                            );
                            let active_curves = node.curves.get(name);
                            let is_animated = animatable
                                && active_curves.is_some_and(|curves| {
                                    curves.iter().any(|curve| !curve.is_empty())
                                });
                            let has_key_here = animatable
                                && active_curves.is_some_and(|curves| {
                                    curves
                                        .iter()
                                        .any(|curve| curve.has_key_at(self.current_frame))
                                });
                            let label_color = if has_key_here {
                                KEYFRAME_BLUE
                            } else if is_animated {
                                INTERPOLATED_TEAL
                            } else {
                                KNOB_LABEL
                            };

                            // Column 1: knob name label plus the `[Key]` toggle.
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(name).color(label_color));
                                if animatable {
                                    let (key_text, key_color) = if has_key_here {
                                        ("[K]", KEYFRAME_BLUE)
                                    } else if is_animated {
                                        ("[+K]", INTERPOLATED_TEAL)
                                    } else {
                                        ("[+K]", Color32::from_rgb(150, 155, 165))
                                    };
                                    if ui
                                        .small_button(RichText::new(key_text).color(key_color))
                                        .clicked()
                                    {
                                        key_toggle = true;
                                    }
                                }
                            });

                            // Column 2: knob widget(s).
                            match &effective {
                                KnobValue::Float(value) => {
                                    // Use `f64` end-to-end: an `f32` round-trip
                                    // made every interpolated frame look like a
                                    // fresh edit and sprinkled phantom keys.
                                    let mut number = *value;
                                    let response = if name == "mix" {
                                        // `mix` is a blend factor: a slider
                                        // communicates 0..1 far better than a
                                        // tiny drag field. `custom_formatter`
                                        // only fixes the *readout* width
                                        // (`0.50`) so the grid column cannot
                                        // reflow mid-drag. Unlike
                                        // `fixed_decimals`, it never re-rounds
                                        // the bound `f64`, which is what used
                                        // to author a key on every frame.
                                        //
                                        // The stable id keeps the drag attached
                                        // to this row even when sibling rows
                                        // change shape.
                                        ui.push_id((node.id, name), |ui| {
                                            ui.add(
                                                egui::Slider::new(&mut number, 0.0..=1.0)
                                                    .custom_formatter(|value, _| {
                                                        format!("{value:.2}")
                                                    }),
                                            )
                                        })
                                        .inner
                                    } else if node.kind == "Defocus" && name == "defocus" {
                                        // Defocus radius is clamped to the
                                        // optical range the renderer supports
                                        // (0..=24) so dragging never produces
                                        // non-finite or out-of-range values.
                                        ui.add(
                                            DragValue::new(&mut number)
                                                .speed(0.01)
                                                .range(0.0..=24.0),
                                        )
                                    } else if node.kind == "Defocus" && name == "aspect" {
                                        // Defocus aspect is clamped to
                                        // [0.25, 4.0] to match the renderer's
                                        // clamp and prevent extreme kernel
                                        // tap counts.
                                        ui.add(
                                            DragValue::new(&mut number)
                                                .speed(0.01)
                                                .range(0.25..=4.0),
                                        )
                                    } else {
                                        ui.add(DragValue::new(&mut number).speed(0.01))
                                    };
                                    note(&response);
                                    if response.changed() && (number - *value).abs() > 1e-6 {
                                        next = KnobValue::Float(number);
                                        changed = true;
                                    }
                                }
                                KnobValue::Int(value) => {
                                    let mut integer = *value;
                                    let response = ui.add(DragValue::new(&mut integer).speed(1.0));
                                    note(&response);
                                    if response.changed() && integer != *value {
                                        next = KnobValue::Int(integer);
                                        changed = true;
                                    }
                                }
                                KnobValue::Bool(value) => {
                                    let mut boolean = *value;
                                    let response = ui.checkbox(&mut boolean, "");
                                    note(&response);
                                    if response.changed() {
                                        next = KnobValue::Bool(boolean);
                                        changed = true;
                                    }
                                }
                                KnobValue::Text(text) => {
                                    let mut edited = text.clone();
                                    let presets = match name {
                                        "operation" => Some(OPERATIONS),
                                        "bbox" => Some(BBOX_MODES),
                                        "channels" if node.kind == "Write" => Some(CHANNELS_MODES),
                                        "channels" if node.kind == "Invert" => {
                                            Some(INVERT_CHANNELS)
                                        }
                                        "alpha"
                                            if node.kind == "Premult"
                                                || node.kind == "Unpremult" =>
                                        {
                                            Some(ALPHA_MODES)
                                        }
                                        "mode" if node.kind == "Saturation" => {
                                            Some(SATURATION_MODES)
                                        }
                                        "resize" if node.kind == "Reformat" => Some(REFORMAT_MODES),
                                        "red" | "green" | "blue" | "alpha"
                                            if node.kind == "Shuffle" =>
                                        {
                                            Some(SHUFFLE_SELECTORS)
                                        }
                                        "color" if node.kind == "BackdropNode" => {
                                            Some(BACKDROP_COLORS)
                                        }
                                        "ramp_type" if node.kind == "Ramp" => Some(RAMP_MODES),
                                        "missing_frames" if node.kind == "Read" => {
                                            Some(MISSING_FRAMES_MODES)
                                        }
                                        _ => None,
                                    };
                                    if let Some(presets) = presets {
                                        let combo = ComboBox::from_id_salt((node_id.0, name))
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
                                                        changed = true;
                                                    }
                                                }
                                            });
                                        note(&combo.response);
                                    } else {
                                        let edit = ui.add(
                                            TextEdit::singleline(&mut edited).desired_width(150.0),
                                        );
                                        note(&edit);
                                        changed = edit.changed();
                                        // Browse... button for the `file` knob on Read/Write nodes.
                                        if name == "file"
                                            && (node.kind == "Read" || node.kind == "Write")
                                            && ui.button("Browse...").clicked()
                                        {
                                            self.pending_file_browse = Some(FileBrowseRequest {
                                                node_id,
                                                node_kind: node.kind.clone(),
                                                current_path: edited.clone(),
                                            });
                                        }
                                    }
                                    if changed {
                                        next = KnobValue::Text(edited);
                                    }
                                }
                                KnobValue::Color(channels)
                                    if (node.kind == "Crop"
                                        && (name == "box_min" || name == "box_max"))
                                        || name == "translate"
                                        || name == "center"
                                        || (node.kind == "Ramp"
                                            && (name == "p0" || name == "p1"))
                                        || (node.kind == "CornerPin2D"
                                            && (name == "to1"
                                                || name == "to2"
                                                || name == "to3"
                                                || name == "to4")) =>
                                {
                                    // 2D spatial knobs keep `Color([x, y, z, w])`
                                    // storage for cartridge compatibility but edit as
                                    // coordinates, never as RGBA.
                                    let mut color = *channels;
                                    ui.label("X");
                                    let x = ui.add(DragValue::new(&mut color[0]).speed(1.0));
                                    note(&x);
                                    if x.changed() {
                                        changed = true;
                                    }
                                    ui.label("Y");
                                    let y = ui.add(DragValue::new(&mut color[1]).speed(1.0));
                                    note(&y);
                                    if y.changed() {
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
                                    let swatch = ui.color_edit_button_srgba(&mut srgba);
                                    note(&swatch);
                                    if swatch.changed() {
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
                                        let response = ui.add(DragValue::new(channel).speed(0.01));
                                        note(&response);
                                        if response.changed() {
                                            changed = true;
                                        }
                                    }
                                    if changed {
                                        next = KnobValue::Color(color);
                                    }
                                }
                            }
                            // `[Key]` toggles a keyframe at the current frame. Adding
                            // one preserves the interpolated value when the knob is
                            // already animated; removing clears the entry entirely
                            // once every channel has no keys left.
                            if key_toggle {
                                let previous = node.curves.get(name).cloned().unwrap_or_default();
                                let curves = if has_key_here {
                                    remove_keyframe(previous.clone(), self.current_frame)
                                } else {
                                    insert_keyframe(
                                        previous.clone(),
                                        &effective,
                                        self.current_frame,
                                    )
                                };
                                messages.push(KernelMessage::Execute(
                                    UndoableCommand::SetKnobCurve {
                                        node: node_id,
                                        name: name.to_string(),
                                        curves,
                                        previous,
                                    },
                                ));
                            }

                            ui.end_row();

                            self.knob_drag_active |= drag_started_here;

                            #[cfg(test)]
                            KNOB_WIDGET_RECTS.with(|rects| {
                                rects.borrow_mut().insert(
                                    (node_id, name.to_string()),
                                    widget_rect.unwrap_or(egui::Rect::ZERO),
                                );
                            });
                            #[cfg(not(test))]
                            let _ = widget_rect;

                            if changed
                                && knob_value_differs(&next, &pre_edit_value)
                                && (!animatable || user_edited || pointer_down)
                            {
                                // Continue the live session if there is one, otherwise
                                // start one from the value the artist saw before this
                                // keystroke/drag.
                                let initial_value = match ui.ctx().data(|data| {
                                    data.get_temp::<ActiveKnobEdit>(active_knob_edit_id())
                                }) {
                                    Some(active)
                                        if active.node == node_id && active.knob == name =>
                                    {
                                        active.initial_value
                                    }
                                    _ => {
                                        let initial = pre_edit_value.clone();
                                        ui.ctx().data_mut(|data| {
                                            data.insert_temp(
                                                active_knob_edit_id(),
                                                ActiveKnobEdit {
                                                    node: node_id,
                                                    knob: name.to_string(),
                                                    initial_value: initial.clone(),
                                                },
                                            );
                                        });
                                        initial
                                    }
                                };
                                let knob_command = UndoableCommand::SetKnob {
                                    node: node_id,
                                    knob: name.to_string(),
                                    value: next.clone(),
                                    previous: initial_value,
                                };

                                // Auto-key: dragging or typing on an already animated
                                // knob writes the new value as a key at the current
                                // frame, so animation editing stays one gesture. A
                                // single `Batch` keeps both halves on one undo step and
                                // lets the kernel coalesce a whole drag into one entry.
                                if is_animated {
                                    let previous =
                                        node.curves.get(name).cloned().unwrap_or_default();
                                    let curves = set_curve_values(
                                        previous.clone(),
                                        &next,
                                        self.current_frame,
                                    );
                                    messages.push(KernelMessage::Execute(UndoableCommand::Batch(
                                        vec![
                                            knob_command,
                                            UndoableCommand::SetKnobCurve {
                                                node: node_id,
                                                name: name.to_string(),
                                                curves,
                                                previous,
                                            },
                                        ],
                                    )));
                                } else {
                                    messages.push(KernelMessage::Execute(knob_command));
                                }
                            }

                            // A row that no longer holds keyboard focus or a pointer
                            // drag has finished its edit. Clearing after the emit above
                            // keeps the `Enter` frame reporting the true origin.
                            if !row_active {
                                ui.ctx().data_mut(|data| {
                                    if let Some(active) =
                                        data.get_temp::<ActiveKnobEdit>(active_knob_edit_id())
                                    {
                                        if active.node == node_id && active.knob == name {
                                            data.remove::<ActiveKnobEdit>(active_knob_edit_id());
                                        }
                                    }
                                });
                            }
                        }
                    });
                if node.kind == "CornerPin2D" {
                    let response = ui
                        .button("[Set to Input]")
                        .on_hover_text("Snap to1..to4 corners to upstream input format");
                    #[cfg(test)]
                    KNOB_WIDGET_RECTS.with(|rects| {
                        rects
                            .borrow_mut()
                            .insert((node_id, "[Set to Input]".into()), response.rect);
                    });
                    if response.clicked() {
                        let size = infer_upstream_format(graph, node_id).unwrap_or((256, 256));
                        let mut commands = Vec::with_capacity(4);
                        for (name, value) in corner_pin_format_knobs(size) {
                            commands.push(UndoableCommand::SetKnob {
                                node: node_id,
                                knob: name.into(),
                                value: value.clone(),
                                previous: node
                                    .knobs
                                    .get(name)
                                    .cloned()
                                    .unwrap_or_else(|| value.clone()),
                            });
                            if let Some(previous) = node
                                .curves
                                .get(name)
                                .filter(|curves| curves.iter().any(|curve| !curve.is_empty()))
                            {
                                commands.push(UndoableCommand::SetKnobCurve {
                                    node: node_id,
                                    name: name.into(),
                                    curves: set_curve_values(
                                        previous.clone(),
                                        &value,
                                        self.current_frame,
                                    ),
                                    previous: previous.clone(),
                                });
                            }
                        }
                        messages.push(KernelMessage::Execute(UndoableCommand::Batch(commands)));
                    }
                }
            });

        messages
    }
}

/// Resolves the value a knob widget should display at `frame`.
///
/// Static knobs return `base_val` unchanged. Animated knobs evaluate their
/// curves: Float/Int read channel 0, Color evaluates each channel from its own
/// curve and keeps the base channel when that curve is missing or empty.
pub fn effective_knob_value(
    node: &CanvasNode,
    knob_name: &str,
    base_val: &KnobValue,
    frame: f64,
) -> KnobValue {
    let Some(curves) = node.curves.get(knob_name) else {
        return base_val.clone();
    };
    if !curves.iter().any(|curve| !curve.is_empty()) {
        return base_val.clone();
    }

    let primary = curves
        .iter()
        .find(|curve| !curve.is_empty())
        .expect("at least one non-empty curve was checked above");

    match base_val {
        KnobValue::Float(_) => KnobValue::Float(primary.evaluate(frame)),
        KnobValue::Int(_) => KnobValue::Int(primary.evaluate(frame).round() as i64),
        KnobValue::Color(base) => {
            let mut rgba = *base;
            for (index, channel) in rgba.iter_mut().enumerate() {
                if let Some(curve) = curves.get(index).filter(|curve| !curve.is_empty()) {
                    *channel = curve.evaluate(frame) as f32;
                }
            }
            KnobValue::Color(rgba)
        }
        other => other.clone(),
    }
}

/// `true` when `edited` carries a real change from `original`.
///
/// Numeric comparisons use a tolerance so a value that round-tripped through a
/// widget is never mistaken for a new edit.
fn knob_value_differs(edited: &KnobValue, original: &KnobValue) -> bool {
    match (edited, original) {
        (KnobValue::Float(a), KnobValue::Float(b)) => (a - b).abs() > 1e-6,
        (KnobValue::Int(a), KnobValue::Int(b)) => a != b,
        (KnobValue::Color(a), KnobValue::Color(b)) => a
            .iter()
            .zip(b.iter())
            .any(|(edited, original)| (edited - original).abs() > 1e-6),
        _ => edited != original,
    }
}

/// Numeric channels of an animatable knob: one for Float/Int, four for Color.
fn knob_channel_values(value: &KnobValue) -> Vec<f64> {
    match value {
        KnobValue::Float(value) => vec![*value],
        KnobValue::Int(value) => vec![*value as f64],
        KnobValue::Color(rgba) => rgba.iter().map(|channel| *channel as f64).collect(),
        _ => Vec::new(),
    }
}

/// Inserts a key at `frame` for every channel, preserving the current
/// interpolated value when a channel is already animated.
fn insert_keyframe(
    mut curves: Vec<AnimationCurve>,
    value: &KnobValue,
    frame: f64,
) -> Vec<AnimationCurve> {
    let values = knob_channel_values(value);
    if curves.len() < values.len() {
        curves.resize(values.len(), AnimationCurve::new());
    }
    for (index, curve) in curves.iter_mut().enumerate() {
        let value = if curve.is_empty() {
            values.get(index).copied().unwrap_or(0.0)
        } else {
            curve.evaluate(frame)
        };
        curve.set_key(frame, value, InterpMode::Smooth);
    }
    curves
}

/// Removes the key at `frame` from every channel. Returns an empty vector once
/// every channel is empty, so `SetKnobCurve` clears the knob's animation.
fn remove_keyframe(mut curves: Vec<AnimationCurve>, frame: f64) -> Vec<AnimationCurve> {
    for curve in &mut curves {
        curve.remove_key(frame);
    }
    if curves.iter().all(|curve| curve.is_empty()) {
        Vec::new()
    } else {
        curves
    }
}

/// Writes concrete channel values at `frame`, used for auto-key after an edit.
pub(crate) fn set_curve_values(
    mut curves: Vec<AnimationCurve>,
    value: &KnobValue,
    frame: f64,
) -> Vec<AnimationCurve> {
    let values = knob_channel_values(value);
    if curves.len() < values.len() {
        curves.resize(values.len(), AnimationCurve::new());
    }
    for (curve, value) in curves.iter_mut().zip(values) {
        curve.set_key(frame, value, InterpMode::Smooth);
    }
    curves
}

fn to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, Vec2};
    use tanova_kernel::{Dag, KernelEvent, KnobTable, UndoStack};

    const NODE: NodeId = NodeId(2);

    fn screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 1200.0))
    }

    /// Runs one `PropertiesPanel::show` pass and returns what it emitted.
    fn run_panel(
        ctx: &egui::Context,
        mirror: &GraphMirror,
        events: Vec<Event>,
    ) -> Vec<KernelMessage> {
        run_panel_at(ctx, mirror, events, 1.0)
    }

    /// Same as [`run_panel`] but with an explicit playhead frame.
    fn run_panel_at(
        ctx: &egui::Context,
        mirror: &GraphMirror,
        events: Vec<Event>,
        frame: f64,
    ) -> Vec<KernelMessage> {
        run_panel_sized(ctx, mirror, events, frame, screen())
    }

    /// Runs one panel pass inside `screen`, so tests can mimic the narrow
    /// properties column the live app uses.
    fn run_panel_sized(
        ctx: &egui::Context,
        mirror: &GraphMirror,
        events: Vec<Event>,
        frame: f64,
        screen: Rect,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let mut panel = PropertiesPanel {
                            current_frame: frame,
                            ..PropertiesPanel::default()
                        };
                        messages = panel.show(ui, mirror, Some(NODE), None);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        messages
    }

    fn knob_rect(name: &str) -> egui::Rect {
        KNOB_WIDGET_RECTS.with(|rects| {
            rects
                .borrow()
                .get(&(NODE, name.to_string()))
                .copied()
                .unwrap_or_else(|| panic!("no widget rect captured for {name}"))
        })
    }

    #[test]
    fn corner_pin_auto_chain_and_set_to_input_match_1920x1080_upstream() {
        use crate::canvas::{NodeCanvasState, spawn_node_message};
        use tanova_kernel::ProjectFormat;

        tanova_nodes::ensure_linked();
        let checker = NodeId(1);
        let format = ProjectFormat::new(256, 256);
        let mut dag = Dag::new();
        dag.set_project_format(Some(format));
        dag.add_node(checker, "CheckerBoard2").unwrap();
        dag.set_knob(checker, "format_width", KnobValue::Int(1920))
            .unwrap();
        dag.set_knob(checker, "format_height", KnobValue::Int(1080))
            .unwrap();
        let mut mirror = GraphMirror::from_dag(&dag, None, Some(format));
        let mut canvas = NodeCanvasState::default();
        canvas.primary_selected = Some(checker);
        let spawn = spawn_node_message(&mirror, &mut canvas, "CornerPin2D", Pos2::ZERO);
        assert_eq!(canvas.primary_selected, Some(NODE));
        let KernelMessage::Execute(UndoableCommand::Batch(commands)) = &spawn else {
            panic!("expected atomic spawn");
        };
        assert!(matches!(
            commands.as_slice(),
            [
                UndoableCommand::AddNode { .. },
                UndoableCommand::ConnectEdge {
                    from_node: NodeId(1),
                    to_node: NODE,
                    to_port: tanova_kernel::PortId(0),
                    ..
                },
                UndoableCommand::SetKnob { .. },
                UndoableCommand::SetKnob { .. },
                UndoableCommand::SetKnob { .. },
                UndoableCommand::SetKnob { .. },
            ]
        ));
        let expected = [
            ("to1", [0.0; 4]),
            ("to2", [1920.0, 0.0, 0.0, 0.0]),
            ("to3", [1920.0, 1080.0, 0.0, 0.0]),
            ("to4", [0.0, 1080.0, 0.0, 0.0]),
        ];
        let assert_corners = |mirror: &GraphMirror| {
            for (name, value) in expected {
                assert_eq!(
                    mirror.node(NODE).unwrap().knobs.get(name),
                    Some(&KnobValue::Color(value))
                );
            }
        };
        let mut undo = UndoStack::new();
        feed(vec![spawn], &mut mirror, &mut dag, &mut undo);
        assert_corners(&mirror);

        let stale = KnobValue::Color([256.0, 256.0, 0.0, 0.0]);
        let edit = UndoableCommand::SetKnob {
            node: NODE,
            knob: "to3".into(),
            value: stale.clone(),
            previous: KnobValue::Color(expected[2].1),
        };
        dag.apply_command(edit.clone()).unwrap();
        mirror.apply_command(&edit);
        let ctx = egui::Context::default();
        run_panel(&ctx, &mirror, vec![]);
        let pos = knob_rect("[Set to Input]").center();
        run_panel(&ctx, &mirror, vec![Event::PointerMoved(pos)]);
        let click = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        run_panel(&ctx, &mirror, vec![click(true)]);
        let messages = run_panel(&ctx, &mirror, vec![click(false)]);
        assert!(
            matches!(messages.as_slice(),
                [KernelMessage::Execute(UndoableCommand::Batch(commands))] if commands.len() == 4
            ),
            "{messages:?}"
        );
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_corners(&mirror);
        undo_once(&mut undo, &mut dag, &mut mirror);
        assert_eq!(mirror.node(NODE).unwrap().knobs.get("to3"), Some(&stale));
    }

    #[test]
    fn write_channels_paints_only_one_value_widget() {
        tanova_nodes::ensure_linked();
        let mut dag = Dag::new();
        dag.add_node(NODE, "Write").unwrap();
        let mirror = GraphMirror::from_dag(&dag, None, None);
        let output = egui::Context::default().run_pass(
            egui::RawInput {
                screen_rect: Some(screen()),
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        assert!(
                            PropertiesPanel::default()
                                .show(ui, &mirror, Some(NODE), None)
                                .is_empty()
                        );
                    });
                })
            },
        );
        fn count_rgba(shape: &egui::Shape) -> usize {
            match shape {
                egui::Shape::Text(text) => usize::from(text.galley.text() == "rgba"),
                egui::Shape::Vec(shapes) => shapes.iter().map(count_rgba).sum(),
                _ => 0,
            }
        }
        assert_eq!(
            output
                .shapes
                .iter()
                .map(|shape| count_rgba(&shape.shape))
                .sum::<usize>(),
            1
        );
        output.drop_without_applying_deltas();
    }

    /// Mirrors the kernel: apply each emitted `SetKnob` to the DAG, record the
    /// enriched command on the undo stack, and follow the change in the mirror.
    fn feed(
        messages: Vec<KernelMessage>,
        mirror: &mut GraphMirror,
        dag: &mut Dag,
        undo: &mut UndoStack,
    ) {
        for message in messages {
            if let KernelMessage::Execute(command) = message {
                let enriched = dag
                    .apply_command(command)
                    .expect("the dag should accept a knob edit");
                undo.push(enriched.clone());
                mirror.apply_command(&enriched);
            }
        }
    }

    fn undo_once(undo: &mut UndoStack, dag: &mut Dag, mirror: &mut GraphMirror) {
        let command = undo.undo().expect("an undo step must exist");
        dag.undo_command(&command)
            .expect("the dag applies the inverse");
        mirror.apply_event(&KernelEvent::Undone(command));
    }

    fn redo_once(undo: &mut UndoStack, dag: &mut Dag, mirror: &mut GraphMirror) {
        let command = undo.redo().expect("a redo step must exist");
        dag.apply_command(command.clone())
            .expect("the dag re-applies the command");
        mirror.apply_event(&KernelEvent::Redone(command));
    }

    fn gain(mirror: &GraphMirror) -> f64 {
        match mirror
            .node(NODE)
            .and_then(|node| node.knobs.get("gain"))
            .cloned()
        {
            Some(KnobValue::Float(value)) => value,
            other => panic!("gain should be a float knob, got {other:?}"),
        }
    }

    /// `gain` is stored as `f64` but edited through an `f32` widget, so compare
    /// with a tolerance rather than bit-for-bit.
    fn assert_gain(mirror: &GraphMirror, expected: f64) {
        let actual = gain(mirror);
        assert!(
            (actual - expected).abs() < 1e-6,
            "gain should be {expected}, got {actual}"
        );
    }

    fn click(center: Pos2) -> Vec<Vec<Event>> {
        vec![
            vec![
                Event::PointerMoved(center),
                Event::PointerButton {
                    pos: center,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
            vec![Event::PointerButton {
                pos: center,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
            // The first edit-mode frame selects all the text.
            vec![],
        ]
    }

    /// A numeric field commits on every keystroke, so `0.5` briefly becomes
    /// `0.0`. Pausing between keystrokes must not split the edit: one undo has
    /// to land on the previous committed number, never on the intermediate `0`.
    #[test]
    fn multi_keystroke_knob_typing_across_pauses_undoes_per_committed_number() {
        let ctx = egui::Context::default();
        // Build the node explicitly instead of linking the cartridge registry:
        // a linked registry changes the default knobs other tests observe.
        let mut knobs = KnobTable::new();
        knobs.insert("gain", KnobValue::Float(1.0));

        let mut mirror = GraphMirror::default();
        mirror.apply_event(&KernelEvent::Applied(UndoableCommand::RestoreNode {
            id: NODE,
            kind: "Grade".to_string(),
            x: 0.0,
            y: 0.0,
            disabled: false,
            knobs: Box::new(knobs.clone()),
            edges: Vec::new(),
        }));
        let mut dag = Dag::new();
        dag.restore_node(NODE, "Grade", 0.0, 0.0, false, &knobs, &[])
            .expect("dag node #2");
        let mut undo = UndoStack::new();

        assert_gain(&mirror, 1.0);

        // Lay the panel out once so the gain widget's rect is known.
        let _ = run_panel(&ctx, &mirror, vec![]);
        let center = knob_rect("gain").center();

        // --- Edit 1: 1.0 -> 0.5 -------------------------------------------
        for events in click(center) {
            let _ = run_panel(&ctx, &mirror, events);
        }

        let messages = run_panel(&ctx, &mirror, vec![Event::Text("0".to_string())]);
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_gain(&mirror, 0.0);
        // Longer than COALESCE_WINDOW_MS while the field keeps focus.
        undo.break_coalescing();

        let messages = run_panel(&ctx, &mirror, vec![Event::Text(".5".to_string())]);
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_gain(&mirror, 0.5);
        undo.break_coalescing();

        // Enter commits and releases focus, ending the session.
        let _ = run_panel(&ctx, &mirror, vec![enter()]);
        assert!(
            undo.can_undo(),
            "the first edit should be on the undo stack"
        );

        // --- Edit 2: 0.5 -> 0.2 -------------------------------------------
        for events in click(center) {
            let _ = run_panel(&ctx, &mirror, events);
        }

        let messages = run_panel(&ctx, &mirror, vec![Event::Text("0".to_string())]);
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_gain(&mirror, 0.0);
        undo.break_coalescing();

        let messages = run_panel(&ctx, &mirror, vec![Event::Text(".2".to_string())]);
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_gain(&mirror, 0.2);
        undo.break_coalescing();

        let _ = run_panel(&ctx, &mirror, vec![enter()]);

        // Two sessions, two undo steps.
        undo_once(&mut undo, &mut dag, &mut mirror);
        assert_gain(&mirror, 0.5);
        undo_once(&mut undo, &mut dag, &mut mirror);
        assert_gain(&mirror, 1.0);
        assert!(!undo.can_undo(), "the keystrokes must not add extra steps");

        redo_once(&mut undo, &mut dag, &mut mirror);
        assert_gain(&mirror, 0.5);
        redo_once(&mut undo, &mut dag, &mut mirror);
        assert_gain(&mirror, 0.2);
    }

    #[test]
    fn animated_slider_scrubbing_timeline_emits_zero_phantom_keys() {
        let ctx = egui::Context::default();

        // Grade #2 with an animated `mix`: 0.0 at frame 1, 1.0 at frame 20.
        let mut knobs = KnobTable::new();
        knobs.insert("mix", KnobValue::Float(0.0));

        let mut mirror = GraphMirror::default();
        mirror.apply_event(&KernelEvent::Applied(UndoableCommand::RestoreNode {
            id: NODE,
            kind: "Grade".to_string(),
            x: 0.0,
            y: 0.0,
            disabled: false,
            knobs: Box::new(knobs),
            edges: Vec::new(),
        }));

        let mut animated = AnimationCurve::new();
        animated.set_key(1.0, 0.0, InterpMode::Smooth);
        animated.set_key(20.0, 1.0, InterpMode::Smooth);
        mirror.apply_command(&UndoableCommand::SetKnobCurve {
            node: NODE,
            name: "mix".to_string(),
            curves: vec![animated],
            previous: Vec::new(),
        });
        assert!(
            !mirror.keyframe_times(Some(NODE)).is_empty(),
            "the fixture must expose keyframe ticks"
        );

        // Warm-up pass, as the app gets one layout pass per displayed frame.
        let _ = run_panel_at(&ctx, &mirror, vec![], 1.0);

        // Scrub the playhead across every frame without touching the widget.
        let mut emitted = Vec::new();
        for frame in 1..=30 {
            let messages = run_panel_at(&ctx, &mirror, vec![], frame as f64);
            emitted.extend(messages);
        }

        let executes = emitted
            .iter()
            .filter(|message| matches!(message, KernelMessage::Execute(_)))
            .count();
        assert_eq!(
            executes, 0,
            "scrubbing an animated slider must never author keys; got {emitted:?}"
        );
    }

    /// The `mix` value currently mirrored for the test node.
    fn mix(mirror: &GraphMirror) -> f64 {
        match mirror
            .node(NODE)
            .and_then(|node| node.knobs.get("mix"))
            .cloned()
        {
            Some(KnobValue::Float(value)) => value,
            other => panic!("mix should be a float knob, got {other:?}"),
        }
    }

    /// Drags the `mix` rail across five intermediate frames without releasing
    /// the button.
    ///
    /// A held pointer must move the thumb on *every* frame: freezing mid-drag
    /// and jumping to the release position is the failure this guards against.
    /// The pointer drifts vertically off the thin rail on purpose, as a real
    /// hand does.
    #[test]
    fn mix_slider_drag_emits_continuous_monotonic_updates_before_release() {
        let ctx = egui::Context::default();

        let mut knobs = KnobTable::new();
        knobs.insert("mix", KnobValue::Float(0.0));

        let mut mirror = GraphMirror::default();
        mirror.apply_event(&KernelEvent::Applied(UndoableCommand::RestoreNode {
            id: NODE,
            kind: "Grade".to_string(),
            x: 0.0,
            y: 0.0,
            disabled: false,
            knobs: Box::new(knobs.clone()),
            edges: Vec::new(),
        }));
        let mut dag = Dag::new();
        dag.restore_node(NODE, "Grade", 0.0, 0.0, false, &knobs, &[])
            .expect("dag node #2");
        let mut undo = UndoStack::new();

        // Mimic the live properties column: ~300pt wide, short enough to
        // scroll. A wide test viewport hides the layout pressure that the real
        // panel has.
        let narrow = Rect::from_min_size(Pos2::ZERO, Vec2::new(300.0, 420.0));

        // Warm-up pass: the panel must lay out once before it can be aimed at.
        let _ = run_panel_sized(&ctx, &mirror, vec![], 1.0, narrow);
        let rect = knob_rect("mix");
        let y = rect.center().y;
        let x_at = |t: f32| rect.left() + t * MIX_SLIDER_WIDTH;

        // 1. Press on the rail at 25% and walk right in five steps.
        let press = Pos2::new(x_at(0.25), y);
        let messages = run_panel_sized(
            &ctx,
            &mirror,
            vec![
                Event::PointerMoved(press),
                Event::PointerButton {
                    pos: press,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
            1.0,
            narrow,
        );
        feed(messages, &mut mirror, &mut dag, &mut undo);

        let mut samples = vec![(0.25_f32, mix(&mirror))];
        assert!(
            (samples[0].1 - 0.25).abs() < 0.12,
            "pressing at 25% must place mix near 0.25, got {}",
            samples[0].1
        );

        for (step, target) in [0.4_f32, 0.55, 0.7, 0.85, 1.0].iter().enumerate() {
            // The pointer wanders above and below the rail while dragging.
            let drift = if step % 2 == 0 { -5.0 } else { 4.0 };
            let pos = Pos2::new(x_at(*target), y + drift);
            let messages =
                run_panel_sized(&ctx, &mirror, vec![Event::PointerMoved(pos)], 1.0, narrow);
            feed(messages, &mut mirror, &mut dag, &mut undo);
            samples.push((*target, mix(&mirror)));
        }

        // 2. Every intermediate frame must have tracked the pointer. A frozen
        //    thumb would leave the value flat until the release frame.
        for pair in samples.windows(2) {
            let (target, previous) = pair[0];
            let (_, current) = pair[1];
            assert!(
                current > previous + 1e-6,
                "mix stalled at {previous} while dragging toward {target}; samples: {samples:?}"
            );
        }
        for (target, observed) in &samples {
            assert!(
                (observed - f64::from(*target)).abs() < 0.12,
                "mix should track the pointer at {target}, got {observed}; samples: {samples:?}"
            );
        }

        // 3. Releasing is bookkeeping only: it must not be the first update.
        let release = Pos2::new(x_at(1.0), y);
        let messages = run_panel_sized(
            &ctx,
            &mirror,
            vec![Event::PointerButton {
                pos: release,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
            1.0,
            narrow,
        );
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert!(
            (mix(&mirror) - 1.0).abs() < 1e-6,
            "the drag must land at 1.0, got {}",
            mix(&mirror)
        );

        // One gesture: the whole drag coalesces into a single undo step.
        assert!(undo.can_undo(), "the drag must be undoable");
    }

    fn enter() -> Event {
        Event::Key {
            key: Key::Enter,
            physical_key: Some(Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    /// Builds a one-channel curve with the given `(frame, value)` keys.
    fn curve(keys: &[(f64, f64)]) -> Vec<AnimationCurve> {
        let mut curve = AnimationCurve::new();
        for (frame, value) in keys {
            curve.set_key(*frame, *value, InterpMode::Smooth);
        }
        vec![curve]
    }

    #[test]
    fn set_knob_curve_undo_redo_and_keyframe_jump() {
        let mut knobs = KnobTable::new();
        knobs.insert("mix", KnobValue::Float(0.0));

        let mut mirror = GraphMirror::default();
        mirror.apply_event(&KernelEvent::Applied(UndoableCommand::RestoreNode {
            id: NODE,
            kind: "Grade".to_string(),
            x: 0.0,
            y: 0.0,
            disabled: false,
            knobs: Box::new(knobs.clone()),
            edges: Vec::new(),
        }));
        let mut dag = Dag::new();
        dag.restore_node(NODE, "Grade", 0.0, 0.0, false, &knobs, &[])
            .expect("dag node #2");
        let mut undo = UndoStack::new();

        assert!(
            mirror.keyframe_times(Some(NODE)).is_empty(),
            "a fresh node starts with no keys"
        );

        // Key 1: frame 5.
        let first = curve(&[(5.0, 0.0)]);
        feed(
            vec![KernelMessage::Execute(UndoableCommand::SetKnobCurve {
                node: NODE,
                name: "mix".to_string(),
                curves: first.clone(),
                previous: Vec::new(),
            })],
            &mut mirror,
            &mut dag,
            &mut undo,
        );
        assert_eq!(mirror.keyframe_times(Some(NODE)), vec![5]);

        // Key 2 extends the same curve to frame 20. Breaking coalescing keeps
        // this a separate undo step instead of merging with key 1.
        undo.break_coalescing();
        let second = curve(&[(5.0, 0.0), (20.0, 1.0)]);
        feed(
            vec![KernelMessage::Execute(UndoableCommand::SetKnobCurve {
                node: NODE,
                name: "mix".to_string(),
                curves: second.clone(),
                previous: first.clone(),
            })],
            &mut mirror,
            &mut dag,
            &mut undo,
        );
        assert_eq!(
            mirror.keyframe_times(Some(NODE)),
            vec![5, 20],
            "the timeline must see both keys"
        );
        // With nothing selected the timeline gathers every node's keys.
        assert_eq!(mirror.keyframe_times(None), vec![5, 20]);

        // Undo unwinds the second key, then the first.
        undo_once(&mut undo, &mut dag, &mut mirror);
        assert_eq!(mirror.keyframe_times(Some(NODE)), vec![5]);
        undo_once(&mut undo, &mut dag, &mut mirror);
        assert!(
            mirror.keyframe_times(Some(NODE)).is_empty(),
            "undoing both edits must clear the animation"
        );
        assert!(
            dag.node(NODE).is_some_and(|node| node.curves.is_empty()),
            "the kernel node must lose its curves too"
        );

        // Redo restores both keys in order.
        redo_once(&mut undo, &mut dag, &mut mirror);
        assert_eq!(mirror.keyframe_times(Some(NODE)), vec![5]);
        redo_once(&mut undo, &mut dag, &mut mirror);
        assert_eq!(
            mirror.keyframe_times(Some(NODE)),
            vec![5, 20],
            "redo must restore the full animation"
        );
    }

    /// Pressing and dragging a plain `KnobValue::Float` `DragValue` (e.g.
    /// `Defocus.defocus`) must emit `SetKnob` on every intermediate frame,
    /// not just on release.
    #[test]
    fn float_drag_value_emits_continuous_updates_before_release() {
        let ctx = egui::Context::default();

        let mut knobs = KnobTable::new();
        knobs.insert("defocus", KnobValue::Float(4.0));

        let mut mirror = GraphMirror::default();
        mirror.apply_event(&KernelEvent::Applied(UndoableCommand::RestoreNode {
            id: NODE,
            kind: "Defocus".to_string(),
            x: 0.0,
            y: 0.0,
            disabled: false,
            knobs: Box::new(knobs.clone()),
            edges: Vec::new(),
        }));
        let mut dag = Dag::new();
        dag.restore_node(NODE, "Defocus", 0.0, 0.0, false, &knobs, &[])
            .expect("dag node #2");
        let mut undo = UndoStack::new();

        let narrow = Rect::from_min_size(Pos2::ZERO, Vec2::new(300.0, 420.0));

        // Warm-up pass: capture the defocus widget's rect.
        let _ = run_panel_sized(&ctx, &mirror, vec![], 1.0, narrow);
        let rect = knob_rect("defocus");
        let y = rect.center().y;

        // Press on the defocus DragValue. Press alone must not emit SetKnob.
        let press = Pos2::new(rect.center().x, y);
        let messages = run_panel_sized(
            &ctx,
            &mirror,
            vec![
                Event::PointerMoved(press),
                Event::PointerButton {
                    pos: press,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
            1.0,
            narrow,
        );
        let press_executes: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m, KernelMessage::Execute(_)))
            .collect();
        assert!(
            press_executes.is_empty(),
            "press must not emit SetKnob, got {messages:?}"
        );
        feed(messages, &mut mirror, &mut dag, &mut undo);

        // Drag right across 5 intermediate frames without releasing the button.
        // The pointer drifts above/below the rail on purpose.
        let mut emitted_values = Vec::new();
        for (i, target) in [5.0_f64, 6.0, 7.0, 8.0, 9.0].iter().enumerate() {
            let drift = if i % 2 == 0 { -3.0 } else { 3.0 };
            let x = rect.left() + ((*target - 4.0) * 12.0) as f32;
            let pos = Pos2::new(x, y + drift);
            let messages =
                run_panel_sized(&ctx, &mirror, vec![Event::PointerMoved(pos)], 1.0, narrow);
            // Collect emitted SetKnob values first, then feed to mirror/dag.
            {
                for message in &messages {
                    if let KernelMessage::Execute(UndoableCommand::SetKnob {
                        knob, value, ..
                    }) = message
                    {
                        if knob == "defocus" {
                            if let KnobValue::Float(v) = value {
                                emitted_values.push(*v);
                            }
                        }
                    }
                }
            }
            feed(messages, &mut mirror, &mut dag, &mut undo);
        }

        // Every intermediate frame must have tracked the pointer: a frozen
        // DragValue would leave the value flat until the release frame.
        assert!(
            emitted_values.len() >= 3,
            "defocus DragValue must emit SetKnob on intermediate frames during drag, got {emitted_values:?}"
        );
        for pair in emitted_values.windows(2) {
            assert!(
                pair[1] > pair[0],
                "defocus stalled during drag, samples: {emitted_values:?}"
            );
        }
        // The last emitted value must be greater than the first (drag moved).
        assert!(
            emitted_values.last().copied().unwrap_or(0.0) > emitted_values[0],
            "defocus must increase during drag, samples: {emitted_values:?}"
        );

        // Release is bookkeeping only: it must not be the first update.
        let release = Pos2::new(rect.right(), y);
        let messages = run_panel_sized(
            &ctx,
            &mirror,
            vec![Event::PointerButton {
                pos: release,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
            1.0,
            narrow,
        );
        feed(messages, &mut mirror, &mut dag, &mut undo);
        let final_defocus = mirror
            .node(NODE)
            .unwrap()
            .knobs
            .get("defocus")
            .cloned()
            .unwrap();
        assert!(
            matches!(final_defocus, KnobValue::Float(v) if v > 4.0),
            "the drag must increase defocus above 4.0, got {final_defocus:?}"
        );
    }
}
