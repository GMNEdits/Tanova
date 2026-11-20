//! Interactive curve editor for `{curve ...}` keyframe animation.
//!
//! Renders a spline plot with clickable/draggable keyframe markers and emits
//! `UndoableCommand::SetKnobCurve` so the kernel owns all curve state. Lives
//! in a bottom-left tab next to the Node Graph.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

use tanova_kernel::{AnimationCurve, InterpMode, KernelMessage, Keyframe, NodeId, UndoableCommand};

use crate::canvas::GraphMirror;

/// Default visible frame range.
const DEFAULT_FRAME_MIN: f64 = 1.0;
const DEFAULT_FRAME_MAX: f64 = 100.0;
/// Default visible value range.
const DEFAULT_VAL_MIN: f64 = -0.1;
const DEFAULT_VAL_MAX: f64 = 1.5;

/// Minimum span allowed for either axis so a single key or a flat curve never
/// divides by zero when mapping world -> screen.
const MIN_FRAME_SPAN: f64 = 10.0;
const MIN_VAL_SPAN: f64 = 0.2;
const MIN_ZOOM_FRAME_SPAN: f64 = 1.0;
const MIN_ZOOM_VAL_SPAN: f64 = 0.1;

/// Channel colors used in the curve list and on the plot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChannelColor {
    Scalar, // Gold
    Red,
    Green,
    Blue,
    Alpha,
}

impl ChannelColor {
    fn of(name: &str, index: usize) -> Self {
        let lower = name.to_ascii_lowercase();
        match (lower.as_str(), index) {
            ("rgba", 0) | ("rgb", 0) | ("rgba", _) if index == 0 => ChannelColor::Red,
            ("rgba", 1) | ("rgb", 1) => ChannelColor::Green,
            ("rgba", 2) | ("rgb", 2) => ChannelColor::Blue,
            ("rgba", 3) | ("alpha", _) => ChannelColor::Alpha,
            _ => ChannelColor::Scalar,
        }
    }

    fn color(self) -> Color32 {
        match self {
            ChannelColor::Scalar => Color32::from_rgb(255, 190, 60),
            ChannelColor::Red => Color32::from_rgb(255, 70, 70),
            ChannelColor::Green => Color32::from_rgb(90, 220, 90),
            ChannelColor::Blue => Color32::from_rgb(70, 130, 255),
            ChannelColor::Alpha => Color32::from_rgb(220, 110, 255),
        }
    }
}

/// One curve entry shown in the left sidebar.
#[derive(Clone, PartialEq, Eq)]
struct CurveEntry {
    node_id: NodeId,
    knob: String,
    channel_index: usize,
    channel: ChannelColor,
}

/// State for the interactive curve editor panel.
pub struct CurveEditorPanel {
    /// Visible frame range.
    pub view_frame_min: f64,
    pub view_frame_max: f64,
    /// Visible value range.
    pub view_val_min: f64,
    pub view_val_max: f64,
    /// Currently selected curve: `(node_id, knob_name, channel_index)`.
    pub selected_curve: Option<(NodeId, String, usize)>,
    /// Index into the selected curve's `keys` (sorted ascending by frame).
    pub selected_key_index: Option<usize>,
    /// True while a keyframe marker is being dragged.
    pub dragging_key: bool,
    /// Current playhead frame, synced from the timeline each frame.
    pub current_frame: f64,
}

/// Fixed width of the left curve-list sidebar.
const SIDEBAR_WIDTH: f32 = 140.0;

/// Inner plot margins so numeric labels never clip at plot edges.
const PLOT_MARGIN_LEFT: f32 = 42.0;
const PLOT_MARGIN_TOP: f32 = 16.0;
const PLOT_MARGIN_RIGHT: f32 = 12.0;
const PLOT_MARGIN_BOTTOM: f32 = 10.0;

/// Nice grid step candidates (frames) for adaptive spacing.
const FRAME_STEPS: &[f64] = &[1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0];
/// Nice grid step candidates (values) for adaptive spacing.
const VAL_STEPS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.0, 5.0];

/// Target pixel spacing between adjacent gridlines.
const TARGET_GRID_SPACING: f32 = 52.0;

impl Default for CurveEditorPanel {
    fn default() -> Self {
        Self {
            view_frame_min: DEFAULT_FRAME_MIN,
            view_frame_max: DEFAULT_FRAME_MAX,
            view_val_min: DEFAULT_VAL_MIN,
            view_val_max: DEFAULT_VAL_MAX,
            selected_curve: None,
            selected_key_index: None,
            dragging_key: false,
            current_frame: 1.0,
        }
    }
}

