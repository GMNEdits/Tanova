//! The live Nuke-style viewer widget.
//!
//! The panel owns only display state (viewer target, exposure, gamma, pan,
//! zoom, the last uploaded texture). Frames arrive from the kernel thread as
//! [`FrameNotification`]s; the UI never evaluates the DAG itself.

use std::sync::Arc;

use egui::{
    Align, Align2, Color32, ColorImage, FontId, Layout, PointerButton, Pos2, Rect, Sense, Shape,
    Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2,
};
use tanova_kernel::{FrameNotification, KernelMessage, KnobValue, NodeId, Packet, UndoableCommand};
use tanova_render::{BBox, Mat3x3, Raster2D};

use crate::canvas::GraphMirror;

const VIEWER_BG: Color32 = Color32::from_rgb(14, 14, 17);
const CHECKER_DARK: Color32 = Color32::from_rgb(26, 26, 30);
const CHECKER_LIGHT: Color32 = Color32::from_rgb(34, 34, 40);
const FORMAT_LINE: Color32 = Color32::from_rgb(150, 150, 165);
const BBOX_LINE: Color32 = Color32::from_rgb(110, 200, 255);
const HUD_BG: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 190);
const VIEWER: Color32 = Color32::from_rgb(255, 190, 40);
const JACK: Color32 = Color32::from_rgb(255, 214, 92);
const JACK_HOVER: Color32 = Color32::from_rgb(255, 238, 170);

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;
const JACK_RADIUS: f32 = 8.0;
const JACK_HIT_RADIUS: f32 = 18.0;
const JACK_ARROW_LENGTH: f32 = 45.0;
const JACK_SHAFT_HIT_RADIUS: f32 = 8.0;
const JACK_X_COLOR: Color32 = Color32::from_rgb(232, 72, 72);
const JACK_Y_COLOR: Color32 = Color32::from_rgb(84, 210, 96);

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

/// Height reserved at the bottom of the viewer for the timeline strip.
pub const TIMELINE_HEIGHT: f32 = 32.0;

/// Visual-only viewer state plus the last frame the kernel delivered.
pub struct ViewerPanel {
    viewer_node: Option<NodeId>,
    /// Viewer exposure multiplier applied before gamma.
    pub gain: f32,
    /// Viewer display gamma.
    pub gamma: f32,
    /// Screen-space pan of the image inside the viewer rect.
    pub pan: Vec2,
    /// Image zoom relative to the fitted size.
    pub zoom: f32,
    frame: Option<Arc<Packet>>,
    texture: Option<TextureHandle>,
    dirty: bool,
    /// In-flight transform jack drag, if any.
    jack_drag: Option<JackDragState>,
}

impl Default for ViewerPanel {
    fn default() -> Self {
        Self {
            viewer_node: None,
            gain: 1.0,
            gamma: 1.0,
            pan: Vec2::ZERO,
            zoom: 1.0,
            frame: None,
            texture: None,
            dirty: true,
            jack_drag: None,
        }
    }
}

impl ViewerPanel {
    /// Creates an empty viewer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current viewer target.
    pub fn viewer_node(&self) -> Option<NodeId> {
        self.viewer_node
    }

    /// Binds the viewer to a node (or clears it).
    ///
    /// The previous frame is dropped so a stale image never stands in for the
    /// new target while the kernel renders.
    pub fn bind(&mut self, node: Option<NodeId>) {
        if self.viewer_node == node {
            return;
        }
        self.viewer_node = node;
        self.clear_frame();
        self.dirty = true;
    }

    /// Accepts a frame from the kernel, ignoring frames for other targets.
    ///
    /// A cleared notification (empty packet) drops the stored image so the
    /// viewer shows only the checkerboard instead of stale pixels.
    pub fn set_frame(&mut self, frame: &FrameNotification) {
        if frame.node != self.viewer_node {
            return;
        }
        if frame.is_cleared() {
            self.clear_frame();
            return;
        }
        self.frame = Some(Arc::clone(&frame.packet));
        self.dirty = true;
    }

    /// Drops the last image and its GPU texture.
    ///
    /// The transform jack is reset too: it must never hover over an empty
    /// viewport, because there is no image space to map it into.
    pub fn clear_frame(&mut self) {
        let had_content = self.frame.is_some() || self.texture.is_some();
        self.frame = None;
        self.texture = None;
        self.reset_jack_drag();
        if had_content {
            self.dirty = true;
        }
    }

