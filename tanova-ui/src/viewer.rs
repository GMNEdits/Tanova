//! The live Nuke-style viewer widget.
//!
//! The panel owns only display state (viewer target, exposure, gamma, pan,
//! zoom, the last uploaded texture). Frames arrive from the kernel thread as
//! [`FrameNotification`]s; the UI never evaluates the DAG itself.

use std::sync::Arc;

use egui::{
    Align, Align2, Color32, ColorImage, FontId, Layout, Pos2, Rect, Sense, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Ui, Vec2,
};
use tanova_kernel::{FrameNotification, KernelMessage, KnobValue, NodeId, Packet, UndoableCommand};
use tanova_render::{BBox, Raster2D};

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
const JACK_RADIUS: f32 = 7.0;
const JACK_HIT_RADIUS: f32 = 12.0;

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
    /// True while the primary button drags the selected Transform jack.
    dragging_handle: bool,
    /// Pointer position when the jack drag began.
    drag_start_pointer: Option<Pos2>,
    /// `translate` captured when the jack drag began.
    drag_base_translate: Option<[f32; 4]>,
    /// Local drag preview so the jack never waits on the kernel round-trip.
    drag_preview_translate: Option<[f32; 4]>,
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
            dragging_handle: false,
            drag_start_pointer: None,
            drag_base_translate: None,
            drag_preview_translate: None,
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
        self.frame = None;
        self.dirty = true;
    }

    /// Accepts a frame from the kernel, ignoring frames for other targets.
    pub fn set_frame(&mut self, frame: &FrameNotification) {
        if self.viewer_node != Some(frame.node) {
            return;
        }
        self.frame = Some(Arc::clone(&frame.packet));
        self.dirty = true;
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
        let bbox_rect = self.world_to_screen(image_rect, format, raster.bbox);
        if raster.bbox != format {
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

    /// Draws the selected Transform node's jack and turns primary drags into
    /// `SetKnob` commands for the `translate` knob.
    ///
    /// The jack position is `center + translate` (the transform's pivot after
    /// the affine map) and is recomputed from `GraphMirror` on every frame.
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
        let Some((node_id, graph_translate, center)) = selected_transform(graph, selected) else {
            self.reset_jack_drag();
            return Vec::new();
        };

        let pointer = ui.input(|input| input.pointer.hover_pos());
        let primary_pressed = ui.input(|input| input.pointer.primary_pressed());
        let primary_down = ui.input(|input| input.pointer.primary_down());
        let primary_released = ui.input(|input| input.pointer.primary_released());

        let mut translate = graph_translate;
        let mut handle_world = (center[0] + translate[0], center[1] + translate[1]);
        let mut handle =
            self.world_to_screen_point(image_rect, format, handle_world.0, handle_world.1);

        if primary_pressed && response.hovered() {
            if let Some(pointer) = pointer {
                if pointer.distance(handle) <= JACK_HIT_RADIUS {
                    self.dragging_handle = true;
                    self.drag_start_pointer = Some(pointer);
                    self.drag_base_translate = Some(graph_translate);
                    self.drag_preview_translate = Some(graph_translate);
                }
            }
        }

        let mut messages = Vec::new();
        if self.dragging_handle && primary_down {
            if let (Some(start), Some(base), Some(pointer)) =
                (self.drag_start_pointer, self.drag_base_translate, pointer)
            {
                let world_delta = self.screen_delta_to_world(image_rect, format, pointer - start);
                let next = [
                    base[0] + world_delta.x,
                    base[1] + world_delta.y,
                    base[2],
                    base[3],
                ];
                if self.drag_preview_translate != Some(next) {
                    messages.push(KernelMessage::Execute(UndoableCommand::SetKnob {
                        node: node_id,
                        knob: "translate".to_string(),
                        value: KnobValue::Color(next),
                        previous: KnobValue::Color(graph_translate),
                    }));
                    self.drag_preview_translate = Some(next);
                }
                translate = next;
                handle_world = (center[0] + translate[0], center[1] + translate[1]);
                handle =
                    self.world_to_screen_point(image_rect, format, handle_world.0, handle_world.1);
            }
        }

        if primary_released || !primary_down {
            self.reset_jack_drag();
        }

        let hovered = pointer.is_some_and(|pointer| pointer.distance(handle) <= JACK_HIT_RADIUS);
        let color = if self.dragging_handle || hovered {
            JACK_HOVER
        } else {
            JACK
        };
        draw_jack(painter, handle, color);

        // The pivot is a lighter dot so the user can see center vs. translate.
        let pivot = self.world_to_screen_point(image_rect, format, center[0], center[1]);
        painter.circle_filled(pivot, 2.5, Color32::from_rgb(150, 220, 255));

        messages
    }

    fn reset_jack_drag(&mut self) {
        self.dragging_handle = false;
        self.drag_start_pointer = None;
        self.drag_base_translate = None;
        self.drag_preview_translate = None;
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

/// Reads the selected node's Transform knobs, if it is a `Transform`.
fn selected_transform(
    graph: &GraphMirror,
    selected: Option<NodeId>,
) -> Option<(NodeId, [f32; 4], [f32; 4])> {
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
    Some((id, translate, center))
}

/// Draws a Nuke-style transform jack: a ring, a crosshair, and a label.
fn draw_jack(painter: &egui::Painter, position: Pos2, color: Color32) {
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
}
