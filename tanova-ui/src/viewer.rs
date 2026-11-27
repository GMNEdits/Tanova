//! The live Nuke-style viewer widget.
//!
//! The panel owns only display state (viewer target, exposure, gamma, pan,
//! zoom, the last uploaded texture). Frames arrive from the kernel thread as
//! [`FrameNotification`]s; the UI never evaluates the DAG itself.

use std::sync::Arc;

use egui::{
    Align, Align2, Color32, ColorImage, FontId, Key, Layout, Modifiers, Pos2, Rect, Sense, Shape,
    Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2,
};
#[allow(unused_imports)]
use tanova_kernel::KnobValue;
use tanova_kernel::{FrameNotification, KernelMessage, NodeErrorInfo, NodeId, Packet, ProxyScale};
use tanova_render::{BBox, DisplayTransform, Raster2D, ViewerChannel};

use crate::canvas::{CanvasNode, GraphMirror};
#[cfg(test)]
use crate::overlay::jack_hit;
pub use crate::overlay::{AxisConstraint, JackDragState, forward_transform_matrix};
use crate::overlay::{OverlayResponse, OverlayState, OverlayViewport, draw_dashed_rect};

#[cfg(test)]
use egui::PointerButton;
#[cfg(test)]
use tanova_kernel::UndoableCommand;

const VIEWER_BG: Color32 = Color32::from_rgb(14, 14, 17);
const CHECKER_DARK: Color32 = Color32::from_rgb(26, 26, 30);
const CHECKER_LIGHT: Color32 = Color32::from_rgb(34, 34, 40);
const FORMAT_LINE: Color32 = Color32::from_rgb(150, 150, 165);
const HUD_BG: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 190);
const VIEWER: Color32 = Color32::from_rgb(255, 190, 40);

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;
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
    /// Non-blocking error banner text for the current render pass.
    pub active_error: Option<String>,
    last_packet: Option<Arc<Packet>>,
    texture: Option<TextureHandle>,
    dirty: bool,
    uploaded_view: Option<(u32, u32, DisplayTransform, ViewerChannel)>,
    /// Last whole frame handed to the kernel as `SetTime`.
    ///
    /// Deduplicates scrub/playback messages so a 60 Hz UI does not queue
    /// duplicate renders for a frame the kernel already has.
    last_sent_frame: Option<i64>,
    /// Shared 2D overlay drag state (Transform, CornerPin2D, Ramp).
    overlay: OverlayState,
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
            active_error: None,
            last_packet: None,
            texture: None,
            dirty: true,
            uploaded_view: None,
            last_sent_frame: None,
            overlay: OverlayState::default(),
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

    /// `true` while any Viewer overlay handle is being dragged.
    pub fn is_dragging_overlay(&self) -> bool {
        self.overlay.is_dragging()
    }

    /// Replaces the non-blocking error banner from one kernel render pass.
    ///
    /// An empty slice clears it; the banner shows the first failing node.
    pub fn set_node_errors(&mut self, errors: &[NodeErrorInfo]) {
        self.active_error = errors
            .first()
            .map(|error| format!("#{} | {}", error.node.0, error.message));
    }

    /// Sets or clears the non-blocking error banner text.
    ///
    /// The app formats this with the failing node's kind and id so the banner
    /// reads like `Error on Read #4: File not found: plate.0001.exr`.
    pub fn set_active_error(&mut self, message: Option<String>) {
        self.active_error = message;
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
        // A new target (or clearing the target) retires the stale error banner.
        self.active_error = None;
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
        // A displayable frame proves the current chain rendered cleanly.
        self.active_error = None;
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
        self.overlay.reset();
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
            self.overlay.reset();
            self.handle_navigation(ui, &response);
            if self.active_error.is_some() {
                // The render failed, so "Waiting for a frame" is misleading:
                // point the artist at the banner instead.
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    "Render stopped - see error banner above".to_string(),
                    FontId::proportional(13.0),
                    Color32::from_rgb(130, 130, 140),
                );
            } else {
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
            }
            // The banner must paint even when there is no raster: a failed
            // render clears the frame, and this is the only place the reason
            // can reach the artist.
            self.draw_error_banner(ui, &painter, rect);
            return messages;
        };

        if self.dirty || self.texture.is_none() || self.uploaded_view != Some(self.view_settings())
        {
            self.upload(ui.ctx(), raster);
        }

        let format = self.logical_format(raster);
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
            draw_dashed_rect(
                &painter,
                bbox_rect,
                Stroke::new(1.0, crate::overlay::BBOX_LINE),
            );
        }

        self.draw_hud(ui, &painter, rect, image_rect, format, raster);

        // Every 2D handle shares one input pass. It must run before the
        // pan/zoom navigation so a live handle owns the pointer.
        let viewport = OverlayViewport::new(
            image_rect,
            format.width() as f32,
            format.height() as f32,
            self.current_frame,
        );
        let OverlayResponse {
            consumed_pointer,
            commands,
        } = self
            .overlay
            .show_and_handle(ui, &painter, &response, viewport, graph, selected);
        if !consumed_pointer {
            self.handle_navigation(ui, &response);
        }
        messages.extend(commands.into_iter().map(KernelMessage::Execute));

        self.draw_error_banner(ui, &painter, rect);
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

    /// Adds one image-space delta to the active jack drag.
    #[allow(dead_code)]
    fn accumulate_jack_drag(
        &mut self,
        node_id: NodeId,
        world_delta: Vec2,
    ) -> Option<([f32; 2], [f32; 2])> {
        self.overlay.accumulate_jack_drag(node_id, world_delta)
    }

    #[allow(dead_code)]
    fn corner_pin_corners(&self, node: &CanvasNode, format: BBox) -> [[f32; 2]; 4] {
        self.overlay
            .corner_pin_corners(node, format, self.current_frame)
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

    /// Paints the non-blocking error banner pinned to the top interior edge of
    /// the Viewer viewport. One truncated line at rest; the full text on hover.
    fn draw_error_banner(&self, ui: &Ui, painter: &egui::Painter, rect: Rect) {
        let Some(message) = &self.active_error else {
            return;
        };
        let banner_height = 20.0;
        let banner_rect = Rect::from_min_size(
            rect.min + Vec2::new(8.0, 8.0),
            Vec2::new(rect.width() - 16.0, banner_height),
        );
        let banner_bg = Color32::from_rgba_unmultiplied(130, 26, 26, 235);
        painter.rect_filled(banner_rect, 2.0, banner_bg);
        painter.rect_stroke(
            banner_rect,
            2.0,
            Stroke::new(1.0, Color32::from_rgb(235, 75, 70)),
            StrokeKind::Inside,
        );

        let hovered = ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .is_some_and(|p| banner_rect.contains(p))
        });
        let font = FontId::proportional(11.0);
        let full = painter.layout_no_wrap(message.clone(), font.clone(), Color32::WHITE);
        let available = banner_rect.width() - 16.0;
        let display: String = if hovered || full.size().x <= available {
            message.clone()
        } else {
            // Truncate to one line, appending an ellipsis. Walk chars so a
            // non-ASCII byte never splits a UTF-8 boundary.
            let mut truncated = String::new();
            for ch in message.chars() {
                let candidate = format!("{truncated}{ch}...");
                let galley = painter.layout_no_wrap(candidate, font.clone(), Color32::WHITE);
                if galley.size().x > available {
                    break;
                }
                truncated.push(ch);
            }
            format!("{truncated}...")
        };
        painter.text(
            banner_rect.min + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            display,
            font,
            Color32::WHITE,
        );
    }
}