    /// `true` when a displayable image is loaded.
    pub fn has_frame(&self) -> bool {
        self.frame
            .as_ref()
            .is_some_and(|packet| packet.get::<Raster2D>().is_some())
    }

    /// The last delivered raster, if any.
    pub fn raster(&self) -> Option<&Raster2D> {
        self.frame
            .as_ref()
            .and_then(|packet| packet.get::<Raster2D>())
    }

    /// Draws the viewer and uploads the texture when the frame or display
    /// settings changed.
    ///
    /// Returns every kernel message the overlay interaction requested. The
    /// transform jack reads its position from `graph` every frame, so it tracks
    /// knob edits without waiting for a new image from the kernel.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        graph: &GraphMirror,
        selected: Option<NodeId>,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();
        self.show_toolbar(ui);

        // Leave room for the timeline strip the app draws below the image.
        //
        // `available_size` already excludes the spacing after the toolbar, so
        // subtract the timeline plus the spacing that will follow the image.
        // Without this the content is one `item_spacing` taller than the panel
        // every frame, which makes a resizable panel grow without bound.
        let available = ui.available_size();
        let reserved = TIMELINE_HEIGHT + ui.spacing().item_spacing.y;
        let image_size = Vec2::new(available.x, (available.y - reserved).max(1.0));
        let (rect, response) = ui.allocate_exact_size(image_size, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, VIEWER_BG);
        draw_checkerboard(&painter, rect);

        self.handle_navigation(ui, &response);

