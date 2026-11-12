//! Custom Nuke-style node canvas drawn directly on `egui::Painter`.
//!
//! The widget owns only visual state (pan, zoom, layout, selection, wire drag).
//! Every graph mutation is returned as a [`KernelMessage`] for the app to send
//! to the background kernel; no DAG is touched on the UI thread.

use std::collections::HashMap;

use egui::{
    Align2, Color32, CornerRadius, FontId, Painter, PointerButton, Pos2, Rect, Sense, Shape,
    Stroke, StrokeKind, Ui, Vec2, epaint::CubicBezierShape,
};
use tanova_kernel::{
    KernelEvent, KernelMessage, KnobTable, NodeId, PortId, UndoableCommand, find_cartridge,
};

const NODE_W: f32 = 168.0;
const NODE_H: f32 = 72.0;
const HEADER_H: f32 = 24.0;
const PORT_LEAD: f32 = 18.0;
const GRID_SPACING: f32 = 24.0;

const BG: Color32 = Color32::from_rgb(20, 20, 24);
const GRID_MINOR: Color32 = Color32::from_rgb(32, 32, 38);
const GRID_MAJOR: Color32 = Color32::from_rgb(44, 44, 52);
const WIRE: Color32 = Color32::from_rgb(196, 178, 82);
const WIRE_ACTIVE: Color32 = Color32::from_rgb(245, 220, 120);
const NODE_BORDER: Color32 = Color32::from_rgb(110, 110, 125);
const NODE_FILL: Color32 = Color32::from_rgb(44, 44, 50);
const NODE_HEADER: Color32 = Color32::from_rgb(64, 64, 74);
const CRASH_BORDER: Color32 = Color32::from_rgb(230, 70, 70);
const CRASH_FILL: Color32 = Color32::from_rgb(58, 22, 24);
const CRASH_HEADER: Color32 = Color32::from_rgb(92, 30, 34);
const SELECTED: Color32 = Color32::from_rgb(90, 190, 255);
const VIEWER: Color32 = Color32::from_rgb(255, 190, 40);

/// One node as the UI knows it.
#[derive(Debug, Clone)]
pub struct CanvasNode {
    /// Stable node identity.
    pub id: NodeId,
    /// Cartridge kind shown in the node header.
    pub kind: String,
    /// Last panic message, if the kernel reported one.
    pub crashed: Option<String>,
    /// UI-side copy of the node's knobs, updated from `KernelEvent`s.
    pub knobs: KnobTable,
}

/// One connection as the UI knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasEdge {
    /// Upstream node.
    pub from_node: NodeId,
    /// Upstream output port.
    pub from_port: PortId,
    /// Downstream node.
    pub to_node: NodeId,
    /// Downstream input port.
    pub to_port: PortId,
}

/// UI-side mirror of the kernel graph, built purely from [`KernelEvent`]s.
#[derive(Debug, Default)]
pub struct GraphMirror {
    nodes: Vec<CanvasNode>,
    edges: Vec<CanvasEdge>,
}

impl GraphMirror {
    /// Nodes in creation order.
    pub fn nodes(&self) -> &[CanvasNode] {
        &self.nodes
    }

    /// Edges in connection order.
    pub fn edges(&self) -> &[CanvasEdge] {
        &self.edges
    }

    /// Looks up a node.
    pub fn node(&self, id: NodeId) -> Option<&CanvasNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Next free node id for the "add node" toolbar.
    pub fn next_node_id(&self) -> NodeId {
        NodeId(
            self.nodes
                .iter()
                .map(|node| node.id.0)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        )
    }

    /// Folds a kernel event into the mirror.
    pub fn apply_event(&mut self, event: &KernelEvent) {
        match event {
            KernelEvent::Applied(command) | KernelEvent::Redone(command) => {
                self.apply_command(command);
            }
            KernelEvent::Undone(command) => self.apply_command(&command.inverse()),
            KernelEvent::NodeCrashed { node, message } => {
                if let Some(node) = self
                    .nodes
                    .iter_mut()
                    .find(|candidate| candidate.id == *node)
                {
                    node.crashed = Some(message.clone());
                }
            }
            KernelEvent::CommandFailed { .. } | KernelEvent::EpochChanged(_) => {}
        }
    }

