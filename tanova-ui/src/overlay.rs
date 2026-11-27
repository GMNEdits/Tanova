//! Shared, extensible Viewer overlays.
//!
//! Every interactive handle (the `Transform` jack, `CornerPin2D` corners and
//! `Ramp` endpoints) lives here so the Viewer has one input path and one place
//! to add the next 2D widget. All coordinates are full-resolution logical
//! pixels, even when the Viewer is displaying a proxy frame.

use egui::{Align2, Color32, FontId, PointerButton, Pos2, Rect, Response, Shape, Stroke, Ui, Vec2};
use tanova_kernel::{KnobValue, NodeId, UndoableCommand};
use tanova_render::{BBox, Mat3x3};

use crate::canvas::{CanvasNode, GraphMirror};
use crate::properties::{effective_knob_value, set_curve_values};

/// Shared overlay wireframe / bounding-box blue.
pub(crate) const BBOX_LINE: Color32 = Color32::from_rgb(110, 200, 255);
const JACK: Color32 = Color32::from_rgb(255, 214, 92);
const JACK_HOVER: Color32 = Color32::from_rgb(255, 238, 170);
const JACK_RADIUS: f32 = 8.0;
const JACK_HIT_RADIUS: f32 = 18.0;
const JACK_ARROW_LENGTH: f32 = 45.0;
const JACK_SHAFT_HIT_RADIUS: f32 = 8.0;
const JACK_X_COLOR: Color32 = Color32::from_rgb(232, 72, 72);
const JACK_Y_COLOR: Color32 = Color32::from_rgb(84, 210, 96);
const CORNER_LINE: Color32 = Color32::from_rgb(255, 175, 55);
const RAMP_LINE: Color32 = Color32::from_rgb(100, 200, 255);
const RAMP_RADIUS: f32 = 7.0;
const HANDLE_HIT_RADIUS: f32 = 12.0;

/// Restricts a jack drag to one Nuke axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisConstraint {
    /// Only horizontal image-space movement.
    X,
    /// Only vertical image-space movement.
    Y,
}

/// In-flight transform jack drag.
///
/// The accumulated translation lives here so the handle and bounding box keep
/// moving even before the kernel round-trips a new `GraphMirror`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JackDragState {
    /// Transform node being dragged.
    pub node_id: NodeId,
    /// Accumulated `[translate_x, translate_y]` in image pixels.
    pub current_translate: [f32; 2],
    /// `None` for free 2D movement, otherwise a single axis.
    pub axis_constraint: Option<AxisConstraint>,
}

/// Proxy-safe mapping between full-resolution logical image space and screen.
///
/// The Viewer builds one of these from the fitted image rectangle and the
/// frame's *logical* size, so every overlay keeps writing full-resolution
/// coordinates while a 1:2 or 1:4 proxy is on screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayViewport {
    /// Screen rectangle occupied by the displayed image.
    pub img_rect: Rect,
    /// Full-resolution logical width.
    pub logical_width: f32,
    /// Full-resolution logical height.
    pub logical_height: f32,
    /// Playhead used to evaluate animated knobs and auto-key edits.
    pub current_frame: f64,
}

impl OverlayViewport {
    /// Creates a viewport from an already-fitted image rectangle.
    pub fn new(
        img_rect: Rect,
        logical_width: f32,
        logical_height: f32,
        current_frame: f64,
    ) -> Self {
        Self {
            img_rect,
            logical_width: logical_width.max(1.0),
            logical_height: logical_height.max(1.0),
            current_frame,
        }
    }

    /// The logical format as a Bottom-Left bbox.
    pub fn logical_format(&self) -> BBox {
        BBox::new(
            0,
            0,
            self.logical_width.round() as i32,
            self.logical_height.round() as i32,
        )
    }

    /// Maps one Bottom-Left logical point into screen space.
    pub fn image_to_screen(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(
            self.img_rect.left() + x * self.img_rect.width() / self.logical_width,
            self.img_rect.bottom() - y * self.img_rect.height() / self.logical_height,
        )
    }