        let frame = self.frame.clone();
        let Some(raster) = frame.as_ref().and_then(|packet| packet.get::<Raster2D>()) else {
            self.reset_jack_drag();
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                match self.viewer_node {
                    Some(node) => format!("Waiting for a frame from #{}", node.0),
                    None => "No viewer target: select a node and press 1".to_string(),
                },
                FontId::proportional(13.0),
                Color32::from_rgb(130, 130, 140),
            );
            return messages;
        };

        if self.dirty || self.texture.is_none() {
            self.upload(ui.ctx(), raster);
        }

        let format = raster.format.to_bbox();
        let image_rect = self.image_rect(rect, raster);
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));

        if let Some(texture) = &self.texture {
            painter.image(texture.id(), image_rect, uv, Color32::WHITE);
        }

        // Format outline and label.
        painter.rect_stroke(
            image_rect,
            0,
            Stroke::new(1.0, FORMAT_LINE),
            StrokeKind::Inside,
        );
        painter.text(
            image_rect.left_top() + Vec2::new(4.0, 3.0),
            Align2::LEFT_TOP,
            format!("{}x{}", raster.format.width, raster.format.height),
            FontId::proportional(10.0),
            FORMAT_LINE,
        );

        // Active data window (may differ from the format, e.g. after a crop).
        // A selected Transform draws its own forward-mapped box instead, so
        // the two outlines never overlap.
        let transform_selected = selected
            .and_then(|id| graph.node(id))
            .is_some_and(|node| node.kind == "Transform");
        if !transform_selected && raster.bbox != format {
            let bbox_rect = self.world_to_screen(image_rect, format, raster.bbox);
            draw_dashed_rect(&painter, bbox_rect, Stroke::new(1.0, BBOX_LINE));
        }

        self.draw_hud(ui, &painter, rect, image_rect, format, raster);
        messages.extend(
            self.draw_transform_overlay(
                ui, &painter, &response, image_rect, format, graph, selected,
            ),
        );
        messages
    }

    /// Draws the timeline strip below the image.
    ///
    /// The app owns the actual `TimelineState` and passes its fields so the
    /// viewer crate stays independent of the application shell.
    pub fn show_timeline(
        &mut self,
        ui: &mut Ui,
        start: i64,
        end: i64,
        current: &mut f64,
        playing: &mut bool,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };

        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), TIMELINE_HEIGHT),
            Layout::left_to_right(Align::Center),
            |ui| {
                if ui.button(if *playing { "Pause" } else { "Play" }).clicked() {
                    *playing = !*playing;
                }

                let mut frame = current.round() as i64;
                let slider = ui.add(
                    egui::Slider::new(&mut frame, start..=end)
                        .text("frame")
                        .integer(),
                );
                if slider.changed() {
                    *current = frame as f64;
                    messages.push(KernelMessage::SetTime(*current));
                }

                ui.separator();
                ui.label(format!("{:.0} / {}", current.round(), end));
            },
        );

        messages
    }

    fn show_toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.colored_label(VIEWER, "VIEWER 1");
            match self.viewer_node {
                Some(node) => ui.label(format!("#{}", node.0)),
                None => ui.weak("(unbound)"),
            };
            ui.separator();

            let gain = ui.add(
                egui::Slider::new(&mut self.gain, 0.0..=4.0)
                    .text("gain")
                    .fixed_decimals(2),
            );
            if gain.changed() {
                self.dirty = true;
            }
            let gamma = ui.add(
                egui::Slider::new(&mut self.gamma, 0.2..=4.0)
                    .text("gamma")
                    .fixed_decimals(2),
            );
            if gamma.changed() {
                self.dirty = true;
            }
            ui.separator();
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            if ui.button("Fit").clicked() {
                self.zoom = 1.0;
                self.pan = Vec2::ZERO;
            }
        });
    }

    fn handle_navigation(&mut self, ui: &Ui, response: &egui::Response) {
        if response.dragged_by(egui::PointerButton::Middle)
            || response.dragged_by(egui::PointerButton::Secondary)
        {
            self.pan += response.drag_delta();
        }

        if response.hovered() {
            let (scroll, zoom_delta) = ui.input(|input| {
                (
                    input.smooth_scroll_delta.y + input.smooth_scroll_delta.x,
                    input.zoom_delta(),
                )
            });
            if scroll != 0.0 {
                self.zoom = (self.zoom * (1.0 + scroll * 0.002)).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            if (zoom_delta - 1.0).abs() > f32::EPSILON {
                self.zoom = (self.zoom * zoom_delta).clamp(MIN_ZOOM, MAX_ZOOM);
            }
        }
    }

    /// Fits the raster into the viewer rect, then applies zoom and pan.
    fn image_rect(&self, rect: Rect, raster: &Raster2D) -> Rect {
        let format = raster.format.to_bbox();
        let width = format.width().max(1) as f32;
        let height = format.height().max(1) as f32;
        let padding = 24.0;
        let available = Vec2::new(
            (rect.width() - padding).max(1.0),
            (rect.height() - padding).max(1.0),
        );
        let fit = (available.x / width).min(available.y / height);
        let scale = (fit * self.zoom).max(1e-4);
        let size = Vec2::new(width * scale, height * scale);
        Rect::from_center_size(rect.center() + self.pan, size)
    }

    /// Maps a Bottom-Left world bbox into the displayed image rect.
    fn world_to_screen(&self, image_rect: Rect, format: BBox, world: BBox) -> Rect {
        let width = format.width().max(1) as f32;
        let height = format.height().max(1) as f32;
        let scale_x = image_rect.width() / width;
        let scale_y = image_rect.height() / height;

        let left = image_rect.left() + (world.min_x - format.min_x) as f32 * scale_x;
        let right = image_rect.left() + (world.max_x - format.min_x) as f32 * scale_x;
        let bottom = image_rect.bottom() - (world.min_y - format.min_y) as f32 * scale_y;
        let top = image_rect.bottom() - (world.max_y - format.min_y) as f32 * scale_y;
        Rect::from_min_max(Pos2::new(left, top), Pos2::new(right, bottom))
    }

    /// Maps one Bottom-Left world point into the displayed image rect.
    fn world_to_screen_point(&self, image_rect: Rect, format: BBox, x: f32, y: f32) -> Pos2 {
        let width = format.width().max(1) as f32;
        let height = format.height().max(1) as f32;
        let scale_x = image_rect.width() / width;
        let scale_y = image_rect.height() / height;

        Pos2::new(
            image_rect.left() + (x - format.min_x as f32) * scale_x,
            image_rect.bottom() - (y - format.min_y as f32) * scale_y,
        )
    }

    /// Converts a screen-space drag delta back into world units.
    fn screen_delta_to_world(&self, image_rect: Rect, format: BBox, delta: Vec2) -> Vec2 {
        let width = format.width().max(1) as f32;
        let height = format.height().max(1) as f32;
        let scale_x = (image_rect.width() / width).max(1e-6);
        let scale_y = (image_rect.height() / height).max(1e-6);
        Vec2::new(delta.x / scale_x, -delta.y / scale_y)
    }

    /// Draws the selected Transform node's Nuke-style jack and turns primary
    /// drags into `SetKnob` commands for the `translate` knob.
    ///
    /// While a drag is in flight the handle and the dashed box read
    /// `jack_drag.current_translate`, so they never wait for `GraphMirror`.
    #[allow(clippy::too_many_arguments)]
    fn draw_transform_overlay(
        &mut self,
        ui: &Ui,
        painter: &egui::Painter,
        response: &egui::Response,
        image_rect: Rect,
        format: BBox,
        graph: &GraphMirror,
        selected: Option<NodeId>,
    ) -> Vec<KernelMessage> {
        let Some(knobs) = selected_transform(graph, selected) else {
            self.reset_jack_drag();
            return Vec::new();
        };
        let node_id = knobs.node_id;

        // Begin a drag only when the press landed on the jack or one of its
        // axis arrows.
        if response.drag_started_by(PointerButton::Primary) {
            if let Some(pointer) = ui.input(|input| input.pointer.hover_pos()) {
                let anchor = self.jack_anchor(image_rect, format, &knobs, self.jack_drag);
                if let Some(constraint) = jack_hit(anchor, pointer) {
                    self.jack_drag = Some(JackDragState {
                        node_id,
                        current_translate: [knobs.translate[0], knobs.translate[1]],
                        axis_constraint: constraint,
                    });
                }
            }
        }

        let mut messages = Vec::new();

        // Accumulate this frame's pointer delta on top of the in-flight value.
        if response.dragged_by(PointerButton::Primary) {
            let delta = response.drag_delta();
            let world = self.screen_delta_to_world(image_rect, format, delta);
            if let Some((before, after)) = self.accumulate_jack_drag(node_id, world) {
                messages.push(KernelMessage::Execute(UndoableCommand::SetKnob {
                    node: node_id,
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
                }));
            }
        }

        // The dashed box follows the forward matrix (scale 0.5 shrinks),
        // using the in-flight translation while this node is dragged.
        let translate = self.effective_translate(node_id, knobs.translate);
        let forward = forward_transform_matrix(translate, knobs.center, knobs.rotate, knobs.scale);
        let transformed = format.transform(&forward);
        draw_dashed_rect(
            painter,
            self.world_to_screen(image_rect, format, transformed),
            Stroke::new(1.0, BBOX_LINE),
        );

        let anchor = self.jack_anchor(image_rect, format, &knobs, self.jack_drag);
        let hovered = ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .is_some_and(|pointer| jack_hit(anchor, pointer).is_some())
        });
        let color = if hovered || self.jack_drag.is_some_and(|drag| drag.node_id == node_id) {
            JACK_HOVER
        } else {
            JACK
        };
        draw_jack(painter, anchor, color);

        // Release ends the drag; this frame still draws the final value.
        if ui.input(|input| input.pointer.primary_released()) {
            self.reset_jack_drag();
        }

        messages
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
    fn jack_anchor(
        &self,
        image_rect: Rect,
        format: BBox,
        knobs: &TransformKnobs,
        drag: Option<JackDragState>,
    ) -> Pos2 {
        let translate = match drag.filter(|drag| drag.node_id == knobs.node_id) {
            Some(drag) => drag.current_translate,
            None => [knobs.translate[0], knobs.translate[1]],
        };
        self.world_to_screen_point(
            image_rect,
            format,
            knobs.center[0] + translate[0],
            knobs.center[1] + translate[1],
        )
    }

    /// Adds one image-space delta to the active drag.
    ///
    /// Returns `(before, after)` so the caller can emit an exact undo step.
    fn accumulate_jack_drag(
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

    fn reset_jack_drag(&mut self) {
        self.jack_drag = None;
    }

    fn upload(&mut self, ctx: &egui::Context, raster: &Raster2D) {
        let width = raster.format.width.max(1) as usize;
        let height = raster.format.height.max(1) as usize;
        let bytes =
            raster.to_top_left_rgba8_with_display(raster.format.to_bbox(), self.gain, self.gamma);
        let image = ColorImage::from_rgba_unmultiplied([width, height], &bytes);

        match &mut self.texture {
            Some(texture) => texture.set(image, TextureOptions::NEAREST),
            None => {
                self.texture =
                    Some(ctx.load_texture("tanova_viewer", image, TextureOptions::NEAREST));
            }
        }
        self.dirty = false;
    }

    fn draw_hud(
        &self,
        ui: &Ui,
        painter: &egui::Painter,
        rect: Rect,
        image_rect: Rect,
        format: BBox,
        raster: &Raster2D,
    ) {
        let pointer = ui.input(|input| input.pointer.hover_pos());
        let mut text = String::from("XY: -- | RGBA: --");

        if let Some(pointer) = pointer {
            if image_rect.contains(pointer) {
                let width = format.width().max(1) as f32;
                let height = format.height().max(1) as f32;
                let u = (pointer.x - image_rect.left()) / image_rect.width().max(1e-6);
                let v = (pointer.y - image_rect.top()) / image_rect.height().max(1e-6);
                let x = format.min_x + (u * width).floor() as i32;
                // Screen +Y points down; Nuke pixel coordinates point up.
                let y = format.min_y + ((1.0 - v) * height).floor() as i32;
                let [r, g, b, a] = raster.sample_rgba(x, y);
                text = format!("XY: ({x}, {y}) | RGBA: ({r:.4}, {g:.4}, {b:.4}, {a:.4})");
            }
        }

        let font = FontId::monospace(11.0);
        let galley = painter.layout_no_wrap(text, font, Color32::from_rgb(220, 220, 225));
        let padding = Vec2::new(6.0, 3.0);
        let hud = Rect::from_min_size(
            rect.left_bottom() + Vec2::new(6.0, -galley.size().y - 6.0),
            galley.size() + padding * 2.0,
        );
        painter.rect_filled(hud, 3, HUD_BG);
        painter.galley(hud.min + padding, galley, Color32::WHITE);
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

/// Reads the selected node's Transform knobs, if it is a `Transform`.
fn selected_transform(graph: &GraphMirror, selected: Option<NodeId>) -> Option<TransformKnobs> {
    let id = selected?;
    let node = graph.node(id)?;
    if node.kind != "Transform" {
        return None;
    }

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
    Some(TransformKnobs {
        node_id: id,
        translate,
        center,
        rotate,
        scale,
    })
}

/// Forward matrix `T(translate) * T(center) * S(scale) * R(rotate) * T(-center)`.
///
/// Mirrors the cartridge's `transform_matrix`, so the overlay box shrinks when
/// the render shrinks.
fn forward_transform_matrix(
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
fn jack_hit(anchor: Pos2, pointer: Pos2) -> Option<Option<AxisConstraint>> {
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

fn draw_checkerboard(painter: &egui::Painter, rect: Rect) {
    let step = 16.0;
    let mut row = 0;
    let mut y = rect.top();
    while y < rect.bottom() {
        let mut col = 0;
        let mut x = rect.left();
        while x < rect.right() {
            let color = if (row + col) % 2 == 0 {
                CHECKER_DARK
            } else {
                CHECKER_LIGHT
            };
            let cell = Rect::from_min_max(
                Pos2::new(x, y),
                Pos2::new((x + step).min(rect.right()), (y + step).min(rect.bottom())),
            );
            painter.rect_filled(cell, 0, color);
            x += step;
            col += 1;
        }
        y += step;
        row += 1;
    }
}

fn draw_dashed_rect(painter: &egui::Painter, rect: Rect, stroke: Stroke) {
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    painter.extend(egui::Shape::dashed_line(&corners, stroke, 5.0, 4.0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tanova_render::{EdgeMode, Format};

    #[test]
    fn fit_uses_the_format_not_the_data_window() {
        let viewer = ViewerPanel::new();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let raster = Raster2D::new(
            Format::new("fit", 256, 256, 1.0),
            BBox::new(0, 0, 128, 128),
            EdgeMode::BlackOutside,
        );

        let image_rect = viewer.image_rect(rect, &raster);
        assert!(
            (image_rect.width() - 576.0).abs() < 1e-3,
            "Fit must use the 256x256 format, got {}",
            image_rect.width()
        );
        assert!((image_rect.height() - 576.0).abs() < 1e-3);
    }

    #[test]
    fn world_points_map_bottom_left_to_screen() {
        let viewer = ViewerPanel::new();
        let image_rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(200.0, 100.0));
        let format = BBox::new(0, 0, 100, 50);

        assert_eq!(
            viewer.world_to_screen_point(image_rect, format, 0.0, 0.0),
            Pos2::new(0.0, 100.0),
            "world origin is the bottom-left of the image"
        );
        assert_eq!(
            viewer.world_to_screen_point(image_rect, format, 50.0, 25.0),
            Pos2::new(100.0, 50.0),
            "the format center maps to the image center"
        );
    }

    #[test]
    fn screen_deltas_invert_the_pan_and_zoom_scale() {
        let viewer = ViewerPanel::new();
        let image_rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(200.0, 100.0));
        let format = BBox::new(0, 0, 100, 50);

        let world = viewer.screen_delta_to_world(image_rect, format, Vec2::new(10.0, -10.0));
        assert!((world.x - 5.0).abs() < 1e-6, "world x was {}", world.x);
        assert!((world.y - 5.0).abs() < 1e-6, "world y was {}", world.y);
    }

    #[test]
    fn forward_bbox_shrinks_and_moves_with_scale_and_translate() {
        let format = BBox::new(0, 0, 256, 256);

        let shrunk = forward_transform_matrix([0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.0, 1.0], 0.0, 0.5);
        assert_eq!(
            format.transform(&shrunk),
            BBox::new(0, 0, 128, 128),
            "scale 0.5 must shrink the overlay, never grow it"
        );

        let moved =
            forward_transform_matrix([64.0, 64.0, 0.0, 1.0], [0.0, 0.0, 0.0, 1.0], 0.0, 0.5);
        assert_eq!(format.transform(&moved), BBox::new(64, 64, 192, 192));
    }

    #[test]
    fn jack_drag_accumulates_across_frames_and_inverts_screen_y() {
        let mut viewer = ViewerPanel::new();
        let image_rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(200.0, 100.0));
        let format = BBox::new(0, 0, 100, 50);
        let node = NodeId(7);
        viewer.jack_drag = Some(JackDragState {
            node_id: node,
            current_translate: [0.0, 0.0],
            axis_constraint: None,
        });

        // Screen +Y is down, Nuke +Y is up: an upward drag increases translate Y.
        let first = viewer.screen_delta_to_world(image_rect, format, Vec2::new(0.0, -10.0));
        let (before, after) = viewer
            .accumulate_jack_drag(node, first)
            .expect("first drag frame");
        assert_eq!(before, [0.0, 0.0]);
        assert!(
            after[1] > 0.0,
            "screen_dy < 0 must produce translate_y > 0, got {after:?}"
        );
        assert!((after[1] - 5.0).abs() < 1e-6, "world y was {}", after[1]);

        // The next frame must build on the first, not reset to the mirror value.
        let second = viewer.screen_delta_to_world(image_rect, format, Vec2::new(10.0, 0.0));
        let (before, after) = viewer
            .accumulate_jack_drag(node, second)
            .expect("second drag frame");
        assert!(
            (before[1] - 5.0).abs() < 1e-6,
            "second frame must start at the accumulated value, got {before:?}"
        );
        assert!((after[0] - 5.0).abs() < 1e-6, "world x was {}", after[0]);
        assert!((after[1] - 5.0).abs() < 1e-6, "world y was {}", after[1]);
    }

    #[test]
    fn jack_axis_constraints_zero_the_other_axis() {
        let mut viewer = ViewerPanel::new();
        let node = NodeId(7);

        viewer.jack_drag = Some(JackDragState {
            node_id: node,
            current_translate: [1.0, 2.0],
            axis_constraint: Some(AxisConstraint::X),
        });
        let (_, after) = viewer
            .accumulate_jack_drag(node, Vec2::new(4.0, 9.0))
            .expect("x drag");
        assert_eq!(after, [5.0, 2.0]);

        viewer.jack_drag = Some(JackDragState {
            node_id: node,
            current_translate: [1.0, 2.0],
            axis_constraint: Some(AxisConstraint::Y),
        });
        let (_, after) = viewer
            .accumulate_jack_drag(node, Vec2::new(4.0, 9.0))
            .expect("y drag");
        assert_eq!(after, [1.0, 11.0]);
    }

    #[test]
    fn jack_hit_selects_free_2d_and_each_axis() {
        let anchor = Pos2::new(100.0, 100.0);
        assert_eq!(jack_hit(anchor, anchor), Some(None));
        assert_eq!(
            jack_hit(anchor, anchor + Vec2::new(30.0, 0.0)),
            Some(Some(AxisConstraint::X))
        );
        assert_eq!(
            jack_hit(anchor, anchor - Vec2::new(0.0, 30.0)),
            Some(Some(AxisConstraint::Y))
        );
        assert_eq!(jack_hit(anchor, anchor + Vec2::new(80.0, 80.0)), None);
    }
}