    /// Applies the effective change of one command.
    pub fn apply_command(&mut self, command: &UndoableCommand) {
        match command {
            UndoableCommand::AddNode { id, kind } => {
                if !self.nodes.iter().any(|node| node.id == *id) {
                    let knobs = find_cartridge(kind)
                        .map(|descriptor| (descriptor.default_knobs)())
                        .unwrap_or_default();
                    self.nodes.push(CanvasNode {
                        id: *id,
                        kind: kind.clone(),
                        crashed: None,
                        knobs,
                    });
                }
            }
            UndoableCommand::RemoveNode { id, .. } => {
                self.nodes.retain(|node| node.id != *id);
                self.edges
                    .retain(|edge| edge.from_node != *id && edge.to_node != *id);
            }
            UndoableCommand::ConnectEdge {
                from_node,
                from_port,
                to_node,
                to_port,
            } => {
                let edge = CanvasEdge {
                    from_node: *from_node,
                    from_port: *from_port,
                    to_node: *to_node,
                    to_port: *to_port,
                };
                if !self.edges.contains(&edge) {
                    self.edges.push(edge);
                }
            }
            UndoableCommand::DisconnectEdge {
                from_node,
                from_port,
                to_node,
                to_port,
            } => {
                let edge = CanvasEdge {
                    from_node: *from_node,
                    from_port: *from_port,
                    to_node: *to_node,
                    to_port: *to_port,
                };
                self.edges.retain(|candidate| *candidate != edge);
            }
            UndoableCommand::SetKnob {
                node, knob, value, ..
            } => {
                if let Some(node) = self
                    .nodes
                    .iter_mut()
                    .find(|candidate| candidate.id == *node)
                {
                    node.knobs.insert(knob.clone(), value.clone());
                }
            }
        }
    }
}

/// An in-progress wire drag from one node's output port.
#[derive(Debug, Clone, Copy)]
pub struct WireDrag {
    /// Node the wire started from.
    pub from_node: NodeId,
    /// Output port the wire started from.
    pub from_port: PortId,
}

/// Visual-only canvas state.
#[derive(Debug, Clone)]
pub struct NodeCanvasState {
    /// Screen-space pan of the canvas origin.
    pub pan: Vec2,
    /// Zoom factor, clamped by the widget.
    pub zoom: f32,
    /// Canvas-space top-left position of every node.
    pub positions: HashMap<NodeId, Pos2>,
    /// Currently selected node.
    pub selected: Option<NodeId>,
    /// Active output-to-input wire drag.
    pub wire_drag: Option<WireDrag>,
    drag_node: Option<NodeId>,
    panning: bool,
}

impl Default for NodeCanvasState {
    fn default() -> Self {
        Self {
            pan: Vec2::new(80.0, 48.0),
            zoom: 1.0,
            positions: HashMap::new(),
            selected: None,
            wire_drag: None,
            drag_node: None,
            panning: false,
        }
    }
}

impl NodeCanvasState {
    /// Assigns vertical B-pipe positions to nodes that do not have one yet.
    pub fn layout_nodes(&mut self, graph: &GraphMirror) {
        for (index, node) in graph.nodes().iter().enumerate() {
            self.positions
                .entry(node.id)
                .or_insert_with(|| Pos2::new(0.0, index as f32 * (NODE_H + 52.0)));
        }
    }

    /// Canvas-space position of a node.
    pub fn node_position(&self, id: NodeId) -> Option<Pos2> {
        self.positions.get(&id).copied()
    }
}

/// Stateless painter widget for the node graph.
pub struct NodeCanvas;