    /// Maps a Bottom-Left logical bbox into screen space.
    pub fn image_to_screen_rect(&self, world: BBox) -> Rect {
        let left = self.image_to_screen(world.min_x as f32, world.max_y as f32);
        let right = self.image_to_screen(world.max_x as f32, world.min_y as f32);
        Rect::from_min_max(left, right)
    }

    /// Inverse of [`OverlayViewport::image_to_screen`].
    pub fn screen_to_image(&self, pointer: Pos2) -> [f32; 2] {
        let u = (pointer.x - self.img_rect.left()) / self.img_rect.width().max(1e-6);
        let v = (pointer.y - self.img_rect.top()) / self.img_rect.height().max(1e-6);
        [u * self.logical_width, (1.0 - v) * self.logical_height]
    }

    /// Converts a screen-space drag delta back into logical units.
    pub fn screen_delta_to_world(&self, delta: Vec2) -> Vec2 {
        Vec2::new(
            delta.x * self.logical_width / self.img_rect.width().max(1e-6),
            -delta.y * self.logical_height / self.img_rect.height().max(1e-6),
        )
    }
}

/// Commands and pointer ownership produced by one overlay pass.
#[derive(Debug, Default)]
pub struct OverlayResponse {
    /// `true` when the overlay handled the primary pointer, so the Viewer must
    /// not start a pan/zoom navigation for this frame.
    pub consumed_pointer: bool,
    /// Undoable commands the interaction requested.
    pub commands: Vec<UndoableCommand>,
}

/// Persistent drag state shared by every Viewer overlay.
#[derive(Debug, Default)]
pub struct OverlayState {
    /// In-flight transform jack drag, if any.
    pub jack_drag: Option<JackDragState>,
    /// In-flight corner-pin handle drag: `(node_id, corner_index)`.
    pub dragging_corner: Option<(NodeId, usize)>,
    /// In-flight corner value in logical pixels.
    pub corner_drag_value: Option<[f32; 2]>,
    /// In-flight ramp endpoint drag: `(node_id, endpoint_index)`.
    pub dragging_ramp: Option<(NodeId, usize)>,
    /// In-flight ramp endpoint value in logical pixels.
    pub ramp_drag_value: Option<[f32; 2]>,
}

impl OverlayState {
    /// `true` while any overlay handle is being dragged.
    pub fn is_dragging(&self) -> bool {
        self.jack_drag.is_some() || self.dragging_corner.is_some() || self.dragging_ramp.is_some()
    }

    /// Drops every in-flight drag.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Drops only the transform jack drag.
    pub fn reset_jack(&mut self) {
        self.jack_drag = None;
    }

    /// Drops only the corner-pin drag.
    pub fn reset_corner(&mut self) {
        self.dragging_corner = None;
        self.corner_drag_value = None;
    }

    /// Drops only the ramp endpoint drag.
    pub fn reset_ramp(&mut self) {
        self.dragging_ramp = None;
        self.ramp_drag_value = None;
    }

    /// Handles input for, and paints, the selected node's overlay.
    ///
    /// Call this before the Viewer's pan/zoom navigation; a `true`
    /// [`OverlayResponse::consumed_pointer`] means the handle owns this frame.
    pub fn show_and_handle(
        &mut self,
        ui: &Ui,
        painter: &egui::Painter,
        response: &Response,
        viewport: OverlayViewport,
        graph: &GraphMirror,
        selected: Option<NodeId>,
    ) -> OverlayResponse {
        let Some(node) = selected.and_then(|id| graph.node(id)) else {
            self.reset();
            return OverlayResponse::default();
        };

        match node.kind.as_str() {
            "Transform" => {
                self.reset_corner();
                self.reset_ramp();
                self.show_transform(ui, painter, response, viewport, node)
            }
            "CornerPin2D" => {
                self.reset_jack();
                self.reset_ramp();
                self.show_corner_pin(ui, painter, response, viewport, node)
            }
            "Ramp" => {
                self.reset_jack();
                self.reset_corner();
                self.show_ramp(ui, painter, response, viewport, node)
            }
            _ => {
                self.reset();
                OverlayResponse::default()
            }
        }
    }

