//! The live Nuke-style viewer widget.
//!
//! The panel owns only display state (viewer target, exposure, gamma, pan,
//! zoom, the last uploaded texture). Frames arrive from the kernel thread as
//! [`FrameNotification`]s; the UI never evaluates the DAG itself.

use std::sync::Arc;

use egui::{
    Align, Align2, Color32, ColorImage, FontId, Key, Layout, Modifiers, PointerButton, Pos2, Rect,
    Sense, Shape, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2,
};
use tanova_kernel::{
    FrameNotification, KernelMessage, KnobValue, NodeId, Packet, ProxyScale, UndoableCommand,
};
use tanova_render::{BBox, DisplayTransform, Mat3x3, Raster2D, ViewerChannel};

use crate::canvas::GraphMirror;
use crate::properties::{effective_knob_value, set_curve_values};

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
/// Gold tick marks for keyframes on the timeline rail.
const KEYFRAME_TICK: Color32 = Color32::from_rgb(255, 200, 50);
/// Recessed timeline scrubber track fill.
const TRACK_BG: Color32 = Color32::from_rgb(22, 24, 29);
/// Timeline scrubber track border.
const TRACK_BORDER: Color32 = Color32::from_rgb(55, 60, 70);
/// Muted ruler ticks and frame numbers under the timeline track.
const RULER_TICK: Color32 = Color32::from_rgb(75, 82, 95);
/// High-contrast playhead line and cap.
const PLAYHEAD_COLOR: Color32 = Color32::from_rgb(245, 135, 35);

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
    /// Viewer display color transform (default `Linear`).
    pub display_transform: DisplayTransform,
    /// Viewer channel solo (`Rgba`, `R`, `G`, `B`, `A`; default `Rgba`).
    pub channel: ViewerChannel,
    /// Requested proxy mode, independent of the image still on screen.
    pub proxy_scale: f32,
    /// Scale of the most recently delivered frame.
    pub frame_proxy_scale: f32,
    /// Playhead used to evaluate overlay knobs and auto-key animated edits.
    pub current_frame: f64,
    last_packet: Option<Arc<Packet>>,
    texture: Option<TextureHandle>,
    dirty: bool,
    uploaded_view: Option<(u32, u32, DisplayTransform, ViewerChannel)>,
    /// Last whole frame handed to the kernel as `SetTime`.
    ///
    /// Deduplicates scrub/playback messages so a 60 Hz UI does not queue
    /// duplicate renders for a frame the kernel already has.
    last_sent_frame: Option<i64>,
    /// In-flight transform jack drag, if any.
    jack_drag: Option<JackDragState>,
    /// In-flight corner-pin handle drag: `(node_id, corner_index)`.
    dragging_corner: Option<(NodeId, usize)>,
    corner_drag_value: Option<[f32; 2]>,
    #[cfg(test)]
    image_rect_for_test: Option<Rect>,
}

impl Default for ViewerPanel {
    fn default() -> Self {
        Self {
            viewer_node: None,
            gain: 1.0,
            gamma: 1.0,
            pan: Vec2::ZERO,
            zoom: 1.0,
            display_transform: DisplayTransform::Linear,
            channel: ViewerChannel::Rgba,
            proxy_scale: 1.0,
            frame_proxy_scale: 1.0,
            current_frame: 1.0,
            last_packet: None,
            texture: None,
            dirty: true,
            uploaded_view: None,
            last_sent_frame: None,
            jack_drag: None,
            dragging_corner: None,
            corner_drag_value: None,
            #[cfg(test)]
            image_rect_for_test: None,
        }
    }
}

/// Human-readable names for the viewer display transforms.
///
/// Used for the combo box's closed-button text so the active transform is
/// always visible. Kept in the UI crate because `DisplayTransform` lives in
/// `tanova-render` and its identifiers are not artist-facing.
trait DisplayTransformLabel {
    fn label(self) -> &'static str;
}

impl DisplayTransformLabel for DisplayTransform {
    fn label(self) -> &'static str {
        match self {
            DisplayTransform::Linear => "Linear",
            DisplayTransform::Srgb => "sRGB",
            DisplayTransform::Rec709 => "Rec.709",
            DisplayTransform::AcesTonemap => "ACES",
        }
    }
}

/// Queues `SetTime(frame)` unless the kernel already has that whole frame.
///
/// Playback and scrubbing can call this many times per second with the same
/// rounded frame; re-sending would queue redundant renders and make the
/// timeline feel laggy.
fn push_set_time(last_sent: &mut Option<i64>, frame: f64, messages: &mut Vec<KernelMessage>) {
    let rounded = frame.round() as i64;
    if *last_sent == Some(rounded) {
        return;
    }
    *last_sent = Some(rounded);
    messages.push(KernelMessage::SetTime(frame));
}

impl ViewerPanel {
    /// Creates an empty viewer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `frame` already reached the kernel from outside the
    /// timeline.
    ///
    /// Playback advances the frame through `TanovaApp::dispatch`, so the
    /// timeline's own dedupe state must follow along; otherwise scrubbing back
    /// to that frame later would be suppressed and the viewer would stall.
    pub fn note_frame_sent(&mut self, frame: f64) {
        self.last_sent_frame = Some(frame.round() as i64);
    }