impl NodeCanvas {
    /// Draws the canvas and returns every kernel message the user asked for.
    pub fn show(
        ui: &mut Ui,
        state: &mut NodeCanvasState,
        graph: &GraphMirror,
        cartridge_kinds: &[&'static str],
        viewer: Option<NodeId>,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();

        ui.horizontal(|ui| {
            ui.label("Add:");
            for kind in cartridge_kinds {
                if ui.button(*kind).clicked() {
                    messages.push(KernelMessage::Execute(UndoableCommand::AddNode {
                        id: graph.next_node_id(),
                        kind: (*kind).to_string(),
                    }));
                }
            }
            ui.separator();
            if ui.button("Undo").clicked() {
                messages.push(KernelMessage::Undo);
            }
            if ui.button("Redo").clicked() {
                messages.push(KernelMessage::Redo);
            }
            if ui.button("Bump Epoch").clicked() {
                messages.push(KernelMessage::BumpEpoch);
            }
            ui.separator();
            if ui.button("View (1)").clicked() {
                if let Some(selected) = state.selected {
                    messages.push(KernelMessage::SetViewerNode(Some(selected)));
                }
            }
            if let Some(viewer) = viewer {
                ui.colored_label(VIEWER, format!("VIEWER 1: #{}", viewer.0));
            }
            ui.separator();
            ui.label(format!("zoom {:.0}%", state.zoom * 100.0));
            if let Some(selected) = state.selected {
                ui.label(format!("selected #{}", selected.0));
            }
        });
        ui.separator();

        let rect = ui.available_rect_before_wrap();
        if rect.width() < 8.0 || rect.height() < 8.0 {
            return messages;
        }
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let pointer_pos = ui.input(|input| input.pointer.interact_pos());
        let pointer_delta = ui.input(|input| input.pointer.delta());
        let primary_pressed = ui.input(|input| input.pointer.primary_pressed());
        let primary_down = ui.input(|input| input.pointer.primary_down());
        let primary_released = ui.input(|input| input.pointer.primary_released());
        let middle_down = ui.input(|input| input.pointer.button_down(PointerButton::Middle));
        let secondary_down = ui.input(|input| input.pointer.button_down(PointerButton::Secondary));
        let scroll = ui.input(|input| input.smooth_scroll_delta.y);

        if response.hovered() && scroll.abs() > 0.0 {
            if let Some(pointer) = pointer_pos {
                zoom_around(state, rect, pointer, scroll);
            }
        }

        state.layout_nodes(graph);

        if primary_pressed && response.hovered() {
            if let Some(pointer) = pointer_pos {
                if let Some(node) = hit_node(state, graph, pointer, rect) {
                    state.selected = Some(node);
                    let node_rect = node_screen_rect(state, node, rect);
                    if pointer.distance(output_port(node_rect, state.zoom))
                        <= port_hit_radius(state.zoom)
                    {
                        state.wire_drag = Some(WireDrag {
                            from_node: node,
                            from_port: PortId(0),
                        });
                    } else {
                        state.drag_node = Some(node);
                    }
                } else {
                    state.selected = None;
                }
            }
        }

        if primary_down {
            if let Some(node) = state.drag_node {
                if let Some(position) = state.positions.get_mut(&node) {
                    *position += pointer_delta / state.zoom;
                }
            }
        }

        if primary_released {
            if let Some(drag) = state.wire_drag.take() {
                if let Some(pointer) = pointer_pos {
                    if let Some((to_node, to_port)) = hit_input_port(state, graph, pointer, rect) {
                        if to_node != drag.from_node {
                            messages.push(KernelMessage::Execute(UndoableCommand::ConnectEdge {
                                from_node: drag.from_node,
                                from_port: drag.from_port,
                                to_node,
                                to_port,
                            }));
                        }
                    }
                }
            }
            state.drag_node = None;
        }

        if middle_down || secondary_down {
            if response.hovered() || state.panning {
                state.panning = true;
                state.pan += pointer_delta;
            }
        } else {
            state.panning = false;
        }

        // Nuke binds `1` to "view this node".
        if ui.input(|input| input.key_pressed(egui::Key::Num1))
            && !ui.ctx().egui_wants_keyboard_input()
        {
            if let Some(selected) = state.selected {
                messages.push(KernelMessage::SetViewerNode(Some(selected)));
            }
        }

        draw_grid(&painter, rect, state.pan, state.zoom);

        for edge in graph.edges() {
            let (Some(from_position), Some(to_position)) = (
                state.node_position(edge.from_node),
                state.node_position(edge.to_node),
            ) else {
                continue;
            };
            let from = output_port(node_screen_rect_at(state, from_position, rect), state.zoom);
            let to = input_port(node_screen_rect_at(state, to_position, rect));
            let label = pipe_label(edge.to_port);
            draw_wire(&painter, from, to, state.zoom, WIRE, Some(&label));
        }

        for node in graph.nodes() {
            let Some(position) = state.node_position(node.id) else {
                continue;
            };
            let node_rect = node_screen_rect_at(state, position, rect);
            draw_node(
                &painter,
                node_rect,
                node,
                state.selected == Some(node.id),
                viewer == Some(node.id),
                state.zoom,
            );
        }

        if let Some(drag) = state.wire_drag {
            if let Some(pointer) = pointer_pos {
                if let Some(position) = state.node_position(drag.from_node) {
                    let from = output_port(node_screen_rect_at(state, position, rect), state.zoom);
                    draw_wire(&painter, from, pointer, state.zoom, WIRE_ACTIVE, None);
                }
            }
        }

        painter.text(
            rect.left_bottom() + Vec2::new(12.0, -10.0),
            Align2::LEFT_BOTTOM,
            "MMB/RMB drag: pan  •  Wheel: zoom  •  Drag output to input: connect",
            FontId::proportional(11.0),
            Color32::from_gray(110),
        );

        messages
    }
}

fn zoom_around(state: &mut NodeCanvasState, rect: Rect, pointer: Pos2, scroll: f32) {
    let old_zoom = state.zoom;
    let new_zoom = (old_zoom * (scroll * 0.0015).exp()).clamp(0.25, 2.5);
    if (new_zoom - old_zoom).abs() <= f32::EPSILON {
        return;
    }

    let canvas_point = (pointer - rect.min - state.pan) / old_zoom;
    state.zoom = new_zoom;
    state.pan = pointer - rect.min - canvas_point * new_zoom;
}

fn node_screen_rect(state: &NodeCanvasState, id: NodeId, rect: Rect) -> Rect {
    state.node_position(id).map_or(Rect::NOTHING, |position| {
        node_screen_rect_at(state, position, rect)
    })
}

fn node_screen_rect_at(state: &NodeCanvasState, position: Pos2, rect: Rect) -> Rect {
    let min = rect.min + state.pan + position.to_vec2() * state.zoom;
    Rect::from_min_size(min, Vec2::new(NODE_W, NODE_H) * state.zoom)
}

fn input_port(rect: Rect) -> Pos2 {
    rect.center_top()
}

fn output_port(rect: Rect, zoom: f32) -> Pos2 {
    rect.center_bottom() + Vec2::new(0.0, PORT_LEAD * zoom)
}

fn port_hit_radius(zoom: f32) -> f32 {
    (12.0 * zoom).max(8.0)
}

fn hit_node(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    pointer: Pos2,
    rect: Rect,
) -> Option<NodeId> {
    graph
        .nodes()
        .iter()
        .rev()
        .find(|node| node_screen_rect(state, node.id, rect).contains(pointer))
        .map(|node| node.id)
}

fn hit_input_port(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    pointer: Pos2,
    rect: Rect,
) -> Option<(NodeId, PortId)> {
    let radius = port_hit_radius(state.zoom);
    graph
        .nodes()
        .iter()
        .rev()
        .find(|node| pointer.distance(input_port(node_screen_rect(state, node.id, rect))) <= radius)
        .map(|node| (node.id, PortId(0)))
}

fn pipe_label(port: PortId) -> String {
    match port.0 {
        0 => "B".to_string(),
        1 => "A".to_string(),
        other => other.to_string(),
    }
}

fn draw_grid(painter: &Painter, rect: Rect, pan: Vec2, zoom: f32) {
    painter.rect_filled(rect, 0.0, BG);

    let mut spacing = GRID_SPACING * zoom;
    while spacing < 12.0 {
        spacing *= 2.0;
    }
    let origin = rect.min + pan;

    let first_x = ((rect.left() - origin.x) / spacing).floor() as i64;
    let last_x = ((rect.right() - origin.x) / spacing).ceil() as i64;
    for index in first_x..=last_x {
        let x = origin.x + index as f32 * spacing;
        let major = index.rem_euclid(5) == 0;
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(
                if major { 1.0 } else { 0.5 },
                if major { GRID_MAJOR } else { GRID_MINOR },
            ),
        );
    }