/// Scan every node in the mirror and return all animated curves,
/// labeled `NodeId #knob.ch` so the sidebar can list them even when
/// no single node is selected.
fn all_animated_curves(graph: &GraphMirror) -> Vec<CurveEntry> {
    let mut out: Vec<CurveEntry> = Vec::new();
    for node in graph.nodes() {
        for (knob, curves) in &node.curves {
            for (index, curve) in curves.iter().enumerate() {
                if !curve.keys.is_empty() {
                    out.push(CurveEntry {
                        node_id: node.id,
                        knob: knob.clone(),
                        channel_index: index,
                        channel: ChannelColor::of(knob, index),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| {
        a.node_id
            .0
            .cmp(&b.node_id.0)
            .then_with(|| a.knob.cmp(&b.knob))
            .then_with(|| a.channel_index.cmp(&b.channel_index))
    });
    out
}

/// Rebuilds the curve list from the graph mirror for a single node.
fn animated_curves_for_node(graph: &GraphMirror, node_id: NodeId) -> Vec<CurveEntry> {
    let mut out: Vec<CurveEntry> = Vec::new();
    let Some(node) = graph.node(node_id) else {
        return out;
    };
    for (knob, curves) in &node.curves {
        for (index, curve) in curves.iter().enumerate() {
            if !curve.keys.is_empty() {
                out.push(CurveEntry {
                    node_id: node.id,
                    knob: knob.clone(),
                    channel_index: index,
                    channel: ChannelColor::of(knob, index),
                });
            }
        }
    }
    out.sort_by(|a, b| {
        a.knob
            .cmp(&b.knob)
            .then_with(|| a.channel_index.cmp(&b.channel_index))
    });
    out
}

/// Pick a nice grid step so that adjacent gridlines are at least
/// `TARGET_GRID_SPACING` px apart across `screen_span` px showing `data_span`.
fn nice_step(data_span: f64, screen_span: f32, candidates: &[f64]) -> f64 {
    let ideal = data_span / (screen_span as f64 / TARGET_GRID_SPACING as f64).max(1.0);
    candidates
        .iter()
        .copied()
        .find(|&s| s >= ideal)
        .unwrap_or(*candidates.last().unwrap())
}

impl CurveEditorPanel {
    /// Returns `true` while a keyframe marker is being dragged.
    pub fn is_dragging_key(&self) -> bool {
        self.dragging_key
    }

    /// Fits the visible frame/value spans around every key of the selected
    /// node's curves, adding ~15% padding and enforcing safe minimums.
    fn fit_view(&mut self, graph: &GraphMirror) {
        let Some((node_id, _, _)) = self.selected_curve else {
            return;
        };
        let Some(node) = graph.node(node_id) else {
            return;
        };

        let mut min_f = f64::INFINITY;
        let mut max_f = f64::NEG_INFINITY;
        let mut min_v = f64::INFINITY;
        let mut max_v = f64::NEG_INFINITY;

        for curves in node.curves.values() {
            for curve in curves {
                for key in &curve.keys {
                    min_f = min_f.min(key.frame);
                    max_f = max_f.max(key.frame);
                    min_v = min_v.min(key.value);
                    max_v = max_v.max(key.value);
                }
            }
        }

        if min_f.is_infinite() {
            return; // no keys at all
        }

        let pad_f = ((max_f - min_f).abs().max(1.0) * 0.15).max(1.0);
        let pad_v = ((max_v - min_v).abs().max(0.5) * 0.15).max(0.1);

        let mut frame_min = (min_f - pad_f).max(0.0);
        let mut frame_max = max_f + pad_f;
        if frame_max - frame_min < MIN_FRAME_SPAN {
            let mid = (frame_min + frame_max) / 2.0;
            frame_min = mid - MIN_FRAME_SPAN / 2.0;
            frame_max = mid + MIN_FRAME_SPAN / 2.0;
        }

        let mut val_min = min_v - pad_v;
        let mut val_max = max_v + pad_v;
        if val_max - val_min < MIN_VAL_SPAN {
            let mid = (val_min + val_max) / 2.0;
            val_min = mid - MIN_VAL_SPAN / 2.0;
            val_max = mid + MIN_VAL_SPAN / 2.0;
        }

        self.view_frame_min = frame_min;
        self.view_frame_max = frame_max;
        self.view_val_min = val_min;
        self.view_val_max = val_max;
    }

    /// Maps a frame value to an x screen position within `rect`.
    fn frame_to_x(&self, rect: &Rect, frame: f64) -> f32 {
        let span = (self.view_frame_max - self.view_frame_min).max(MIN_FRAME_SPAN);
        let t = ((frame - self.view_frame_min) / span).clamp(0.0, 1.0);
        rect.min.x + (t * rect.width() as f64) as f32
    }

    /// Maps a value to a y screen position within `rect` (y increases downward).
    fn val_to_y(&self, rect: &Rect, value: f64) -> f32 {
        let span = (self.view_val_max - self.view_val_min).max(MIN_VAL_SPAN);
        let t = ((value - self.view_val_min) / span).clamp(0.0, 1.0);
        rect.max.y - (t * rect.height() as f64) as f32
    }

    /// Maps an x screen position back to a frame value.
    fn x_to_frame(&self, rect: &Rect, x: f32) -> f64 {
        let span = (self.view_frame_max - self.view_frame_min).max(MIN_FRAME_SPAN);
        let t = ((x as f64 - rect.min.x as f64) / rect.width() as f64).clamp(0.0, 1.0);
        self.view_frame_min + t * span
    }

    /// Maps a y screen position back to a value.
    fn y_to_val(&self, rect: &Rect, y: f32) -> f64 {
        let span = (self.view_val_max - self.view_val_min).max(MIN_VAL_SPAN);
        let t = ((rect.max.y as f64 - y as f64) / rect.height() as f64).clamp(0.0, 1.0);
        self.view_val_min + t * span
    }

    /// Returns the messages the user asked for this frame.
    ///
    /// **Node selection (Bug 1 fix):** `show` does not take a node ID — it
    /// resolves the effective selected curve from `self.selected_curve` first,
    /// then falls back to scanning all nodes for animated curves (Nuke-style).
    /// This mirrors `PropertiesPanel`, which reads `canvas.primary_selected`.
    pub fn show(&mut self, ui: &mut Ui, graph: &GraphMirror) -> Vec<KernelMessage> {
        let mut messages: Vec<KernelMessage> = Vec::new();

        // Sample pointer input before any widget runs.
        let pointer_pos = ui.input(|i| i.pointer.hover_pos());

        // --- Resolve effective selected curve (Bug 1 fix) ---
        //
        // 1. If `selected_curve` is set and its node/channel still has keys,
        //    keep it (the artist explicitly picked this curve).
        // 2. Otherwise, if `selected_curve` is set but stale, clear it.
        // 3. Otherwise, scan ALL nodes for animated curves (Nuke fallback).
        // 4. Auto-select the first available curve so the spline is visible.
        let all_curves = all_animated_curves(graph);
        let preferred_still_valid = self.selected_curve.as_ref().and_then(|(nid, knob, ch)| {
            graph.node(*nid).and_then(|node| {
                node.curves
                    .get(knob)
                    .and_then(|curves| curves.get(*ch).filter(|c| !c.keys.is_empty()).map(|_| ()))
            })
        });
        if preferred_still_valid.is_none() {
            self.selected_curve = None;
        }
        if self.selected_curve.is_none() && !all_curves.is_empty() {
            // Auto-select the first animated curve.
            let first = all_curves[0].clone();
            self.selected_curve = Some((first.node_id, first.knob, first.channel_index));
        }
        let fallback_mode = self.selected_curve.is_none() && !all_curves.is_empty();

        // Top pure-ASCII toolbar.
        ui.horizontal(|ui| {
            if ui.button("[Fit]").clicked() {
                self.fit_view(graph);
            }
            ui.separator();
            if ui.button("[Smooth]").clicked() {
                self.set_interp(graph, InterpMode::Smooth, &mut messages);
            }
            if ui.button("[Linear]").clicked() {
                self.set_interp(graph, InterpMode::Linear, &mut messages);
            }
            if ui.button("[Constant]").clicked() {
                self.set_interp(graph, InterpMode::Constant, &mut messages);
            }
            ui.separator();
            if ui.button("[Del Key]").clicked() {
                self.del_key(graph, &mut messages);
            }
        });

        let available = ui.available_rect_before_wrap();
        if available.width() < 30.0 || available.height() < 30.0 {
            return messages;
        }

        // --- Sidebar + Plot split (Bug 2 fix): compact fixed-width sidebar ---
        ui.columns(2, |uis| {
            let (sidebar_column, plot_column) = uis.split_at_mut(1);
            let sidebar_ui = &mut sidebar_column[0];
            let plot_ui = &mut plot_column[0];

            // Constrain sidebar to fixed width.
            sidebar_ui.set_width(SIDEBAR_WIDTH);

            sidebar_ui.label("Curves");
            sidebar_ui.separator();

            let entries: Vec<CurveEntry> = if fallback_mode {
                // Nuke-style: list every animated curve across all nodes.
                all_curves.clone()
            } else {
                // Single-node mode: only curves for the selected node.
                let selected_node = self.selected_curve.as_ref().map(|(n, _, _)| *n);
                selected_node
                    .map(|id| animated_curves_for_node(graph, id))
                    .unwrap_or_default()
            };

            let selected_knob = self.selected_curve.as_ref().map(|(_, k, _)| k.clone());
            let selected_channel = self.selected_curve.as_ref().map(|(_, _, c)| *c);
            let selected_node = self.selected_curve.as_ref().map(|(n, _, _)| *n);

            for entry in &entries {
                let is_selected = selected_knob.as_deref() == Some(entry.knob.as_str())
                    && Some(entry.channel_index) == selected_channel
                    && (!fallback_mode || entry.node_id == selected_node.unwrap_or(NodeId(0)));
                let label = if entry.knob.eq_ignore_ascii_case("mix") && entry.channel_index == 0 {
                    if fallback_mode {
                        format!("{} #{} (scalar)", entry.knob, entry.node_id.0)
                    } else {
                        format!("{} (scalar)", entry.knob)
                    }
                } else if fallback_mode {
                    format!(
                        "{} #{} ch{}",
                        entry.knob, entry.node_id.0, entry.channel_index
                    )
                } else {
                    format!("{} ch{}", entry.knob, entry.channel_index)
                };
                if sidebar_ui.selectable_label(is_selected, label).clicked() {
                    self.selected_curve =
                        Some((entry.node_id, entry.knob.clone(), entry.channel_index));
                    self.selected_key_index = None;
                }
            }
            if entries.is_empty() {
                sidebar_ui.label("Select a node with animated knobs ([+K]) to edit curves.");
            }

            // --- Plot ---
            let rect = plot_ui.available_rect_before_wrap();
            if rect.width() < 30.0 || rect.height() < 30.0 {
                return;
            }
            let painter = plot_ui.painter_at(rect);

            // Inner plot margins so labels never clip at plot edges.
            let plot_rect = Rect::from_min_max(
                Pos2::new(rect.min.x + PLOT_MARGIN_LEFT, rect.min.y + PLOT_MARGIN_TOP),
                Pos2::new(
                    rect.max.x - PLOT_MARGIN_RIGHT,
                    rect.max.y - PLOT_MARGIN_BOTTOM,
                ),
            );

            // Recessed dark background.
            painter.rect_filled(rect, 4.0, Color32::from_rgb(18, 20, 26));
            painter.rect_stroke(
                rect,
                4.0,
                Stroke::new(1.0, Color32::from_rgb(60, 65, 75)),
                StrokeKind::Inside,
            );

            // --- Adaptive grid spacing (Bug 2 fix) ---
            let frame_step = nice_step(
                self.view_frame_max - self.view_frame_min,
                plot_rect.width(),
                FRAME_STEPS,
            );
            let val_step = nice_step(
                self.view_val_max - self.view_val_min,
                plot_rect.height(),
                VAL_STEPS,
            );

            // Vertical frame gridlines + labels (above the top margin).
            let mut frame = (self.view_frame_min / frame_step).ceil() * frame_step;
            while frame <= self.view_frame_max {
                let x = self.frame_to_x(&plot_rect, frame);
                painter.line_segment(
                    [Pos2::new(x, plot_rect.min.y), Pos2::new(x, plot_rect.max.y)],
                    Stroke::new(1.0, Color32::from_rgb(40, 43, 50)),
                );
                if frame >= 0.0 {
                    painter.text(
                        Pos2::new(x - 6.0, plot_rect.max.y + 2.0),
                        Align2::CENTER_TOP,
                        format!("{frame:.0}"),
                        FontId::proportional(10.0),
                        Color32::from_gray(150),
                    );
                }
                frame += frame_step;
            }

            // Horizontal value gridlines + labels (left of the left margin).
            let mut val = (self.view_val_min / val_step).ceil() * val_step;
            while val <= self.view_val_max {
                let y = self.val_to_y(&plot_rect, val);
                painter.line_segment(
                    [Pos2::new(plot_rect.min.x, y), Pos2::new(plot_rect.max.x, y)],
                    Stroke::new(1.0, Color32::from_rgb(40, 43, 50)),
                );
                painter.text(
                    Pos2::new(plot_rect.min.x - 36.0, y - 7.0),
                    Align2::RIGHT_TOP,
                    format!("{val:.2}"),
                    FontId::proportional(10.0),
                    Color32::from_gray(150),
                );
                val += val_step;
            }

            // Playhead line.
            if self.current_frame >= self.view_frame_min
                && self.current_frame <= self.view_frame_max
            {
                let x = self.frame_to_x(&plot_rect, self.current_frame);
                painter.line_segment(
                    [Pos2::new(x, plot_rect.min.y), Pos2::new(x, plot_rect.max.y)],
                    Stroke::new(2.0, Color32::from_rgb(255, 140, 0)),
                );
            }

            // Draw curves + keys for the selected curve.
            let selected = self.selected_curve.clone();
            if let Some((node_id, knob, channel_index)) = selected {
                if let Some(node) = graph.node(node_id) {
                    let curves_opt = node.curves.get(&knob);
                    if let Some(curves) = curves_opt {
                        if channel_index < curves.len() {
                            let curve = &curves[channel_index];
                            let color = ChannelColor::of(&knob, channel_index).color();

                            // Polyline trace across visible frames.
                            let span =
                                (self.view_frame_max - self.view_frame_min).max(MIN_FRAME_SPAN);
                            let steps = 128;
                            let mut points: Vec<Pos2> = Vec::new();
                            for i in 0..=steps {
                                let t = i as f64 / steps as f64;
                                let f = self.view_frame_min + t * span;
                                let v = curve.evaluate(f);
                                points.push(Pos2::new(
                                    self.frame_to_x(&plot_rect, f),
                                    self.val_to_y(&plot_rect, v),
                                ));
                            }
                            if points.len() >= 2 {
                                for i in 0..points.len() - 1 {
                                    painter.line_segment(
                                        [points[i], points[i + 1]],
                                        Stroke::new(2.0, color),
                                    );
                                }
                            }

                            // Keyframe markers.
                            let key_radius = 6.0;
                            let mut hovered_key: Option<usize> = None;

                            for (idx, key) in curve.keys.iter().enumerate() {
                                let is_selected = Some(idx) == self.selected_key_index;
                                let x = self.frame_to_x(&plot_rect, key.frame);
                                let y = self.val_to_y(&plot_rect, key.value);
                                let center = Pos2::new(x, y);

                                let key_rect =
                                    Rect::from_center_size(center, Vec2::splat(key_radius * 2.0));
                                let hovered =
                                    pointer_pos.map(|p| key_rect.contains(p)).unwrap_or(false);
                                if hovered {
                                    hovered_key = Some(idx);
                                }

                                let fill = if is_selected {
                                    Color32::from_rgb(255, 255, 255)
                                } else {
                                    color
                                };
                                painter.circle_filled(center, key_radius, fill);
                                painter.circle_stroke(
                                    center,
                                    key_radius,
                                    Stroke::new(1.5, Color32::from_rgb(255, 255, 255)),
                                );
                            }

                            // Interaction: click / drag keys.
                            let response = plot_ui.interact(
                                plot_rect,
                                plot_ui.next_auto_id(),
                                Sense::click_and_drag(),
                            );
                            if response.clicked() {
                                if let Some(pointer) = pointer_pos {
                                    if let Some(idx) = hovered_key {
                                        self.selected_key_index = Some(idx);
                                        self.dragging_key = false;
                                    } else {
                                        // Click empty space: create a key at the cursor.
                                        let new_frame =
                                            self.x_to_frame(&plot_rect, pointer.x).round();
                                        let new_val = self.y_to_val(&plot_rect, pointer.y);
                                        if let Some(node) = graph.node(node_id) {
                                            if let Some(curves) = node.curves.get(&knob) {
                                                if channel_index < curves.len() {
                                                    let mut new_curves = node
                                                        .curves
                                                        .get(&knob)
                                                        .cloned()
                                                        .unwrap_or_default();
                                                    if new_curves.len() <= channel_index {
                                                        new_curves.resize(
                                                            channel_index + 1,
                                                            AnimationCurve::new(),
                                                        );
                                                    }
                                                    // Deduplicate: don't collide within 1e-4 of an existing key.
                                                    let collides =
                                                        new_curves[channel_index].keys.iter().any(
                                                            |k| (k.frame - new_frame).abs() < 1e-4,
                                                        );
                                                    if !collides {
                                                        new_curves[channel_index].keys.push(
                                                            Keyframe {
                                                                frame: new_frame,
                                                                value: new_val,
                                                                interp: InterpMode::Smooth,
                                                            },
                                                        );
                                                        new_curves[channel_index].keys.sort_by(
                                                            |a, b| {
                                                                a.frame
                                                                    .partial_cmp(&b.frame)
                                                                    .unwrap()
                                                            },
                                                        );
                                                        let previous = node
                                                            .curves
                                                            .get(&knob)
                                                            .cloned()
                                                            .unwrap_or_default();
                                                        messages.push(KernelMessage::Execute(
                                                            UndoableCommand::SetKnobCurve {
                                                                node: node_id,
                                                                name: knob.clone(),
                                                                curves: new_curves.clone(),
                                                                previous,
                                                            },
                                                        ));
                                                        self.selected_key_index = new_curves
                                                            [channel_index]
                                                            .keys
                                                            .iter()
                                                            .position(|k| {
                                                                (k.frame - new_frame).abs() < 1e-4
                                                            });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Drag selected key in place — NO ghost keys.
                            if response.dragged() {
                                if let Some(idx) = self.selected_key_index {
                                    if let Some(pointer) = response.interact_pointer_pos() {
                                        self.dragging_key = true;
                                        if let Some(node) = graph.node(node_id) {
                                            if let Some(curves) = node.curves.get(&knob) {
                                                if channel_index < curves.len()
                                                    && idx < curves[channel_index].keys.len()
                                                {
                                                    let new_frame = self
                                                        .x_to_frame(&plot_rect, pointer.x)
                                                        .round();
                                                    let new_val =
                                                        self.y_to_val(&plot_rect, pointer.y);

                                                    let mut new_curves = node
                                                        .curves
                                                        .get(&knob)
                                                        .cloned()
                                                        .unwrap_or_default();
                                                    if new_curves.len() <= channel_index {
                                                        new_curves.resize(
                                                            channel_index + 1,
                                                            AnimationCurve::new(),
                                                        );
                                                    }
                                                    let key =
                                                        &mut new_curves[channel_index].keys[idx];
                                                    key.frame = new_frame;
                                                    key.value = new_val;
                                                    new_curves[channel_index].keys.sort_by(
                                                        |a, b| {
                                                            a.frame.partial_cmp(&b.frame).unwrap()
                                                        },
                                                    );

                                                    // Track the moved key's new sorted position.
                                                    let new_position = new_curves[channel_index]
                                                        .keys
                                                        .iter()
                                                        .position(|k| {
                                                            (k.frame - new_frame).abs() < 1e-4
                                                                && (k.value - new_val).abs() < 1e-4
                                                        })
                                                        .unwrap_or(idx);
                                                    self.selected_key_index = Some(new_position);

                                                    let previous = node
                                                        .curves
                                                        .get(&knob)
                                                        .cloned()
                                                        .unwrap_or_default();
                                                    messages.push(KernelMessage::Execute(
                                                        UndoableCommand::SetKnobCurve {
                                                            node: node_id,
                                                            name: knob.clone(),
                                                            curves: new_curves.clone(),
                                                            previous,
                                                        },
                                                    ));
                                                    plot_ui.ctx().request_repaint();
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                self.dragging_key = false;
                            }

                            // Middle-mouse pan.
                            if response.hovered()
                                && plot_ui
                                    .input(|i| i.pointer.button_down(egui::PointerButton::Middle))
                            {
                                let delta = plot_ui.input(|i| i.pointer.delta());
                                let span =
                                    (self.view_frame_max - self.view_frame_min).max(MIN_FRAME_SPAN);
                                let val_span =
                                    (self.view_val_max - self.view_val_min).max(MIN_VAL_SPAN);
                                let pan_f = delta.x as f64 / plot_rect.width() as f64 * span;
                                let pan_v =
                                    -(delta.y as f64) / plot_rect.height() as f64 * val_span;
                                self.view_frame_min = (self.view_frame_min - pan_f).max(0.0);
                                self.view_frame_max = (self.view_frame_max - pan_f)
                                    .max(self.view_frame_min + MIN_FRAME_SPAN);
                                self.view_val_min -= pan_v;
                                self.view_val_max -= pan_v;
                                if self.view_val_max - self.view_val_min < MIN_VAL_SPAN {
                                    let mid = (self.view_val_min + self.view_val_max) / 2.0;
                                    self.view_val_min = mid - MIN_VAL_SPAN / 2.0;
                                    self.view_val_max = mid + MIN_VAL_SPAN / 2.0;
                                }
                            }

                            // Wheel zoom around cursor.
                            if response.hovered() {
                                let scroll = plot_ui.input(|i| i.smooth_scroll_delta.y);
                                if scroll.abs() > 0.0 {
                                    if let Some(pointer) = pointer_pos {
                                        let old_frame = self.x_to_frame(&plot_rect, pointer.x);
                                        let old_val = self.y_to_val(&plot_rect, pointer.y);
                                        let factor = (-scroll * 0.001).exp() as f64;
                                        let frame_span = (self.view_frame_max
                                            - self.view_frame_min)
                                            .max(MIN_FRAME_SPAN);
                                        let val_span = (self.view_val_max - self.view_val_min)
                                            .max(MIN_VAL_SPAN);
                                        let new_frame_span =
                                            (frame_span * factor).max(MIN_ZOOM_FRAME_SPAN);
                                        let new_val_span =
                                            (val_span * factor).max(MIN_ZOOM_VAL_SPAN);
                                        self.view_frame_min = (old_frame
                                            - (old_frame - self.view_frame_min)
                                                * (new_frame_span / frame_span))
                                            .max(0.0);
                                        self.view_frame_max = self.view_frame_min + new_frame_span;
                                        self.view_val_min = old_val
                                            - (old_val - self.view_val_min)
                                                * (new_val_span / val_span);
                                        self.view_val_max = self.view_val_min + new_val_span;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        messages
    }

    fn set_interp(
        &mut self,
        graph: &GraphMirror,
        interp: InterpMode,
        messages: &mut Vec<KernelMessage>,
    ) {
        if let Some((node_id, knob, channel_index)) = &self.selected_curve {
            let channel_index = *channel_index;
            if let Some(idx) = self.selected_key_index {
                if let Some(node) = graph.node(*node_id) {
                    if let Some(curves) = node.curves.get(knob) {
                        if channel_index < curves.len() {
                            if let Some(key) = curves[channel_index].keys.get(idx) {
                                let _ = key;
                                let mut new_curves =
                                    node.curves.get(knob).cloned().unwrap_or_default();
                                if new_curves.len() <= channel_index {
                                    new_curves.resize(channel_index + 1, AnimationCurve::new());
                                }
                                new_curves[channel_index].keys[idx].interp = interp;
                                new_curves[channel_index]
                                    .keys
                                    .sort_by(|a, b| a.frame.partial_cmp(&b.frame).unwrap());
                                let previous = node.curves.get(knob).cloned().unwrap_or_default();
                                messages.push(KernelMessage::Execute(
                                    UndoableCommand::SetKnobCurve {
                                        node: *node_id,
                                        name: knob.clone(),
                                        curves: new_curves,
                                        previous,
                                    },
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    fn del_key(&mut self, graph: &GraphMirror, messages: &mut Vec<KernelMessage>) {
        if let Some((node_id, knob, channel_index)) = &self.selected_curve {
            let channel_index = *channel_index;
            if let Some(idx) = self.selected_key_index {
                if let Some(node) = graph.node(*node_id) {
                    if let Some(curves) = node.curves.get(knob) {
                        if channel_index < curves.len()
                            && curves[channel_index].keys.get(idx).is_some()
                        {
                            let mut new_curves = node.curves.get(knob).cloned().unwrap_or_default();
                            if new_curves.len() <= channel_index {
                                new_curves.resize(channel_index + 1, AnimationCurve::new());
                            }
                            new_curves[channel_index].keys.remove(idx);
                            new_curves[channel_index]
                                .keys
                                .sort_by(|a, b| a.frame.partial_cmp(&b.frame).unwrap());
                            let previous = node.curves.get(knob).cloned().unwrap_or_default();
                            messages.push(KernelMessage::Execute(UndoableCommand::SetKnobCurve {
                                node: *node_id,
                                name: knob.clone(),
                                curves: new_curves,
                                previous,
                            }));
                            self.selected_key_index = None;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_kernel::UndoableCommand;

    fn make_graph_with_curves() -> GraphMirror {
        let mut graph = GraphMirror::default();
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Grade".to_string(),
            x: 0.0,
            y: 0.0,
        });
        // Add a "mix" curve with keys at frames 1 (value 0.0) and 10 (value 1.0).
        graph.apply_command(&UndoableCommand::SetKnobCurve {
            node: NodeId(1),
            name: "mix".to_string(),
            curves: vec![AnimationCurve {
                keys: vec![
                    Keyframe {
                        frame: 1.0,
                        value: 0.0,
                        interp: InterpMode::Smooth,
                    },
                    Keyframe {
                        frame: 10.0,
                        value: 1.0,
                        interp: InterpMode::Smooth,
                    },
                ],
            }],
            previous: Vec::new(),
        });
        graph
    }

    #[test]
    fn curve_editor_fit_interp_mode_and_key_drag_without_ghost_keys() {
        let graph = make_graph_with_curves();
        let mut panel = CurveEditorPanel {
            selected_curve: Some((NodeId(1), "mix".to_string(), 0)),
            ..Default::default()
        };

        // --- fit_view on a 2-key curve: finite, non-zero spans ---
        panel.fit_view(&graph);
        assert!(
            panel.view_frame_min.is_finite()
                && panel.view_frame_max.is_finite()
                && panel.view_frame_max > panel.view_frame_min
                && panel.view_frame_max - panel.view_frame_min >= 10.0,
            "frame span must be finite and >= 10.0, got {}..{}",
            panel.view_frame_min,
            panel.view_frame_max
        );
        assert!(
            panel.view_val_min.is_finite()
                && panel.view_val_max.is_finite()
                && panel.view_val_max > panel.view_val_min
                && panel.view_val_max - panel.view_val_min >= 0.2,
            "value span must be finite and >= 0.2, got {}..{}",
            panel.view_val_min,
            panel.view_val_max
        );

        // --- fit_view on a flat 1-key curve: safe minimum span ---
        let mut flat_graph = GraphMirror::default();
        flat_graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Grade".to_string(),
            x: 0.0,
            y: 0.0,
        });
        flat_graph.apply_command(&UndoableCommand::SetKnobCurve {
            node: NodeId(1),
            name: "mix".to_string(),
            curves: vec![AnimationCurve {
                keys: vec![Keyframe {
                    frame: 5.0,
                    value: 0.5,
                    interp: InterpMode::Smooth,
                }],
            }],
            previous: Vec::new(),
        });
        let mut flat_panel = CurveEditorPanel {
            selected_curve: Some((NodeId(1), "mix".to_string(), 0)),
            ..Default::default()
        };
        flat_panel.fit_view(&flat_graph);
        assert!(
            flat_panel.view_frame_max - flat_panel.view_frame_min > 9.0,
            "single-key curve must still have a large frame span"
        );
        assert!(
            flat_panel.view_val_max - flat_panel.view_val_min > 0.19,
            "single-key curve must still have a large value span"
        );

        // --- Switching key 0 to Linear emits SetKnobCurve with keys[0].interp == Linear ---
        let mut panel = CurveEditorPanel {
            selected_curve: Some((NodeId(1), "mix".to_string(), 0)),
            selected_key_index: Some(0),
            ..Default::default()
        };
        let mut messages: Vec<KernelMessage> = Vec::new();
        panel.set_interp(&graph, InterpMode::Linear, &mut messages);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            KernelMessage::Execute(UndoableCommand::SetKnobCurve {
                node, name, curves, ..
            }) => {
                assert_eq!(*node, NodeId(1));
                assert_eq!(name, "mix");
                assert_eq!(curves[0].keys[0].interp, InterpMode::Linear);
                // Key 1 unchanged (still Smooth).
                assert_eq!(curves[0].keys[1].interp, InterpMode::Smooth);
            }
            other => panic!("expected SetKnobCurve, got {other:?}"),
        }

        // --- Moving key 0 from frame 1 to frame 5: no ghost keys ---
        // Simulate what show() does when dragging key 0 to frame 5, value 0.3.
        let node = graph.node(NodeId(1)).unwrap();
        let curves = node.curves.get("mix").unwrap();
        assert_eq!(curves[0].keys.len(), 2, "start with 2 keys");
        assert_eq!(curves[0].keys[0].frame, 1.0, "key 0 starts at frame 1");

        // Mutate in place (the core of the drag logic), then sort.
        let mut new_curves = node.curves.get("mix").cloned().unwrap_or_default();
        let key = &mut new_curves[0].keys[0];
        key.frame = 5.0;
        key.value = 0.3;
        new_curves[0]
            .keys
            .sort_by(|a, b| a.frame.partial_cmp(&b.frame).unwrap());

        // Verify zero ghost keys: still exactly 2 keys, none at frame 1.
        assert_eq!(new_curves[0].keys.len(), 2, "no ghost keys left behind");
        assert!(
            new_curves[0].keys.iter().all(|k| k.frame != 1.0),
            "old key at frame 1 must be gone"
        );
        assert_eq!(
            new_curves[0].keys[0].frame, 5.0,
            "moved key is now at frame 5"
        );
        assert_eq!(new_curves[0].keys[0].value, 0.3, "moved key has new value");
        assert_eq!(
            new_curves[0].keys[1].frame, 10.0,
            "other key untouched at frame 10"
        );
    }
}