/// Draws a checkerboard pattern behind the viewer viewport.
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
        viewer.overlay.jack_drag = Some(JackDragState {
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

        viewer.overlay.jack_drag = Some(JackDragState {
            node_id: node,
            current_translate: [1.0, 2.0],
            axis_constraint: Some(AxisConstraint::X),
        });
        let (_, after) = viewer
            .accumulate_jack_drag(node, Vec2::new(4.0, 9.0))
            .expect("x drag");
        assert_eq!(after, [5.0, 2.0]);

        viewer.overlay.jack_drag = Some(JackDragState {
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
            assert_eq!(viewer.overlay.dragging_corner, Some((NodeId(1), 0)));
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

    fn ramp_test_scene() -> (ViewerPanel, GraphMirror) {
        tanova_nodes::ensure_linked();
        let mut graph = GraphMirror::default();
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Ramp".to_string(),
            x: 0.0,
            y: 0.0,
        });
        let mut viewer = ViewerPanel::new();
        viewer.viewer_node = Some(NodeId(1));
        let mut packet = Packet::new();
        packet.insert(Raster2D::for_format(Format::new("test", 128, 128, 1.0)));
        viewer.last_packet = Some(Arc::new(packet));
        viewer.frame_proxy_scale = 0.5;
        (viewer, graph)
    }

    #[test]
    fn ramp_overlay_drag_in_half_proxy_writes_full_resolution_logical_p0_and_p1() {
        let (mut viewer, graph) = ramp_test_scene();
        viewer.proxy_scale = 0.5;
        let ctx = egui::Context::default();
        run_corner_viewer(&ctx, &mut viewer, &graph, vec![]);
        assert_eq!(viewer.logical_frame_size(), Some((256, 256)));

        let image_rect = viewer.image_rect_for_test.unwrap();
        // P1 defaults to (256, 256) logical -> top-right corner of the image rect.
        let p1_screen = Pos2::new(image_rect.right(), image_rect.top());

        // Hover then press on P1 to start the drag (matching the corner-pin test pattern).
        run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![egui::Event::PointerMoved(p1_screen)],
        );
        run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![pointer_button(p1_screen, PointerButton::Primary, true)],
        );
        assert!(viewer.is_dragging_overlay());
        assert_eq!(viewer.overlay.dragging_ramp, Some((NodeId(1), 1)));

        // Drag P1 to logical (150.0, 100.0).
        let new_screen = Pos2::new(
            image_rect.left() + (150.0 / 256.0) * image_rect.width(),
            image_rect.bottom() - (100.0 / 256.0) * image_rect.height(),
        );
        let messages = run_corner_viewer(
            &ctx,
            &mut viewer,
            &graph,
            vec![egui::Event::PointerMoved(new_screen)],
        );
        assert!(
            matches!(
                messages.as_slice(),
                [KernelMessage::Execute(UndoableCommand::SetKnob {
                    knob, value: KnobValue::Color([x, y, 0.0, 0.0]), ..
                })] if knob == "p1"
                    && (*x - 150.0).abs() < 1e-3
                    && (*y - 100.0).abs() < 1e-3
            ),
            "{messages:?}"
        );
        assert_eq!(
            viewer.pan,
            Vec2::ZERO,
            "panning must not be driven by the overlay drag"
        );
    }
}