    let first_y = ((rect.top() - origin.y) / spacing).floor() as i64;
    let last_y = ((rect.bottom() - origin.y) / spacing).ceil() as i64;
    for index in first_y..=last_y {
        let y = origin.y + index as f32 * spacing;
        let major = index.rem_euclid(5) == 0;
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(
                if major { 1.0 } else { 0.5 },
                if major { GRID_MAJOR } else { GRID_MINOR },
            ),
        );
    }
}

fn draw_node(
    painter: &Painter,
    rect: Rect,
    node: &CanvasNode,
    selected: bool,
    is_viewer: bool,
    zoom: f32,
) {
    let crashed = node.crashed.is_some();
    let (fill, header, border) = if crashed {
        (CRASH_FILL, CRASH_HEADER, CRASH_BORDER)
    } else {
        (NODE_FILL, NODE_HEADER, NODE_BORDER)
    };
    let radius = CornerRadius::same((4.0 * zoom).clamp(1.0, 8.0) as u8);

    painter.rect_filled(rect, radius, fill);
    let header_rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), HEADER_H * zoom));
    painter.rect_filled(header_rect, radius, header);
    painter.line_segment(
        [
            Pos2::new(rect.left(), header_rect.bottom()),
            Pos2::new(rect.right(), header_rect.bottom()),
        ],
        Stroke::new(1.0, Color32::from_black_alpha(90)),
    );

    let input = input_port(rect);
    let output = output_port(rect, zoom);
    let stub = Stroke::new((1.5 * zoom).max(0.6), border);

    painter.line_segment(
        [Pos2::new(input.x, input.y - PORT_LEAD * zoom), input],
        stub,
    );
    let arrow = 5.0 * zoom.max(0.7);
    painter.add(Shape::convex_polygon(
        vec![
            Pos2::new(input.x, input.y + arrow),
            Pos2::new(input.x - arrow, input.y - arrow),
            Pos2::new(input.x + arrow, input.y - arrow),
        ],
        border,
        Stroke::NONE,
    ));

    painter.line_segment([rect.center_bottom(), output], stub);
    painter.circle_filled(output, (4.0 * zoom).max(2.5), border);

    let header_font = FontId::proportional((13.0 * zoom).clamp(9.0, 17.0));
    painter.text(
        rect.left_top() + Vec2::new(8.0 * zoom, 4.0 * zoom),
        Align2::LEFT_TOP,
        node.kind.as_str(),
        header_font,
        Color32::from_gray(235),
    );

    let body_font = FontId::proportional((12.0 * zoom).clamp(9.0, 16.0));
    painter.text(
        rect.left_top() + Vec2::new(8.0 * zoom, (HEADER_H + 6.0) * zoom),
        Align2::LEFT_TOP,
        format!("#{}", node.id.0),
        body_font,
        Color32::from_gray(190),
    );

    if crashed {
        painter.text(
            rect.right_bottom() - Vec2::new(7.0 * zoom, 6.0 * zoom),
            Align2::RIGHT_BOTTOM,
            "CRASHED",
            FontId::proportional((10.0 * zoom).clamp(8.0, 13.0)),
            Color32::from_rgb(255, 130, 130),
        );
    }

    painter.rect_stroke(rect, radius, Stroke::new(1.0, border), StrokeKind::Inside);

    if selected {
        painter.rect_stroke(
            rect.expand(2.0),
            radius,
            Stroke::new(2.0, SELECTED),
            StrokeKind::Outside,
        );
    }

    if is_viewer {
        painter.rect_stroke(
            rect.expand(4.0),
            radius,
            Stroke::new(2.0, VIEWER),
            StrokeKind::Outside,
        );
        let tag = Rect::from_min_size(
            rect.right_top() + Vec2::new(-58.0 * zoom, 3.0 * zoom),
            Vec2::new(54.0 * zoom, 13.0 * zoom),
        );
        painter.rect_filled(tag, CornerRadius::same(2), VIEWER);
        painter.text(
            tag.center(),
            Align2::CENTER_CENTER,
            "VIEWER 1",
            FontId::proportional((9.0 * zoom).clamp(7.0, 11.0)),
            Color32::from_rgb(30, 24, 8),
        );
    }
}