    fn show_transform(
        &mut self,
        ui: &Ui,
        painter: &egui::Painter,
        response: &Response,
        viewport: OverlayViewport,
        node: &CanvasNode,
    ) -> OverlayResponse {
        let knobs = TransformKnobs::from_node(node);
        let mut out = OverlayResponse::default();

        // Begin a drag only when the press landed on the jack or one of its
        // axis arrows.
        if response.drag_started_by(PointerButton::Primary)
            && let Some(pointer) = ui.input(|input| input.pointer.hover_pos())
        {
            let anchor = self.jack_anchor(viewport, &knobs);
            if let Some(constraint) = jack_hit(anchor, pointer) {
                self.jack_drag = Some(JackDragState {
                    node_id: knobs.node_id,
                    current_translate: [knobs.translate[0], knobs.translate[1]],
                    axis_constraint: constraint,
                });
            }
        }

        // Accumulate this frame's pointer delta on top of the in-flight value.
        if response.dragged_by(PointerButton::Primary) {
            let world = viewport.screen_delta_to_world(response.drag_delta());
            if let Some((before, after)) = self.accumulate_jack_drag(knobs.node_id, world) {
                out.commands.push(UndoableCommand::SetKnob {
                    node: knobs.node_id,
                    knob: "translate".to_string(),
                    value: KnobValue::Color([
                        after[0],
                        after[1],
                        knobs.translate[2],
                        knobs.translate[3],
                    ]),
                    previous: KnobValue::Color([
                        before[0],
                        before[1],
                        knobs.translate[2],
                        knobs.translate[3],
                    ]),
                });
            }
        }

        // The dashed box follows the forward matrix (scale 0.5 shrinks),
        // using the in-flight translation while this node is dragged.
        let translate = self.effective_translate(knobs.node_id, knobs.translate);
        let forward = forward_transform_matrix(translate, knobs.center, knobs.rotate, knobs.scale);
        let transformed = viewport.logical_format().transform(&forward);
        draw_dashed_rect(
            painter,
            viewport.image_to_screen_rect(transformed),
            Stroke::new(1.0, BBOX_LINE),
        );

        let anchor = self.jack_anchor(viewport, &knobs);
        let hovered = ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .is_some_and(|pointer| jack_hit(anchor, pointer).is_some())
        });
        let color = if hovered
            || self
                .jack_drag
                .is_some_and(|drag| drag.node_id == knobs.node_id)
        {
            JACK_HOVER
        } else {
            JACK
        };
        draw_jack(painter, anchor, color);

        out.consumed_pointer = self.jack_drag.is_some();

        // Release ends the drag; this frame still draws the final value.
        if ui.input(|input| input.pointer.primary_released()) {
            self.reset_jack();
        }

        out
    }

    /// Image-space translate to draw: the in-flight drag value when it belongs
    /// to `node_id`, otherwise the mirror's value.
    fn effective_translate(&self, node_id: NodeId, graph_translate: [f32; 4]) -> [f32; 4] {
        match self.jack_drag.filter(|drag| drag.node_id == node_id) {
            Some(drag) => [
                drag.current_translate[0],
                drag.current_translate[1],
                graph_translate[2],
                graph_translate[3],
            ],
            None => graph_translate,
        }
    }

    /// Screen position of the jack: `center + translate` in image pixels.
    fn jack_anchor(&self, viewport: OverlayViewport, knobs: &TransformKnobs) -> Pos2 {
        let translate = match self.jack_drag.filter(|drag| drag.node_id == knobs.node_id) {
            Some(drag) => drag.current_translate,
            None => [knobs.translate[0], knobs.translate[1]],
        };
        viewport.image_to_screen(
            knobs.center[0] + translate[0],
            knobs.center[1] + translate[1],
        )
    }

    /// Adds one image-space delta to the active drag.
    ///
    /// Returns `(before, after)` so the caller can emit an exact undo step.
    pub fn accumulate_jack_drag(
        &mut self,
        node_id: NodeId,
        world_delta: Vec2,
    ) -> Option<([f32; 2], [f32; 2])> {
        let drag = self
            .jack_drag
            .as_mut()
            .filter(|drag| drag.node_id == node_id)?;
        let mut dx = world_delta.x;
        let mut dy = world_delta.y;
        match drag.axis_constraint {
            Some(AxisConstraint::X) => dy = 0.0,
            Some(AxisConstraint::Y) => dx = 0.0,
            None => {}
        }
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        let before = drag.current_translate;
        drag.current_translate[0] += dx;
        drag.current_translate[1] += dy;
        Some((before, drag.current_translate))
    }

    fn show_corner_pin(
        &mut self,
        ui: &Ui,
        painter: &egui::Painter,
        response: &Response,
        viewport: OverlayViewport,
        node: &CanvasNode,
    ) -> OverlayResponse {
        let node_id = node.id;
        if self.dragging_corner.is_some_and(|(id, _)| id != node_id) {
            self.reset_corner();
        }
        let format = viewport.logical_format();
        let corners = self.corner_pin_corners(node, format, viewport.current_frame);
        let corner_names = ["to1", "to2", "to3", "to4"];

        // Capture handle presses before navigation, without waiting for egui's
        // drag threshold (which can move the pointer outside the hit zone).
        let pressed = ui.input(|input| input.pointer.primary_pressed());
        let mut started = false;
        if pressed && response.contains_pointer() {
            if let Some(pointer) = ui.input(|input| input.pointer.press_origin()) {
                for (i, &corner) in corners.iter().enumerate() {
                    let screen = viewport.image_to_screen(corner[0], corner[1]);
                    if pointer.distance(screen) <= HANDLE_HIT_RADIUS {
                        self.dragging_corner = Some((node_id, i));
                        self.corner_drag_value = Some(corner);
                        ui.ctx().request_repaint();
                        started = true;
                        break;
                    }
                }
            }
        }

        let mut out = OverlayResponse::default();

        if let Some((_, corner_idx)) = self.dragging_corner {
            let pointer = ui.input(|input| input.pointer.interact_pos());
            if !pressed && let Some(pointer) = pointer {
                let [x, y] = viewport.screen_to_image(pointer);
                let fw = viewport.logical_width;
                let fh = viewport.logical_height;
                let xy = [x.clamp(-fw, 2.0 * fw), y.clamp(-fh, 2.0 * fh)];
                let current = self.corner_drag_value.unwrap_or(corners[corner_idx]);
                if xy.iter().zip(current).any(|(a, b)| (a - b).abs() > 1e-4) {
                    let name = corner_names[corner_idx];
                    out.commands
                        .push(emit_2d_knob_command(node, name, xy, viewport.current_frame));
                    self.corner_drag_value = Some(xy);
                    ui.ctx().request_repaint();
                }
            }
        }

        out.consumed_pointer = started || self.dragging_corner.is_some();

        // Also release capture when the window loses the pointer/button.
        if !ui.input(|input| input.pointer.primary_down()) {
            self.reset_corner();
        }

        // Paint the quad after the image, using the same effective values as
        // hit testing.
        let corners = self.corner_pin_corners(node, format, viewport.current_frame);
        let screen_corners: [Pos2; 4] = corners.map(|c| viewport.image_to_screen(c[0], c[1]));
        for i in 0..4 {
            let next = (i + 1) % 4;
            painter.line_segment(
                [screen_corners[i], screen_corners[next]],
                Stroke::new(1.5, CORNER_LINE),
            );
        }

        for (i, &screen) in screen_corners.iter().enumerate() {
            let is_dragging = self
                .dragging_corner
                .is_some_and(|(n, idx)| n == node_id && idx == i);
            let hovered = ui.input(|input| {
                input
                    .pointer
                    .hover_pos()
                    .is_some_and(|p| p.distance(screen) <= HANDLE_HIT_RADIUS)
            });
            let color = if hovered || is_dragging {
                JACK_HOVER
            } else {
                JACK
            };
            let label = (i + 1).to_string();
            painter.circle_filled(screen, RAMP_RADIUS, color);
            painter.circle_stroke(screen, RAMP_RADIUS, Stroke::new(1.0, Color32::WHITE));
            painter.text(
                screen + Vec2::new(JACK_RADIUS + 2.0, 0.0),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(10.0),
                color,
            );
        }

        out
    }

    /// The effective corner positions in logical pixels.
    pub fn corner_pin_corners(
        &self,
        node: &CanvasNode,
        format: BBox,
        current_frame: f64,
    ) -> [[f32; 2]; 4] {
        let fw = format.width() as f32;
        let fh = format.height() as f32;
        let defaults = [[0.0, 0.0], [fw, 0.0], [fw, fh], [0.0, fh]];
        std::array::from_fn(|i| {
            if self.dragging_corner == Some((node.id, i))
                && let Some(xy) = self.corner_drag_value
            {
                return xy;
            }
            let name = format!("to{}", i + 1);
            let base = node.knobs.get(&name).cloned().unwrap_or(KnobValue::Color([
                defaults[i][0],
                defaults[i][1],
                0.0,
                0.0,
            ]));
            match effective_knob_value(node, &name, &base, current_frame) {
                KnobValue::Color(c) => [c[0], c[1]],
                _ => defaults[i],
            }
        })
    }

    fn show_ramp(
        &mut self,
        ui: &Ui,
        painter: &egui::Painter,
        response: &Response,
        viewport: OverlayViewport,
        node: &CanvasNode,
    ) -> OverlayResponse {
        let node_id = node.id;
        if self.dragging_ramp.is_some_and(|(id, _)| id != node_id) {
            self.reset_ramp();
        }
        let points = self.ramp_points(node, viewport.current_frame);
        let names = ["p0", "p1"];

        let pressed = ui.input(|input| input.pointer.primary_pressed());
        let hover = ui.input(|input| input.pointer.hover_pos());
        let pointer_over = hover.is_some_and(|p| response.rect.contains(p));
        let mut started = false;
        if pressed && pointer_over {
            if let Some(pointer) = ui.input(|input| input.pointer.press_origin()) {
                for (i, &point) in points.iter().enumerate() {
                    let screen = viewport.image_to_screen(point[0], point[1]);
                    if pointer.distance(screen) <= HANDLE_HIT_RADIUS {
                        self.dragging_ramp = Some((node_id, i));
                        self.ramp_drag_value = Some(point);
                        ui.ctx().request_repaint();
                        started = true;
                        break;
                    }
                }
            }
        }

        let mut out = OverlayResponse::default();

        if let Some((_, point_idx)) = self.dragging_ramp {
            let pointer = ui.input(|input| input.pointer.interact_pos());
            if !pressed && let Some(pointer) = pointer {
                let xy = viewport.screen_to_image(pointer);
                let current = self.ramp_drag_value.unwrap_or(points[point_idx]);
                if xy.iter().zip(current).any(|(a, b)| (a - b).abs() > 1e-4) {
                    out.commands.push(emit_2d_knob_command(
                        node,
                        names[point_idx],
                        xy,
                        viewport.current_frame,
                    ));
                    self.ramp_drag_value = Some(xy);
                    ui.ctx().request_repaint();
                }
            }
        }

        out.consumed_pointer = started || self.dragging_ramp.is_some();

        if !ui.input(|input| input.pointer.primary_down()) {
            self.reset_ramp();
        }

        // Paint the ramp guide and its two endpoint handles.
        let points = self.ramp_points(node, viewport.current_frame);
        let screen_points: [Pos2; 2] = points.map(|p| viewport.image_to_screen(p[0], p[1]));
        painter.line_segment(
            [screen_points[0], screen_points[1]],
            Stroke::new(1.5, RAMP_LINE),
        );

        for (i, &screen) in screen_points.iter().enumerate() {
            let is_dragging = self
                .dragging_ramp
                .is_some_and(|(n, idx)| n == node_id && idx == i);
            let hovered = ui.input(|input| {
                input
                    .pointer
                    .hover_pos()
                    .is_some_and(|p| p.distance(screen) <= HANDLE_HIT_RADIUS)
            });
            let color = if hovered || is_dragging {
                JACK_HOVER
            } else {
                RAMP_LINE
            };
            painter.circle_filled(screen, RAMP_RADIUS, color);
            painter.circle_stroke(screen, RAMP_RADIUS, Stroke::new(1.0, Color32::WHITE));
            painter.text(
                screen + Vec2::new(RAMP_RADIUS + 2.0, 0.0),
                Align2::LEFT_CENTER,
                if i == 0 { "P0" } else { "P1" },
                FontId::proportional(10.0),
                color,
            );
        }

        out
    }

    fn ramp_points(&self, node: &CanvasNode, current_frame: f64) -> [[f32; 2]; 2] {
        std::array::from_fn(|i| {
            if self.dragging_ramp == Some((node.id, i))
                && let Some(xy) = self.ramp_drag_value
            {
                return xy;
            }
            let name = if i == 0 { "p0" } else { "p1" };
            let base = node
                .knobs
                .get(name)
                .cloned()
                .unwrap_or(KnobValue::Color([0.0; 4]));
            match effective_knob_value(node, name, &base, current_frame) {
                KnobValue::Color(c) => [c[0], c[1]],
                _ => [0.0, 0.0],
            }
        })
    }
}