    /// Current viewer target.
    pub fn viewer_node(&self) -> Option<NodeId> {
        self.viewer_node
    }

    /// `true` while a CornerPin2D corner handle is being dragged on the Viewer.
    pub fn is_dragging_overlay(&self) -> bool {
        self.dragging_corner.is_some()
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
        let scale = ProxyScale::sanitized(frame.proxy_scale);
        let same_image = self
            .last_packet
            .as_ref()
            .is_some_and(|packet| Arc::ptr_eq(packet, &frame.packet))
            && self.frame_proxy_scale == scale
            && self.texture.is_some()
            && self.uploaded_view == Some(self.view_settings());
        self.last_packet = Some(Arc::clone(&frame.packet));
        self.frame_proxy_scale = scale;
        self.dirty |= !same_image;
    }

    /// Drops the last image and its GPU texture.
    ///
    /// The transform jack is reset too: it must never hover over an empty
    /// viewport, because there is no image space to map it into.
    pub fn clear_frame(&mut self) {
        let had_content = self.last_packet.is_some() || self.texture.is_some();
        self.last_packet = None;
        self.frame_proxy_scale = 1.0;
        self.texture = None;
        self.uploaded_view = None;
        self.reset_jack_drag();
        self.reset_corner_drag();
        if had_content {
            self.dirty = true;
        }
    }

    /// `true` when a displayable image is loaded.
    pub fn has_frame(&self) -> bool {
        self.last_packet
            .as_ref()
            .is_some_and(|packet| packet.get::<Raster2D>().is_some())
    }

    /// The last delivered raster, if any.
    pub fn raster(&self) -> Option<&Raster2D> {
        self.last_packet
            .as_ref()
            .and_then(|packet| packet.get::<Raster2D>())
    }

    /// The displayed frame's dimensions in full-resolution logical pixels.
    pub fn logical_frame_size(&self) -> Option<(u32, u32)> {
        self.raster()
            .map(|raster| raster.format.logical_size(self.frame_proxy_scale))
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
        self.show_toolbar(ui, &mut messages);

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

        // Pointer-scoped channel soloing: plain `R`/`G`/`B`/`A` toggle a
        // channel solo only while the pointer hovers the viewer viewport and
        // no widget owns the keyboard. The pointer cannot be over the viewer
        // and the canvas at the same time, so this never conflicts with the
        // canvas `R` (Read), `G` (Grade) and `B` (Blur) spawning hotkeys.
        let pointer_pos = ui.input(|input| input.pointer.hover_pos());
        if let Some(pointer) = pointer_pos {
            if rect.contains(pointer) && !ui.ctx().egui_wants_keyboard_input() {
                let mut toggle_channel = |key: Key, channel: ViewerChannel| {
                    if ui.input_mut(|input| input.consume_key(Modifiers::NONE, key)) {
                        self.channel = if self.channel == channel {
                            ViewerChannel::Rgba
                        } else {
                            channel
                        };
                        self.dirty = true;
                    }
                };
                toggle_channel(Key::R, ViewerChannel::Red);
                toggle_channel(Key::G, ViewerChannel::Green);
                toggle_channel(Key::B, ViewerChannel::Blue);
                toggle_channel(Key::A, ViewerChannel::Alpha);
            }
        }

        let frame = self.last_packet.clone();
        let Some(raster) = frame.as_ref().and_then(|packet| packet.get::<Raster2D>()) else {
            self.reset_jack_drag();
            self.reset_corner_drag();
            self.handle_navigation(ui, &response);
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

        if self.dirty || self.texture.is_none() || self.uploaded_view != Some(self.view_settings())
        {
            self.upload(ui.ctx(), raster);
        }

        let format = self.logical_format(raster);
        // Capture primary presses before navigation, using the same fitted
        // image rect that both the HUD and the overlay will draw against.
        let was_dragging_corner = self.is_dragging_overlay();
        let corner_messages = self.handle_corner_pin_input(
            ui,
            &response,
            self.image_rect(rect, raster),
            format,
            graph,
            selected,
        );
        if !was_dragging_corner && !self.is_dragging_overlay() && corner_messages.is_empty() {
            self.handle_navigation(ui, &response);
        }
        messages.extend(corner_messages);
        let image_rect = self.image_rect(rect, raster);
        #[cfg(test)]
        {
            self.image_rect_for_test = Some(image_rect);
        }
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
            format!("{}x{}", format.width(), format.height()),
            FontId::proportional(10.0),
            FORMAT_LINE,
        );

        // Active data window (may differ from the format, e.g. after a crop).
        // A selected Transform draws its own forward-mapped box instead, so
        // the two outlines never overlap.
        let transform_selected = selected
            .and_then(|id| graph.node(id))
            .is_some_and(|node| node.kind == "Transform");
        let pixel_format = raster.format.to_bbox();
        if !transform_selected && raster.bbox != pixel_format {
            let bbox_rect = self.world_to_screen(image_rect, pixel_format, raster.bbox);
            draw_dashed_rect(&painter, bbox_rect, Stroke::new(1.0, BBOX_LINE));
        }

        self.draw_hud(ui, &painter, rect, image_rect, format, raster);
        messages.extend(
            self.draw_transform_overlay(
                ui, &painter, &response, image_rect, format, graph, selected,
            ),
        );
        self.draw_corner_pin_overlay(ui, &painter, image_rect, format, graph, selected);
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
        keyframes: &[i64],
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };

        // Keep the height reservation identical: the viewer panel relies on it
        // to avoid the egui 0.37 panel-growth feedback loop.
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), TIMELINE_HEIGHT),
            Layout::left_to_right(Align::Center),
            |ui| {
                if ui.button(if *playing { "Pause" } else { "Play" }).clicked() {
                    *playing = !*playing;
                }

                // Jump to the nearest keyframe on either side of the playhead.
                let frame = current.round() as i64;
                if ui.button("[< Key]").clicked() {
                    if let Some(previous) = keyframes.iter().rev().find(|key| **key < frame) {
                        *current = *previous as f64;
                        push_set_time(&mut self.last_sent_frame, *current, &mut messages);
                    }
                }
                if ui.button("[Key >]").clicked() {
                    if let Some(next) = keyframes.iter().find(|key| **key > frame) {
                        *current = *next as f64;
                        push_set_time(&mut self.last_sent_frame, *current, &mut messages);
                    }
                }

                // Compact exact-frame field. Typing a frame is often faster
                // than hitting a 1-pixel-wide tick.
                let mut frame_value = *current;
                let frame_drag = ui.add(
                    egui::DragValue::new(&mut frame_value)
                        .speed(0.25)
                        .range(start as f64..=end as f64)
                        .max_decimals(0),
                );
                if frame_drag.changed() {
                    *current = frame_value.round().clamp(start as f64, end as f64);
                    push_set_time(&mut self.last_sent_frame, *current, &mut messages);
                }

                // Full-width scrubber track. The `- 70.0` leaves room for the
                // range readout on the right; the `.max(120.0)` keeps a usable
                // rail on very narrow panels.
                let track_width = (ui.available_width() - 70.0).max(120.0);
                let (track_rect, track_resp) = ui.allocate_exact_size(
                    egui::vec2(track_width, 22.0),
                    egui::Sense::click_and_drag(),
                );
                let painter = ui.painter();

                let rail_left = track_rect.left() + 8.0;
                let rail_right = track_rect.right() - 8.0;
                let rail_w = (rail_right - rail_left).max(1.0);
                let span = (end - start) as f64;
                let frame_to_x = |f: f64| -> f32 {
                    if span <= 0.0 {
                        rail_left
                    } else {
                        let t = ((f - start as f64) / span).clamp(0.0, 1.0) as f32;
                        rail_left + t * rail_w
                    }
                };

                // 1. Recessed track.
                painter.rect(
                    track_rect,
                    3.0,
                    TRACK_BG,
                    Stroke::new(1.0, TRACK_BORDER),
                    StrokeKind::Inside,
                );

                // 2. Ruler ticks every 5 frames, labels every 20. Minor ticks
                // hug the bottom half so the gold keys and playhead read first.
                let first_tick = start.div_euclid(5).saturating_add(1) * 5;
                let mut tick = first_tick;
                while tick <= end {
                    let x = frame_to_x(tick as f64);
                    let major = tick % 20 == 0;
                    let height = if major { 6.0 } else { 3.0 };
                    painter.line_segment(
                        [
                            Pos2::new(x, track_rect.bottom() - 1.0 - height),
                            Pos2::new(x, track_rect.bottom() - 1.0),
                        ],
                        Stroke::new(1.0, RULER_TICK),
                    );
                    if major {
                        // Labels sit inside the 22px track, just above the
                        // tick marks, so the timeline reservation never has to
                        // grow to fit them.
                        painter.text(
                            Pos2::new(x, track_rect.bottom() - 7.0),
                            Align2::CENTER_BOTTOM,
                            tick.to_string(),
                            FontId::proportional(8.0),
                            RULER_TICK,
                        );
                    }
                    tick += 5;
                }
                // Always label the range start, so frame 1 is never ambiguous.
                painter.text(
                    Pos2::new(frame_to_x(start as f64) + 2.0, track_rect.bottom() - 7.0),
                    Align2::LEFT_BOTTOM,
                    start.to_string(),
                    FontId::proportional(8.0),
                    RULER_TICK,
                );

                // 3. Exact keyframe ticks. Both these and the playhead go
                // through `frame_to_x`, so a key on the current frame lines up
                // perfectly under the playhead.
                for key in keyframes {
                    if *key < start || *key > end {
                        continue;
                    }
                    let x = frame_to_x(*key as f64);
                    painter.line_segment(
                        [
                            Pos2::new(x, track_rect.top() + 2.0),
                            Pos2::new(x, track_rect.bottom() - 2.0),
                        ],
                        Stroke::new(2.5, KEYFRAME_TICK),
                    );
                }

                // 4. Playhead line plus a small top cap.
                let playhead_x = frame_to_x(current.round());
                painter.line_segment(
                    [
                        Pos2::new(playhead_x, track_rect.top()),
                        Pos2::new(playhead_x, track_rect.bottom()),
                    ],
                    Stroke::new(2.0, PLAYHEAD_COLOR),
                );
                painter.add(Shape::convex_polygon(
                    vec![
                        Pos2::new(playhead_x - 5.0, track_rect.top()),
                        Pos2::new(playhead_x + 5.0, track_rect.top()),
                        Pos2::new(playhead_x, track_rect.top() + 6.0),
                    ],
                    PLAYHEAD_COLOR,
                    Stroke::NONE,
                ));

                // Scrubbing: click or drag anywhere on the track maps the
                // pointer back through the same rail geometry.
                if (track_resp.clicked() || track_resp.dragged())
                    && let Some(mouse_pos) = track_resp.interact_pointer_pos()
                {
                    let t = ((mouse_pos.x - rail_left) / rail_w).clamp(0.0, 1.0) as f64;
                    let frame = (start as f64 + t * span).round();
                    *current = frame;
                    push_set_time(&mut self.last_sent_frame, frame, &mut messages);
                }

                // Scrubbing and playback must redraw even when the pointer is
                // still, so the playhead and properties readouts stay live.
                if *playing || track_resp.dragged() {
                    ui.ctx().request_repaint();
                }

                ui.label(format!("{start}..{end}"));
            },
        );

        messages
    }

    fn show_toolbar(&mut self, ui: &mut Ui, messages: &mut Vec<KernelMessage>) {
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
            ui.separator();

            // Channel solo buttons: `[RGBA] [R] [G] [B] [A]`, highlighting
            // the active channel. Clicking sets the channel; pressing the
            // same pointer-scoped hotkey again returns to `[RGBA]`.
            for (label, channel) in [
                ("[RGBA]", ViewerChannel::Rgba),
                ("[R]", ViewerChannel::Red),
                ("[G]", ViewerChannel::Green),
                ("[B]", ViewerChannel::Blue),
                ("[A]", ViewerChannel::Alpha),
            ] {
                if ui
                    .selectable_label(self.channel == channel, label)
                    .clicked()
                {
                    self.channel = channel;
                    self.dirty = true;
                }
            }
            ui.separator();

            // Display transform combo box. `selected_text` keeps the active
            // transform visible in the closed button instead of an empty box.
            let previous_transform = self.display_transform;
            egui::ComboBox::from_label("Display transform")
                .selected_text(self.display_transform.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.display_transform,
                        DisplayTransform::Linear,
                        "Linear",
                    );
                    ui.selectable_value(
                        &mut self.display_transform,
                        DisplayTransform::Srgb,
                        "sRGB",
                    );
                    ui.selectable_value(
                        &mut self.display_transform,
                        DisplayTransform::Rec709,
                        "Rec.709",
                    );
                    ui.selectable_value(
                        &mut self.display_transform,
                        DisplayTransform::AcesTonemap,
                        "ACES",
                    );
                });
            if self.display_transform != previous_transform {
                self.dirty = true;
            }
            ui.separator();
            ui.label("Proxy:");
            for (scale, label) in [(1.0, "1:1"), (0.5, "1:2"), (0.25, "1:4")] {
                if ui
                    .selectable_label(self.proxy_scale == scale, label)
                    .clicked()
                    && self.proxy_scale != scale
                {
                    self.proxy_scale = scale;
                    messages.push(KernelMessage::SetProxyScale(scale));
                }
            }
            // Don't drop the warning while proxy pixels are still displayed
            // after selecting 1:1.
            let badge_scale = if self.proxy_scale < 0.999 {
                self.proxy_scale
            } else {
                self.frame_proxy_scale
            };
            let badge = if badge_scale <= 0.25 {
                " PROXY 1:4 "
            } else {
                " PROXY 1:2 "
            };
            ui.add_visible(
                badge_scale < 0.999,
                egui::Label::new(
                    egui::RichText::new(badge)
                        .monospace()
                        .strong()
                        .color(Color32::from_rgb(30, 24, 8))
                        .background_color(VIEWER),
                ),
            );
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

    fn logical_format(&self, raster: &Raster2D) -> BBox {
        let (width, height) = raster.format.logical_size(self.frame_proxy_scale);
        BBox::new(0, 0, width as i32, height as i32)
    }

    /// Fits the logical frame into the viewer rect, then applies zoom and pan.
    fn image_rect(&self, rect: Rect, raster: &Raster2D) -> Rect {
        let format = self.logical_format(raster);
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

    /// Inverse of `world_to_screen_point`, shared by the HUD and corner drags.
    fn screen_to_image(&self, image_rect: Rect, format: BBox, pointer: Pos2) -> [f32; 2] {
        let u = (pointer.x - image_rect.left()) / image_rect.width().max(1e-6);
        let v = (pointer.y - image_rect.top()) / image_rect.height().max(1e-6);
        [
            format.min_x as f32 + u * format.width().max(1) as f32,
            format.min_y as f32 + (1.0 - v) * format.height().max(1) as f32,
        ]
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

    /// Captures handle presses before navigation, without waiting for egui's
    /// drag threshold (which can move the pointer outside the hit zone).
    #[allow(clippy::too_many_arguments)]
    fn handle_corner_pin_input(
        &mut self,
        ui: &Ui,
        response: &egui::Response,
        image_rect: Rect,
        format: BBox,
        graph: &GraphMirror,
        selected: Option<NodeId>,
    ) -> Vec<KernelMessage> {
        let Some(node_id) = selected.filter(|id| {
            graph
                .node(*id)
                .is_some_and(|node| node.kind == "CornerPin2D")
        }) else {
            self.reset_corner_drag();
            return Vec::new();
        };

        let node = graph.node(node_id).unwrap();
        if self.dragging_corner.is_some_and(|(id, _)| id != node_id) {
            self.reset_corner_drag();
        }
        let corners = self.corner_pin_corners(node, format);
        let corner_names = ["to1", "to2", "to3", "to4"];

        let pressed = ui.input(|input| input.pointer.primary_pressed());
        if pressed && response.contains_pointer() {
            if let Some(pointer) = ui.input(|input| input.pointer.press_origin()) {
                for (i, &corner) in corners.iter().enumerate() {
                    let screen =
                        self.world_to_screen_point(image_rect, format, corner[0], corner[1]);
                    if pointer.distance(screen) <= 12.0 {
                        self.dragging_corner = Some((node_id, i));
                        self.corner_drag_value = Some(corner);
                        ui.ctx().request_repaint();
                        break;
                    }
                }
            }
        }

        let mut messages = Vec::new();

        if let Some((_, corner_idx)) = self.dragging_corner {
            let pointer = ui.input(|input| input.pointer.interact_pos());
            if !pressed && let Some(pointer) = pointer {
                let [x, y] = self.screen_to_image(image_rect, format, pointer);
                let fw = format.width() as f32;
                let fh = format.height() as f32;
                let xy = [x.clamp(-fw, 2.0 * fw), y.clamp(-fh, 2.0 * fh)];
                let current = self.corner_drag_value.unwrap_or(corners[corner_idx]);
                if xy.iter().zip(current).any(|(a, b)| (a - b).abs() > 1e-4) {
                    let name = corner_names[corner_idx];
                    let value = KnobValue::Color([xy[0], xy[1], 0.0, 0.0]);
                    let set = UndoableCommand::SetKnob {
                        node: node_id,
                        knob: name.to_string(),
                        value: value.clone(),
                        previous: node
                            .knobs
                            .get(name)
                            .cloned()
                            .unwrap_or(KnobValue::Color([current[0], current[1], 0.0, 0.0])),
                    };
                    let command = match node.curves.get(name) {
                        Some(previous) if previous.iter().any(|curve| !curve.is_empty()) => {
                            UndoableCommand::Batch(vec![
                                set,
                                UndoableCommand::SetKnobCurve {
                                    node: node_id,
                                    name: name.to_string(),
                                    curves: set_curve_values(
                                        previous.clone(),
                                        &value,
                                        self.current_frame,
                                    ),
                                    previous: previous.clone(),
                                },
                            ])
                        }
                        _ => set,
                    };
                    messages.push(KernelMessage::Execute(command));
                    self.corner_drag_value = Some(xy);
                    ui.ctx().request_repaint();
                }
            }
        }

        // Also release capture when the window loses the pointer/button.
        if !ui.input(|input| input.pointer.primary_down()) {
            self.reset_corner_drag();
        }
        messages
    }

    fn corner_pin_corners(&self, node: &crate::canvas::CanvasNode, format: BBox) -> [[f32; 2]; 4] {
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
            match effective_knob_value(node, &name, &base, self.current_frame) {
                KnobValue::Color(c) => [c[0], c[1]],
                _ => defaults[i],
            }
        })
    }

    /// Paint the quad after the image, using the same effective values as hit testing.
    fn draw_corner_pin_overlay(
        &self,
        ui: &Ui,
        painter: &egui::Painter,
        image_rect: Rect,
        format: BBox,
        graph: &GraphMirror,
        selected: Option<NodeId>,
    ) {
        let Some(node) = selected
            .and_then(|id| graph.node(id))
            .filter(|node| node.kind == "CornerPin2D")
        else {
            return;
        };
        let node_id = node.id;
        let corners = self.corner_pin_corners(node, format);
        // Draw the wireframe quad: to1 -> to2 -> to3 -> to4 -> to1.
        let screen_corners: [Pos2; 4] =
            corners.map(|c| self.world_to_screen_point(image_rect, format, c[0], c[1]));
        for i in 0..4 {
            let next = (i + 1) % 4;
            painter.line_segment(
                [screen_corners[i], screen_corners[next]],
                Stroke::new(1.5, Color32::from_rgba_unmultiplied(255, 175, 55, 200)),
            );
        }

        // Draw handles.
        for (i, &screen) in screen_corners.iter().enumerate() {
            let is_dragging = self
                .dragging_corner
                .is_some_and(|(n, idx)| n == node_id && idx == i);
            let hovered = ui.input(|input| {
                input
                    .pointer
                    .hover_pos()
                    .is_some_and(|p| p.distance(screen) <= 12.0)
            });
            let color = if hovered || is_dragging {
                JACK_HOVER
            } else {
                JACK
            };
            let label = (i + 1).to_string();
            painter.circle_filled(screen, 7.0, color);
            painter.circle_stroke(screen, 7.0, Stroke::new(1.0, Color32::WHITE));
            painter.text(
                screen + Vec2::new(JACK_RADIUS + 2.0, 0.0),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(10.0),
                color,
            );
        }
    }

    fn reset_corner_drag(&mut self) {
        self.dragging_corner = None;
        self.corner_drag_value = None;
    }

    fn view_settings(&self) -> (u32, u32, DisplayTransform, ViewerChannel) {
        (
            self.gain.to_bits(),
            self.gamma.to_bits(),
            self.display_transform,
            self.channel,
        )
    }

    fn upload(&mut self, ctx: &egui::Context, raster: &Raster2D) {
        let width = raster.format.width.max(1) as usize;
        let height = raster.format.height.max(1) as usize;
        let bytes = raster.to_top_left_rgba8_with_view(
            raster.format.to_bbox(),
            self.gain,
            self.gamma,
            self.display_transform,
            self.channel,
        );
        let image = ColorImage::from_rgba_unmultiplied([width, height], &bytes);

        match &mut self.texture {
            Some(texture) => texture.set(image, TextureOptions::NEAREST),
            None => {
                self.texture =
                    Some(ctx.load_texture("tanova_viewer", image, TextureOptions::NEAREST));
            }
        }
        self.uploaded_view = Some(self.view_settings());
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
                let [x, y] = self.screen_to_image(image_rect, format, pointer);
                let x = (x.floor() as i32).clamp(0, format.width() - 1);
                let y = (y.floor() as i32).clamp(0, format.height() - 1);
                let px = ((x as f32 * self.frame_proxy_scale) as i32)
                    .min(raster.format.width as i32 - 1);
                let py = ((y as f32 * self.frame_proxy_scale) as i32)
                    .min(raster.format.height as i32 - 1);
                let [r, g, b, a] = raster.sample_rgba(px, py);
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
    fn set_time_is_only_sent_when_the_whole_frame_changes() {
        let mut last = None;
        let mut messages = Vec::new();

        push_set_time(&mut last, 5.0, &mut messages);
        push_set_time(&mut last, 5.0, &mut messages);
        // A fractional scrub that rounds to the same frame is a duplicate.
        push_set_time(&mut last, 5.4, &mut messages);
        push_set_time(&mut last, 6.0, &mut messages);

        assert_eq!(
            messages.len(),
            2,
            "only frames 5 and 6 should reach the kernel"
        );
        assert!(matches!(messages[0], KernelMessage::SetTime(t) if (t - 5.0).abs() < 1e-9));
        assert!(matches!(messages[1], KernelMessage::SetTime(t) if (t - 6.0).abs() < 1e-9));

        // Playback advances the frame outside `show_timeline`; the panel must
        // learn about it, or scrubbing back to that frame would be suppressed.
        let mut viewer = ViewerPanel::new();
        viewer.note_frame_sent(9.0);
        assert_eq!(viewer.last_sent_frame, Some(9));
        let mut messages = Vec::new();
        push_set_time(&mut viewer.last_sent_frame, 9.0, &mut messages);
        assert!(messages.is_empty(), "frame 9 already reached the kernel");
        push_set_time(&mut viewer.last_sent_frame, 10.0, &mut messages);
        assert_eq!(messages.len(), 1, "a new frame must still be sent");
    }

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

    #[test]
    fn viewer_r_g_b_a_hotkeys_solo_and_toggle_back_when_hovered() {
        let ctx = egui::Context::default();
        let mut viewer = ViewerPanel::new();
        let graph = GraphMirror::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));
        // Anywhere inside the viewer viewport rect allocated by `show`.
        let pointer = Pos2::new(100.0, 100.0);

        let key_event = |key: Key| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };

        let run_frame = |viewer: &mut ViewerPanel, key: Key| {
            ctx.run_pass(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events: vec![egui::Event::PointerMoved(pointer), key_event(key)],
                    ..Default::default()
                },
                |ctx| {
                    ctx.root_ui(|ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            let _ = viewer.show(ui, &graph, None);
                        });
                    });
                },
            )
            .drop_without_applying_deltas();
        };

        // Frame 1: pointer over the viewer, press `R` → solo red.
        run_frame(&mut viewer, Key::R);
        assert_eq!(viewer.channel, ViewerChannel::Red);

        // Frame 2: press `R` again → back to RGBA.
        run_frame(&mut viewer, Key::R);
        assert_eq!(viewer.channel, ViewerChannel::Rgba);

        // `G` and `B` solo while hovered; `A` solos alpha.
        run_frame(&mut viewer, Key::G);
        assert_eq!(viewer.channel, ViewerChannel::Green);
        run_frame(&mut viewer, Key::B);
        assert_eq!(viewer.channel, ViewerChannel::Blue);
        run_frame(&mut viewer, Key::A);
        assert_eq!(viewer.channel, ViewerChannel::Alpha);
    }

    fn corner_pin_test_scene() -> (ViewerPanel, GraphMirror) {
        tanova_nodes::ensure_linked();
        let mut graph = GraphMirror::default();
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "CornerPin2D".to_string(),
            x: 0.0,
            y: 0.0,
        });

        let mut viewer = ViewerPanel::new();
        viewer.viewer_node = Some(NodeId(1));
        let mut packet = Packet::new();
        packet.insert(Raster2D::for_format(Format::new("test", 256, 256, 1.0)));
        viewer.last_packet = Some(Arc::new(packet));
        (viewer, graph)
    }

    fn run_corner_viewer(
        ctx: &egui::Context,
        viewer: &mut ViewerPanel,
        graph: &GraphMirror,
        events: Vec<egui::Event>,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        messages = viewer.show(ui, graph, Some(NodeId(1)));
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        messages
    }

    fn pointer_button(pos: Pos2, button: PointerButton, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn viewer_reuses_texture_for_the_same_packet_until_view_settings_change() {
        let (mut viewer, graph) = corner_pin_test_scene();
        let ctx = egui::Context::default();
        run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
        assert!(viewer.texture.is_some() && !viewer.dirty);
        let mut repeated = FrameNotification::frame(
            NodeId(1),
            2,
            Arc::clone(viewer.last_packet.as_ref().unwrap()),
        );
        viewer.set_frame(&repeated);
        assert!(!viewer.dirty);
        run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
        assert!(!viewer.dirty);

        viewer.gain = 2.0;
        viewer.set_frame(&repeated);
        assert!(viewer.dirty);
        run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
        assert_eq!(viewer.uploaded_view, Some(viewer.view_settings()));
        repeated.proxy_scale = 0.5;
        viewer.set_frame(&repeated);
        assert!(viewer.dirty);

        viewer.set_frame(&FrameNotification::cleared(Some(NodeId(1)), 3));
        assert!(viewer.last_packet.is_none() && viewer.texture.is_none());
    }

    #[test]
    fn viewer_corner_pin_drag_in_half_proxy_writes_full_resolution_logical_coords() {
        let (mut viewer, graph) = corner_pin_test_scene();
        viewer.pan = Vec2::new(9.0, -6.0);
        viewer.zoom = 0.87;
        let ctx = egui::Context::default();
        run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
        let full_rect = viewer.image_rect_for_test.unwrap();

        // A requested mode change cannot reinterpret the old frame's pixels.
        viewer.proxy_scale = 0.5;
        run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
        assert_eq!(viewer.logical_frame_size(), Some((256, 256)));
        assert_eq!(viewer.image_rect_for_test, Some(full_rect));

        let mut packet = Packet::new();
        packet.insert(Raster2D::for_format(Format::new("half", 128, 128, 1.0)));
        let mut frame = FrameNotification::frame(NodeId(1), 2, Arc::new(packet));
        frame.proxy_scale = 0.5;
        viewer.set_frame(&frame);
        run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
        assert_eq!(viewer.frame_proxy_scale, 0.5);
        assert_eq!(viewer.image_rect_for_test, Some(full_rect));
        let start = full_rect.left_bottom();
        run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![egui::Event::PointerMoved(start)],
        );
        run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![pointer_button(start, PointerButton::Primary, true)],
        );
        let logical = BBox::new(0, 0, 256, 256);
        let end = viewer.world_to_screen_point(full_rect, logical, 192.0, 160.0);
        let messages = run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![egui::Event::PointerMoved(end)],
        );
        assert!(
            matches!(
                messages.as_slice(),
                [KernelMessage::Execute(UndoableCommand::SetKnob {
                    knob, value: KnobValue::Color([x, y, _, _]), ..
                })] if knob == "to1" && (*x - 192.0).abs() < 1e-3 && (*y - 160.0).abs() < 1e-3
            ),
            "{messages:?}"
        );
    }

    #[test]
    fn viewer_fit_does_not_jump_for_odd_dimensions_in_quarter_proxy() {
        let mut viewer = ViewerPanel::new();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let format = Format::new("odd", 641, 479, 1.0);
        let full = Raster2D::for_format(format.clone());
        let quarter = Raster2D::for_format(format.scaled(0.25));
        let full_rect = viewer.image_rect(screen, &full);
        viewer.frame_proxy_scale = 0.25;
        assert_eq!(viewer.image_rect(screen, &quarter), full_rect);
    }

    #[test]
    fn viewer_corner_pin_handle_drag_emits_set_knob_without_panning() {
        for (zoom, pan) in [(0.6, Vec2::new(15.0, -12.0)), (0.95, Vec2::new(-8.0, 9.0))] {
            let (mut viewer, graph) = corner_pin_test_scene();
            viewer.zoom = zoom;
            viewer.pan = pan;
            let ctx = egui::Context::default();
            run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
            let image_rect = viewer.image_rect_for_test.unwrap();
            let start = image_rect.left_bottom();
            // Warm hover, then press: capture must happen on press, not on
            // the first move after egui's drag threshold.
            run_corner_viewer(
                &ctx,
                &mut viewer,
                &graph,
                vec![egui::Event::PointerMoved(start)],
            );
            let pressed = run_corner_viewer(
                &ctx,
                &mut viewer,
                &graph,
                vec![pointer_button(start, PointerButton::Primary, true)],
            );
            assert!(pressed.is_empty());
            assert_eq!(viewer.dragging_corner, Some((NodeId(1), 0)));
            assert!(viewer.is_dragging_overlay());
            let end = start + Vec2::new(50.0, -30.0);
            let messages = run_corner_viewer(
                &ctx,
                &mut viewer,
                &graph,
                vec![
                    pointer_button(start, PointerButton::Middle, true),
                    egui::Event::PointerMoved(end),
                ],
            );
            let [
                KernelMessage::Execute(UndoableCommand::SetKnob {
                    node, knob, value, ..
                }),
            ] = messages.as_slice()
            else {
                panic!("expected real SetKnob, got {messages:?}")
            };
            assert_eq!(*node, NodeId(1));
            assert_eq!(knob, "to1");
            let KnobValue::Color(c) = value else {
                panic!("expected XY")
            };
            assert!((c[0] - 50.0 * 256.0 / image_rect.width()).abs() < 1e-3);
            assert!((c[1] - 30.0 * 256.0 / image_rect.height()).abs() < 1e-3);
            assert_eq!([c[2], c[3]], [0.0, 0.0]);
            assert_eq!(viewer.pan, pan);
            // No accumulated deltas or snapback even before the mirror echoes.
            assert!(run_corner_viewer(&ctx, &mut viewer, &graph, vec![]).is_empty());
            let screen =
                viewer.world_to_screen_point(image_rect, BBox::new(0, 0, 256, 256), c[0], c[1]);
            assert!(screen.distance(end) < 1e-3);
            run_corner_viewer(
                &ctx,
                &mut viewer,
                &graph,
                vec![
                    pointer_button(end, PointerButton::Primary, false),
                    pointer_button(end, PointerButton::Middle, false),
                ],
            );
            assert!(!viewer.is_dragging_overlay());
            assert_eq!(viewer.pan, pan);
        }
    }

    #[test]
    fn viewer_corner_pin_animated_handle_evaluates_and_auto_keys() {
        use tanova_kernel::{AnimationCurve, InterpMode};
        let (mut viewer, mut graph) = corner_pin_test_scene();
        let mut x = AnimationCurve::new();
        x.set_key(1.0, 10.0, InterpMode::Linear);
        x.set_key(11.0, 30.0, InterpMode::Linear);
        let previous = vec![x, AnimationCurve::from_constant(30.0)];
        graph.apply_command(&UndoableCommand::SetKnobCurve {
            node: NodeId(1),
            name: "to1".into(),
            curves: previous.clone(),
            previous: vec![],
        });
        viewer.current_frame = 6.0;
        let ctx = egui::Context::default();
        run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
        let rect = viewer.image_rect_for_test.unwrap();
        let format = BBox::new(0, 0, 256, 256);
        let start = viewer.world_to_screen_point(rect, format, 20.0, 30.0);
        run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![egui::Event::PointerMoved(start)],
        );
        run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![pointer_button(start, PointerButton::Primary, true)],
        );
        assert!(
            viewer.is_dragging_overlay(),
            "hit testing must use evaluated curves"
        );
        let end = viewer.world_to_screen_point(rect, format, 45.0, 55.0);
        let messages = run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![egui::Event::PointerMoved(end)],
        );
        let [KernelMessage::Execute(UndoableCommand::Batch(commands))] = messages.as_slice() else {
            panic!("animated drag must emit a batch: {messages:?}");
        };
        let [
            UndoableCommand::SetKnob {
                value,
                previous: base,
                ..
            },
            UndoableCommand::SetKnobCurve {
                curves,
                previous: old,
                ..
            },
        ] = commands.as_slice()
        else {
            panic!("expected knob and curve commands");
        };
        assert_eq!(
            base,
            &KnobValue::Color([0.0; 4]),
            "undo restores the static base, not the evaluated value"
        );
        assert_eq!(old, &previous);
        for (i, expected) in [45.0, 55.0].into_iter().enumerate() {
            assert!((curves[i].evaluate(6.0) - expected).abs() < 1e-3);
            assert!(curves[i].has_key_at(6.0));
        }
        assert!(
            matches!(value, KnobValue::Color(c) if (c[0] - 45.0).abs() < 1e-3 && (c[1] - 55.0).abs() < 1e-3)
        );
        assert!(curves[0].has_key_at(1.0) && curves[0].has_key_at(11.0));
        graph.apply_command(&UndoableCommand::Batch(commands.clone()));
        run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![pointer_button(end, PointerButton::Primary, false)],
        );
        let corners = viewer.corner_pin_corners(graph.node(NodeId(1)).unwrap(), format);
        assert!((corners[0][0] - 45.0).abs() < 1e-3);
        assert_eq!(viewer.pan, Vec2::ZERO);
    }
}