fn draw_wire(
    painter: &Painter,
    from: Pos2,
    to: Pos2,
    zoom: f32,
    color: Color32,
    label: Option<&str>,
) {
    let distance = (to.y - from.y).abs().max(40.0 * zoom);
    let control = distance * 0.5;
    let points = [
        from,
        from + Vec2::new(0.0, control),
        to - Vec2::new(0.0, control),
        to,
    ];
    painter.add(Shape::CubicBezier(CubicBezierShape::from_points_stroke(
        points,
        false,
        Color32::TRANSPARENT,
        Stroke::new((1.8 * zoom).max(0.8), color),
    )));

    let head = 6.0 * zoom.max(0.8);
    let tip = to - Vec2::new(0.0, head * 0.5);
    painter.add(Shape::convex_polygon(
        vec![
            tip,
            tip + Vec2::new(-head * 0.6, -head),
            tip + Vec2::new(head * 0.6, -head),
        ],
        color,
        Stroke::NONE,
    ));

    if let Some(label) = label {
        painter.text(
            to + Vec2::new(8.0, 2.0),
            Align2::LEFT_TOP,
            label,
            FontId::proportional((11.0 * zoom).clamp(9.0, 14.0)),
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_node(id: u64) -> UndoableCommand {
        UndoableCommand::AddNode {
            id: NodeId(id),
            kind: "Dummy".to_string(),
        }
    }

    fn connect(from: u64, to: u64) -> UndoableCommand {
        UndoableCommand::ConnectEdge {
            from_node: NodeId(from),
            from_port: PortId(0),
            to_node: NodeId(to),
            to_port: PortId(0),
        }
    }

    #[test]
    fn mirror_follows_applied_undone_and_redone_events() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1)));
        graph.apply_event(&KernelEvent::Applied(add_node(2)));
        graph.apply_event(&KernelEvent::Applied(connect(1, 2)));

        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(graph.next_node_id(), NodeId(3));

        graph.apply_event(&KernelEvent::Undone(connect(1, 2)));
        assert_eq!(graph.edge_count(), 0);

        graph.apply_event(&KernelEvent::Redone(connect(1, 2)));
        assert_eq!(graph.edge_count(), 1);

        graph.apply_event(&KernelEvent::Undone(add_node(2)));
        assert_eq!(graph.node_count(), 1);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn crash_events_mark_the_node() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1)));
        graph.apply_event(&KernelEvent::NodeCrashed {
            node: NodeId(1),
            message: "boom".to_string(),
        });
        assert_eq!(
            graph
                .node(NodeId(1))
                .and_then(|node| node.crashed.as_deref()),
            Some("boom")
        );
    }

    #[test]
    fn layout_stacks_nodes_vertically() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1)));
        graph.apply_event(&KernelEvent::Applied(add_node(2)));

        let mut state = NodeCanvasState::default();
        state.layout_nodes(&graph);

        let first = state.node_position(NodeId(1)).expect("first laid out");
        let second = state.node_position(NodeId(2)).expect("second laid out");
        assert!(second.y > first.y);
        assert_eq!(first.x, second.x);
    }
}