/// Knobs the transform overlay needs from the selected node.
struct TransformKnobs {
    node_id: NodeId,
    translate: [f32; 4],
    center: [f32; 4],
    rotate: f32,
    scale: f32,
}

impl TransformKnobs {
    fn from_node(node: &CanvasNode) -> Self {
        let translate = match node.knobs.get("translate") {
            Some(KnobValue::Color(color)) => *color,
            _ => [0.0, 0.0, 0.0, 1.0],
        };
        let center = match node.knobs.get("center") {
            Some(KnobValue::Color(color)) => *color,
            _ => [0.0, 0.0, 0.0, 1.0],
        };
        let rotate = match node.knobs.get("rotate") {
            Some(KnobValue::Float(value)) => *value as f32,
            _ => 0.0,
        };
        let scale = match node.knobs.get("scale") {
            Some(KnobValue::Float(value)) => *value as f32,
            _ => 1.0,
        };
        Self {
            node_id: node.id,
            translate,
            center,
            rotate,
            scale,
        }
    }
}

/// Builds the undoable edit for one 2D knob (XY plus the untouched alpha
/// channels), auto-keying the knob's existing curves at `current_frame`.
pub fn emit_2d_knob_command(
    node: &CanvasNode,
    knob_name: &str,
    new_xy: [f32; 2],
    current_frame: f64,
) -> UndoableCommand {
    let previous = node
        .knobs
        .get(knob_name)
        .cloned()
        .unwrap_or(KnobValue::Color([0.0; 4]));
    let prev = match &previous {
        KnobValue::Color(c) => *c,
        _ => [0.0; 4],
    };
    let value = KnobValue::Color([new_xy[0], new_xy[1], prev[2], prev[3]]);
    let set = UndoableCommand::SetKnob {
        node: node.id,
        knob: knob_name.to_string(),
        value: value.clone(),
        previous,
    };

    match node.curves.get(knob_name) {
        Some(previous_curves) if previous_curves.iter().any(|curve| !curve.is_empty()) => {
            UndoableCommand::Batch(vec![
                set,
                UndoableCommand::SetKnobCurve {
                    node: node.id,
                    name: knob_name.to_string(),
                    curves: set_curve_values(previous_curves.clone(), &value, current_frame),
                    previous: previous_curves.clone(),
                },
            ])
        }
        _ => set,
    }
}

/// Forward matrix `T(translate) * T(center) * S(scale) * R(rotate) * T(-center)`.
///
/// Mirrors the cartridge's `transform_matrix`, so the overlay box shrinks when
/// the render shrinks.
pub fn forward_transform_matrix(
    translate: [f32; 4],
    center: [f32; 4],
    rotate_degrees: f32,
    scale: f32,
) -> Mat3x3 {
    Mat3x3::translate(translate[0], translate[1])
        .mul(&Mat3x3::translate(center[0], center[1]))
        .mul(&Mat3x3::scale(scale, scale))
        .mul(&Mat3x3::rotate(rotate_degrees.to_radians()))
        .mul(&Mat3x3::translate(-center[0], -center[1]))
}

/// Hit-tests the jack and its axis arrows.
///
/// `Some(None)` is a free 2D grab, `Some(Some(axis))` is an axis-locked grab,
/// and `None` means the pointer missed the jack entirely.
pub fn jack_hit(anchor: Pos2, pointer: Pos2) -> Option<Option<AxisConstraint>> {
    if pointer.distance(anchor) <= JACK_HIT_RADIUS {
        return Some(None);
    }

    let x_tip = anchor + Vec2::new(JACK_ARROW_LENGTH, 0.0);
    if distance_to_segment(pointer, anchor, x_tip) <= JACK_SHAFT_HIT_RADIUS {
        return Some(Some(AxisConstraint::X));
    }

    let y_tip = anchor - Vec2::new(0.0, JACK_ARROW_LENGTH);
    if distance_to_segment(pointer, anchor, y_tip) <= JACK_SHAFT_HIT_RADIUS {
        return Some(Some(AxisConstraint::Y));
    }

    None
}

fn distance_to_segment(point: Pos2, start: Pos2, end: Pos2) -> f32 {
    let segment = end - start;
    let length_sq = segment.length_sq();
    if length_sq <= f32::EPSILON {
        return point.distance(start);
    }
    let t = ((point - start).dot(segment) / length_sq).clamp(0.0, 1.0);
    point.distance(start + segment * t)
}

/// Draws a Nuke-style transform jack: red `+X`, green `+Y`, yellow pivot.
fn draw_jack(painter: &egui::Painter, position: Pos2, color: Color32) {
    // Nuke +X: red arrow to the right.
    let x_tip = position + Vec2::new(JACK_ARROW_LENGTH, 0.0);
    painter.line_segment([position, x_tip], Stroke::new(2.0, JACK_X_COLOR));
    painter.add(Shape::convex_polygon(
        vec![
            x_tip,
            x_tip - Vec2::new(8.0, 4.5),
            x_tip - Vec2::new(8.0, -4.5),
        ],
        JACK_X_COLOR,
        Stroke::NONE,
    ));

    // Nuke +Y: green arrow upward (screen -Y).
    let y_tip = position - Vec2::new(0.0, JACK_ARROW_LENGTH);
    painter.line_segment([position, y_tip], Stroke::new(2.0, JACK_Y_COLOR));
    painter.add(Shape::convex_polygon(
        vec![
            y_tip,
            y_tip + Vec2::new(-4.5, 8.0),
            y_tip + Vec2::new(4.5, 8.0),
        ],
        JACK_Y_COLOR,
        Stroke::NONE,
    ));

    // Yellow centre ring + crosshair.
    let stroke = Stroke::new(1.5, color);
    painter.circle_stroke(position, JACK_RADIUS, stroke);
    painter.line_segment(
        [
            position - Vec2::new(14.0, 0.0),
            position + Vec2::new(14.0, 0.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            position - Vec2::new(0.0, 14.0),
            position + Vec2::new(0.0, 14.0),
        ],
        stroke,
    );
    painter.circle_filled(position, 2.0, color);
    painter.text(
        position + Vec2::new(12.0, -12.0),
        Align2::LEFT_BOTTOM,
        "Transform",
        FontId::proportional(10.0),
        color,
    );
}

/// Draws a dashed rectangle (used by the data-window outline and the
/// forward-mapped Transform box).
pub(crate) fn draw_dashed_rect(painter: &egui::Painter, rect: Rect, stroke: Stroke) {
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    painter.extend(egui::Shape::dashed_line(&corners, stroke, 5.0, 4.0));
}
