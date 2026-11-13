//! Custom Nuke-style node canvas drawn directly on `egui::Painter`.
//!
//! The widget owns only visual state (pan, zoom, layout, selection, wire drag).
//! Every graph mutation is returned as a [`KernelMessage`] for the app to send
//! to the background kernel; no DAG is touched on the UI thread.

use std::collections::{BTreeMap, HashMap, HashSet};

use egui::{
    Align2, Color32, CornerRadius, FontId, Key, Modifiers, Painter, PointerButton, Pos2, Rect,
    Sense, Shape, Stroke, StrokeKind, Ui, Vec2, epaint::CubicBezierShape,
};
use tanova_kernel::{
    EdgeSnapshot, KernelEvent, KernelMessage, KnobTable, KnobValue, NodeId, PortId, ProjectFormat,
    UndoableCommand, find_cartridge,
};

use crate::workflow::ViewerWorkflow;

const NODE_W: f32 = 168.0;
const NODE_H: f32 = 72.0;
const HEADER_H: f32 = 24.0;
const PORT_LEAD: f32 = 18.0;
const GRID_SPACING: f32 = 24.0;
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 2.5;
/// Vertical gap used when the Nuke workflow has to create a `Viewer` node.
const VIEWER_OFFSET: f32 = 120.0;

/// Node kinds offered by the canvas context menu.
const CONTEXT_MENU_KINDS: &[&str] = &[
    "Read",
    "Constant",
    "Grade",
    "Transform",
    "Blur",
    "Merge2",
    "Viewer",
];

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
    /// Canvas-space X position carried by the `AddNode` command.
    pub x: f32,
    /// Canvas-space Y position carried by the `AddNode` command.
    pub y: f32,
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

impl CanvasEdge {
    /// Converts to a kernel edge snapshot.
    pub fn snapshot(self) -> EdgeSnapshot {
        EdgeSnapshot::new(self.from_node, self.from_port, self.to_node, self.to_port)
    }
}

/// UI-side mirror of the kernel graph, built purely from [`KernelEvent`]s.
#[derive(Debug, Default)]
pub struct GraphMirror {
    nodes: Vec<CanvasNode>,
    edges: Vec<CanvasEdge>,
    /// Active project format, used to stamp source cartridges on spawn.
    project_format: Option<ProjectFormat>,
}

impl GraphMirror {
    /// Records the active project format so newly spawned sources can inherit
    /// it instead of the cartridge's static default.
    pub fn set_project_format(&mut self, format: ProjectFormat) {
        self.project_format = Some(format);
    }

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

    /// Every edge feeding `id`, in connection order.
    pub fn incoming_edges(&self, id: NodeId) -> Vec<CanvasEdge> {
        self.edges
            .iter()
            .copied()
            .filter(|edge| edge.to_node == id)
            .collect()
    }

    /// The edge feeding one specific input port, if any.
    pub fn incoming_edge(&self, to_node: NodeId, to_port: PortId) -> Option<CanvasEdge> {
        self.edges
            .iter()
            .copied()
            .find(|edge| edge.to_node == to_node && edge.to_port == to_port)
    }

    /// Builds a full-state `RemoveNode` from the mirrored node and its wires.
    pub fn removal_command(&self, id: NodeId) -> Option<UndoableCommand> {
        let node = self.node(id)?;
        let edges = self
            .edges
            .iter()
            .filter(|edge| edge.from_node == id || edge.to_node == id)
            .map(|edge| edge.snapshot())
            .collect();

        Some(UndoableCommand::RemoveNode {
            id,
            kind: node.kind.clone(),
            x: node.x,
            y: node.y,
            knobs: Box::new(node.knobs.clone()),
            edges,
        })
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
            KernelEvent::CommandFailed { .. }
            | KernelEvent::EpochChanged(_)
            | KernelEvent::ViewerChanged(_) => {}
        }
    }

    /// Applies the effective change of one command.
    pub fn apply_command(&mut self, command: &UndoableCommand) {
        match command {
            UndoableCommand::AddNode { id, kind, x, y } => {
                if !self.nodes.iter().any(|node| node.id == *id) {
                    let mut knobs = find_cartridge(kind)
                        .map(|descriptor| (descriptor.default_knobs)())
                        .unwrap_or_default();
                    // Source cartridges default to 1920x1080; when a project
                    // format is active, stamp the real resolution so a fresh
                    // Constant matches the viewer instead of oversizing.
                    if let Some(format) = self.project_format {
                        if knobs.contains_key("format_width") {
                            knobs.insert("format_width", KnobValue::Int(format.width as i64));
                        }
                        if knobs.contains_key("format_height") {
                            knobs.insert("format_height", KnobValue::Int(format.height as i64));
                        }
                    }
                    self.nodes.push(CanvasNode {
                        id: *id,
                        kind: kind.clone(),
                        crashed: None,
                        knobs,
                        x: *x,
                        y: *y,
                    });
                }
            }
            UndoableCommand::RemoveNode { id, .. } => {
                self.nodes.retain(|node| node.id != *id);
                self.edges
                    .retain(|edge| edge.from_node != *id && edge.to_node != *id);
            }
            UndoableCommand::RestoreNode {
                id,
                kind,
                x,
                y,
                knobs,
                edges,
            } => {
                if !self.nodes.iter().any(|node| node.id == *id) {
                    self.nodes.push(CanvasNode {
                        id: *id,
                        kind: kind.clone(),
                        crashed: None,
                        knobs: knobs.as_ref().clone(),
                        x: *x,
                        y: *y,
                    });
                }
                for edge in edges {
                    let edge = CanvasEdge {
                        from_node: edge.from_node,
                        from_port: edge.from_port,
                        to_node: edge.to_node,
                        to_port: edge.to_port,
                    };
                    if !self.edges.contains(&edge) {
                        self.edges.push(edge);
                    }
                }
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
            UndoableCommand::MoveNode { id, to_x, to_y, .. } => {
                if let Some(node) = self.nodes.iter_mut().find(|n| n.id == *id) {
                    node.x = *to_x;
                    node.y = *to_y;
                }
            }
            UndoableCommand::Batch(cmds) => {
                for cmd in cmds {
                    self.apply_command(cmd);
                }
            }
        }
    }
}

/// Which direction a live wire drag is travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireDrag {
    /// Dragging a new wire out of a node's bottom output dot toward an input.
    FromOutput {
        /// Upstream node the wire starts at.
        from_node: NodeId,
        /// Upstream output port (always [`PortId(0)`](PortId)).
        from_port: PortId,
    },
    /// Dragging a new wire out of an empty top input port toward a source.
    FromInput {
        /// Downstream node whose port is being filled.
        to_node: NodeId,
        /// Downstream input port being filled.
        to_port: PortId,
    },
    /// Ripping an existing wire off its downstream port.
    ///
    /// The committed edge is left untouched until release, so `Escape` can
    /// cancel with zero side effects.
    Detaching {
        /// Upstream node the live wire stays anchored to.
        from_node: NodeId,
        /// Upstream output port.
        from_port: PortId,
        /// Downstream node the wire is currently attached to.
        orig_to_node: NodeId,
        /// Downstream input port the wire is currently attached to.
        orig_to_port: PortId,
    },
}

/// A node-menu action chosen from the popup.
#[derive(Debug, Clone, Copy)]
enum NodeMenuAction {
    DisconnectInputs(NodeId),
    Delete(NodeId),
}

/// State of the floating Nuke-style `Tab` quick-search popup.
#[derive(Debug, Clone)]
pub struct TabSearchState {
    /// Screen position where the popup is anchored.
    pub screen_pos: Pos2,
    /// Canvas position where a spawned node lands.
    pub world_pos: Pos2,
    /// Current case-insensitive substring query.
    pub query: String,
    /// `true` on the frame the popup opened; requests keyboard focus once.
    pub just_opened: bool,
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
    /// Screen-space rect the canvas was last laid out in.
    ///
    /// Purely informational: tests use it to map a node's world position onto
    /// the screen so they can aim pointer events at it.
    pub last_canvas_rect: Option<Rect>,
    /// Every selected node. A `HashSet` for O(1) toggles; order never matters
    /// because all iteration that needs order sorts first.
    pub selected_nodes: HashSet<NodeId>,
    /// The deterministic single active node (properties, transform jack, `1`).
    pub primary_selected: Option<NodeId>,
    /// In-progress wire drag.
    pub active_wire_drag: Option<WireDrag>,
    /// In-progress box selection, stored as `(start_screen, current_screen)`.
    pub box_selection: Option<(Pos2, Pos2)>,
    /// Open `Tab` quick-search popup, if any.
    pub tab_search: Option<TabSearchState>,
    /// Whether the physical `Tab` key was held down last frame.
    ///
    /// `InputState::keys_down` survives `consume_key`, so the rising edge still
    /// fires even when egui's focus pass has already eaten the key event.
    pub was_tab_down: bool,
    /// True while a latched node-body drag is in progress.
    dragging_nodes: bool,
    /// Canvas position of every dragged node at the moment the button went
    /// down. Stamped as the `from` side of every `MoveNode` for the gesture, so
    /// one `Undo` always returns the node to where the drag started — even when
    /// the artist pauses mid-drag and the moves do not coalesce.
    drag_start_positions: BTreeMap<NodeId, (f32, f32)>,
    /// Monotonic high-water mark for node id allocation. Never decreases, so
    /// deleting the highest node and spawning a new one never reuses a deleted
    /// `NodeId` that might still be on the `UndoStack`.
    next_node_id: u64,
    box_dragging: bool,
    panning: bool,
    context_menu_pos: Option<Pos2>,
    context_menu_open: bool,
    node_menu_pos: Option<Pos2>,
    node_menu_node: Option<NodeId>,
    node_menu_open: bool,
}

impl Default for NodeCanvasState {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            positions: HashMap::new(),
            last_canvas_rect: None,
            selected_nodes: HashSet::new(),
            primary_selected: None,
            active_wire_drag: None,
            box_selection: None,
            tab_search: None,
            was_tab_down: false,
            dragging_nodes: false,
            drag_start_positions: BTreeMap::new(),
            next_node_id: 1,
            box_dragging: false,
            panning: false,
            context_menu_pos: None,
            context_menu_open: false,
            node_menu_pos: None,
            node_menu_node: None,
            node_menu_open: false,
        }
    }
}

impl NodeCanvasState {
    /// Maps a screen-space point into canvas space.
    pub fn screen_to_world(&self, viewport: Rect, pos: Pos2) -> Pos2 {
        Pos2::new(
            (pos.x - viewport.min.x - self.pan.x) / self.zoom,
            (pos.y - viewport.min.y - self.pan.y) / self.zoom,
        )
    }

    /// Maps a canvas-space point back into screen space.
    pub fn world_to_screen(&self, viewport: Rect, pos: Pos2) -> Pos2 {
        Pos2::new(
            viewport.min.x + self.pan.x + pos.x * self.zoom,
            viewport.min.y + self.pan.y + pos.y * self.zoom,
        )
    }

    /// Seeds a position for every mirrored node from its `AddNode` coordinates.
    pub fn layout_nodes(&mut self, graph: &GraphMirror) {
        for node in graph.nodes() {
            // The canvas leads while a node is being dragged, so the artist's
            // motion is never snapped back by a lagging mirror. Every other
            // frame the mirror is authoritative, which is what lets an undo
            // (or redo) move the node on screen.
            if self.dragging_nodes && self.selected_nodes.contains(&node.id) {
                continue;
            }
            self.positions.insert(node.id, Pos2::new(node.x, node.y));
        }
    }

    /// `true` while the `Tab` quick-search popup owns the keyboard.
    pub fn is_search_open(&self) -> bool {
        self.tab_search.is_some()
    }

    /// Canvas-space position of a node.
    pub fn node_position(&self, id: NodeId) -> Option<Pos2> {
        self.positions.get(&id).copied()
    }

    /// Collects every node intersecting a screen-space box, sorted by id.
    ///
    /// The box corners are converted into canvas space *before* the test, so
    /// panning and zooming can never desynchronise the selection.
    pub fn nodes_in_screen_box(
        &self,
        graph: &GraphMirror,
        viewport: Rect,
        start: Pos2,
        current: Pos2,
    ) -> Vec<NodeId> {
        let selection = Rect::from_two_pos(
            self.screen_to_world(viewport, start),
            self.screen_to_world(viewport, current),
        );

        let mut hits: Vec<NodeId> = graph
            .nodes()
            .iter()
            .filter(|node| {
                self.positions.get(&node.id).is_some_and(|position| {
                    Rect::from_min_size(*position, Vec2::new(NODE_W, NODE_H)).intersects(selection)
                })
            })
            .map(|node| node.id)
            .collect();
        hits.sort();
        hits
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
        viewer: Option<NodeId>,
        workflow: ViewerWorkflow,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();

        ui.horizontal(|ui| {
            // Nuke-style pane tab: the canvas is a pane, not a toolbar. The
            // tab names the pane; every action lives on the canvas itself, in
            // the context menu, or on the keyboard.
            egui::Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .inner_margin(egui::Margin::symmetric(10, 3))
                .corner_radius(egui::CornerRadius {
                    nw: 4,
                    ne: 4,
                    sw: 0,
                    se: 0,
                })
                .show(ui, |ui| {
                    ui.strong("Node Graph");
                });
            ui.separator();
            if ui.button("View (1)").clicked() {
                if let Some(selected) = state.primary_selected {
                    messages.push(viewer_message(graph, workflow, selected));
                }
            }
            if let Some(viewer) = viewer {
                ui.colored_label(VIEWER, format!("VIEWER 1: #{}", viewer.0));
            }
            ui.separator();
            ui.label(format!("zoom {:.0}%", state.zoom * 100.0));
            if !state.selected_nodes.is_empty() {
                ui.label(format!(
                    "{} selected (#{})",
                    state.selected_nodes.len(),
                    state.primary_selected.map_or(0, |id| id.0)
                ));
            }
        });
        ui.separator();

        let rect = ui.available_rect_before_wrap();
        if rect.width() < 8.0 || rect.height() < 8.0 {
            return messages;
        }
        state.last_canvas_rect = Some(rect);
        // Deliberately not `Sense::FOCUSABLE`: the canvas must never hold the
        // keyboard, otherwise `memory.focused()` would stay set and every
        // Delete/`1` press would look like the artist is typing in a field.
        let response = ui.allocate_rect(rect, Sense::CLICK | Sense::DRAG);
        let painter = ui.painter_at(rect);

        let pointer_pos = ui.ctx().input(|input| {
            input
                .pointer
                .hover_pos()
                .or_else(|| input.pointer.latest_pos())
        });
        let pointer_delta = ui.input(|input| input.pointer.delta());
        let primary_pressed = ui.input(|input| input.pointer.primary_pressed());
        let primary_down = ui.input(|input| input.pointer.primary_down());
        let primary_released = ui.input(|input| input.pointer.primary_released());
        let middle_down = ui.input(|input| input.pointer.button_down(PointerButton::Middle));
        let scroll = ui.input(|input| input.smooth_scroll_delta.y);
        let shift = ui.input(|input| input.modifiers.shift);
        let escape_pressed = ui.input(|input| input.key_pressed(Key::Escape));

        // egui reports a primary press globally, so a click inside the
        // PropertiesPanel, ViewerPanel, or toolbar would otherwise reach the
        // canvas, start a box selection, and clear the selection on release.
        // Only *initiating* a drag needs the origin check: once a drag is
        // latched the per-frame updates and release cleanup keep running even
        // if the pointer leaves the canvas.
        let press_origin = ui.input(|input| {
            input
                .pointer
                .press_origin()
                .or_else(|| input.pointer.interact_pos())
        });
        let press_starts_inside = press_origin.is_none_or(|origin| rect.contains(origin));

        state.layout_nodes(graph);

        // Prune UI state for nodes that no longer exist in the mirror.
        state.selected_nodes.retain(|id| graph.node(*id).is_some());
        if state
            .primary_selected
            .is_some_and(|id| graph.node(id).is_none())
        {
            state.primary_selected = state.selected_nodes.iter().min().copied();
        }
        if state.active_wire_drag.is_some() {
            let drag_references_node = match state.active_wire_drag.as_ref() {
                Some(WireDrag::FromOutput { from_node, .. }) => graph.node(*from_node).is_some(),
                Some(WireDrag::FromInput { to_node, .. }) => graph.node(*to_node).is_some(),
                Some(WireDrag::Detaching {
                    from_node,
                    orig_to_node,
                    ..
                }) => graph.node(*from_node).is_some() && graph.node(*orig_to_node).is_some(),
                None => false,
            };
            if !drag_references_node {
                state.active_wire_drag = None;
            }
        }

        // Advance the monotonic node-id high-water mark so deleted ids are
        // never reused while they might still be on the undo stack.
        let max_id = graph
            .nodes()
            .iter()
            .map(|node| node.id.0)
            .max()
            .unwrap_or(0);
        state.next_node_id = state.next_node_id.max(max_id + 1);

        // Physical `Tab` opens (or toggles) the quick search. `consume_key`
        // alone is not enough: egui's focus pass may have already eaten the
        // event, so the rising edge of `keys_down` is the reliable signal.
        let tab_event = ui
            .ctx()
            .input_mut(|input| input.consume_key(Modifiers::NONE, Key::Tab));
        let tab_down = ui.ctx().input(|input| input.key_down(Key::Tab));
        let tab_just_pressed = tab_event || (tab_down && !state.was_tab_down);
        state.was_tab_down = tab_down;

        let over_canvas = pointer_pos.is_none_or(|pointer| rect.contains(pointer));
        if tab_just_pressed && state.active_wire_drag.is_none() {
            if state.tab_search.is_some() {
                state.tab_search = None;
            } else if over_canvas {
                // A toolbar button may hold focus; the popup needs the
                // keyboard, so hand it over before opening.
                ui.ctx().memory_mut(|memory| {
                    if let Some(id) = memory.focused() {
                        memory.surrender_focus(id);
                    }
                });
                let raw_anchor = pointer_pos.unwrap_or_else(|| rect.center());
                let anchor = Pos2::new(
                    raw_anchor.x.clamp(rect.left(), rect.right()),
                    raw_anchor.y.clamp(rect.top(), rect.bottom()),
                );
                state.tab_search = Some(TabSearchState {
                    screen_pos: anchor,
                    world_pos: state.screen_to_world(rect, raw_anchor),
                    query: String::new(),
                    just_opened: true,
                });
            }
        }
        let search_open = state.is_search_open();

        if response.hovered() && scroll.abs() > 0.0 {
            if let Some(pointer) = pointer_pos {
                zoom_around(state, rect, pointer, scroll);
            }
        }

        // Escape aborts a wire drag with zero kernel traffic.
        if escape_pressed && state.active_wire_drag.is_some() {
            messages.extend(cancel_wire_drag(state));
        }

        // A click on the canvas means the artist is done with any knob field in
        // the properties panel. Releasing its focus keeps `Ctrl+Z` flowing to
        // the canvas instead of being swallowed by the text edit.
        if primary_pressed && press_starts_inside && state.tab_search.is_none() {
            if let Some(focused_id) = ui.ctx().memory(|mem| mem.focused()) {
                ui.ctx().memory_mut(|mem| mem.surrender_focus(focused_id));
            }
        }

        if primary_pressed && press_starts_inside {
            if let Some(pointer) = pointer_pos {
                if let Some((to_node, to_port)) = hit_input_port(state, graph, pointer, rect) {
                    // Grabbing a connected port rips the wire off, but the
                    // committed edge is only removed on release.
                    state.active_wire_drag = match graph.incoming_edge(to_node, to_port) {
                        Some(edge) => Some(WireDrag::Detaching {
                            from_node: edge.from_node,
                            from_port: edge.from_port,
                            orig_to_node: to_node,
                            orig_to_port: to_port,
                        }),
                        None => Some(WireDrag::FromInput { to_node, to_port }),
                    };
                    state.box_selection = None;
                    state.dragging_nodes = false;
                } else if let Some(from_node) = hit_output_port(state, graph, pointer, rect) {
                    state.active_wire_drag = Some(WireDrag::FromOutput {
                        from_node,
                        from_port: PortId(0),
                    });
                    state.box_selection = None;
                    state.dragging_nodes = false;
                } else if let Some(node) = hit_node(state, graph, pointer, rect) {
                    apply_node_click(state, node, shift);
                    state.dragging_nodes = true;
                    // Latch the pre-drag position of every selected node from
                    // the mirror. Every frame of this gesture reports these as
                    // `from`, so undo returns the whole group to this exact
                    // spot even if the moves never coalesce.
                    state.drag_start_positions.clear();
                    for id in &state.selected_nodes {
                        if let Some(node) = graph.node(*id) {
                            state.drag_start_positions.insert(*id, (node.x, node.y));
                        }
                    }
                } else {
                    // Empty background: start a world-space box selection.
                    state.dragging_nodes = false;
                    if !shift {
                        state.selected_nodes.clear();
                    } else {
                        state.primary_selected = None;
                    }
                    state.box_selection = Some((pointer, pointer));
                    state.box_dragging = true;
                }
            }
        }

        if primary_down {
            if state.box_dragging {
                if let Some(pointer) = pointer_pos {
                    if let Some((start, _)) = state.box_selection {
                        state.box_selection = Some((start, pointer));
                    }
                }
            } else if state.dragging_nodes {
                // Apply the pointer delta in world units so motion matches the
                // cursor at any zoom, then mirror it to the kernel immediately.
                let world_delta = pointer_delta / state.zoom.max(MIN_ZOOM);
                // A stationary frame (press, holding still, release) must never
                // emit a command, otherwise the undo stack fills with no-ops.
                if world_delta.length_sq() > 1e-6 {
                    let mut moves: Vec<UndoableCommand> = Vec::new();
                    for id in state
                        .drag_start_positions
                        .keys()
                        .copied()
                        .collect::<Vec<_>>()
                    {
                        if !state.selected_nodes.contains(&id) {
                            continue;
                        }
                        let Some(node) = graph.node(id) else {
                            continue;
                        };
                        let (cur_x, cur_y) = (node.x, node.y);
                        let (from_x, from_y) = state
                            .drag_start_positions
                            .get(&id)
                            .copied()
                            .unwrap_or((cur_x, cur_y));
                        let to_x = cur_x + world_delta.x;
                        let to_y = cur_y + world_delta.y;
                        // Keep the local cache in step for this frame's draw.
                        state.positions.insert(id, Pos2::new(to_x, to_y));
                        moves.push(UndoableCommand::MoveNode {
                            id,
                            from_x,
                            from_y,
                            to_x,
                            to_y,
                        });
                    }
                    // `drag_start_positions` is a `BTreeMap`, so the moves are
                    // already sorted by `NodeId`.
                    if let Some(batch) = UndoableCommand::from_vec(moves) {
                        messages.push(KernelMessage::Execute(batch));
                    }
                }
            }
        }

        if primary_released {
            if let Some(drag) = state.active_wire_drag.take() {
                if let Some(pointer) = pointer_pos {
                    let target_input = hit_input_port(state, graph, pointer, rect).or_else(|| {
                        hit_node(state, graph, pointer, rect)
                            .filter(|id| node_accepts_inputs(graph, *id))
                            .map(|id| (id, PortId(0)))
                    });
                    let source_node = hit_output_port(state, graph, pointer, rect)
                        .or_else(|| hit_node(state, graph, pointer, rect));

                    let mut command = resolve_wire_release(drag, target_input, source_node);

                    // Dropping onto an occupied input replaces its wire. Prepend
                    // a DisconnectEdge so `Ctrl+Z` restores the previous
                    // connection instead of leaving the port empty.
                    if let Some(UndoableCommand::ConnectEdge {
                        from_node,
                        from_port,
                        to_node,
                        to_port,
                    }) = &command
                    {
                        if let Some(existing) = graph.incoming_edge(*to_node, *to_port) {
                            if existing.from_node != *from_node || existing.from_port != *from_port
                            {
                                command = UndoableCommand::from_vec(vec![
                                    UndoableCommand::DisconnectEdge {
                                        from_node: existing.from_node,
                                        from_port: existing.from_port,
                                        to_node: existing.to_node,
                                        to_port: existing.to_port,
                                    },
                                    command.clone().unwrap(),
                                ]);
                            }
                        }
                    }

                    if let Some(cmd) = command {
                        let viewer_connect = match &cmd {
                            UndoableCommand::ConnectEdge { to_node, .. } => Some(*to_node),
                            UndoableCommand::Batch(cmds) => {
                                // Find the last ConnectEdge in the batch for
                                // viewer binding.
                                cmds.iter().rev().find_map(|c| match c {
                                    UndoableCommand::ConnectEdge { to_node, .. } => Some(*to_node),
                                    _ => None,
                                })
                            }
                            _ => None,
                        };
                        messages.push(KernelMessage::Execute(cmd));

                        // Nuke workflow: wiring a physical Viewer also makes
                        // the viewer panel look through it immediately.
                        if workflow.exposes_physical_viewer() {
                            if let Some(to_node) = viewer_connect {
                                if graph
                                    .node(to_node)
                                    .is_some_and(|node| node.kind == "Viewer")
                                {
                                    messages.push(KernelMessage::SetViewerNode(Some(to_node)));
                                }
                            }
                        }
                    }
                }
            }

            if state.box_dragging {
                state.box_dragging = false;
                if let Some((start, current)) = state.box_selection.take() {
                    let hits = state.nodes_in_screen_box(graph, rect, start, current);
                    if shift {
                        for id in &hits {
                            state.selected_nodes.insert(*id);
                        }
                        if let Some(first) = hits.first() {
                            state.primary_selected = Some(*first);
                        } else if state.primary_selected.is_none() {
                            state.primary_selected = state.selected_nodes.iter().min().copied();
                        }
                    } else {
                        state.selected_nodes = hits.iter().copied().collect();
                        state.primary_selected = hits.first().copied();
                    }
                }
            }

            // The per-frame `MoveNode` stream already reached the kernel, so
            // releasing only clears the latched drag.
            if state.dragging_nodes {
                state.dragging_nodes = false;
                state.drag_start_positions.clear();
            }
        }

        if middle_down {
            if response.hovered() || state.panning {
                state.panning = true;
                state.pan += pointer_delta;
            }
        } else {
            state.panning = false;
        }

        // Right-click: cancel a live wire with no side effects, otherwise open
        // the node menu (over a body) or the spawn menu (over background).
        if response.secondary_clicked() {
            if state.active_wire_drag.is_some() {
                messages.extend(cancel_wire_drag(state));
            } else if let Some(pointer) = pointer_pos {
                if let Some(node) = hit_node(state, graph, pointer, rect) {
                    if !state.selected_nodes.contains(&node) {
                        state.selected_nodes.clear();
                        state.selected_nodes.insert(node);
                    }
                    state.primary_selected = Some(node);
                    state.context_menu_open = false;
                    state.context_menu_pos = None;
                    state.node_menu_node = Some(node);
                    state.node_menu_pos = Some(pointer);
                    state.node_menu_open = true;
                } else {
                    state.node_menu_open = false;
                    state.node_menu_pos = None;
                    state.node_menu_node = None;
                    state.context_menu_pos = Some(pointer);
                    state.context_menu_open = true;
                }
            }
        }

        // Node-specific menu.
        if let Some(anchor) = state.node_menu_pos {
            let node_id = state.node_menu_node;
            let mut open = state.node_menu_open;
            let mut action: Option<NodeMenuAction> = None;

            egui::Popup::context_menu(&response)
                .at_position(anchor)
                .open_bool(&mut open)
                .show(|ui| {
                    if let Some(node_id) = node_id {
                        ui.label(format!("Node #{}", node_id.0));
                        ui.separator();
                        if ui.button("Disconnect Inputs").clicked() {
                            action = Some(NodeMenuAction::DisconnectInputs(node_id));
                            ui.close();
                        }
                        if ui.button("Delete Node").clicked() {
                            action = Some(NodeMenuAction::Delete(node_id));
                            ui.close();
                        }
                    }
                });

            match action {
                Some(NodeMenuAction::DisconnectInputs(id)) => {
                    let mut disconnects: Vec<UndoableCommand> = Vec::new();
                    for edge in graph.incoming_edges(id) {
                        disconnects.push(UndoableCommand::DisconnectEdge {
                            from_node: edge.from_node,
                            from_port: edge.from_port,
                            to_node: edge.to_node,
                            to_port: edge.to_port,
                        });
                    }
                    if let Some(batch) = UndoableCommand::from_vec(disconnects) {
                        messages.push(KernelMessage::Execute(batch));
                    }
                    state.node_menu_open = false;
                    state.node_menu_pos = None;
                    state.node_menu_node = None;
                }
                Some(NodeMenuAction::Delete(id)) => {
                    if let Some(command) = graph.removal_command(id) {
                        messages.push(KernelMessage::Execute(command));
                    }
                    state.selected_nodes.remove(&id);
                    if state.primary_selected == Some(id) {
                        state.primary_selected = state.selected_nodes.iter().min().copied();
                    }
                    state.node_menu_open = false;
                    state.node_menu_pos = None;
                    state.node_menu_node = None;
                }
                None => {
                    state.node_menu_open = open;
                    if !open {
                        state.node_menu_pos = None;
                        state.node_menu_node = None;
                    }
                }
            }
        }

        // Background spawn menu. Fusion hides the physical Viewer so the two
        // paradigms never compete in the same menu.
        if let Some(anchor) = state.context_menu_pos {
            let mut open = state.context_menu_open;
            let mut chosen: Option<&'static str> = None;

            egui::Popup::context_menu(&response)
                .at_position(anchor)
                .open_bool(&mut open)
                .show(|ui| {
                    ui.label("Add node");
                    ui.separator();
                    for kind in spawnable_kinds(workflow) {
                        if ui.button(kind).clicked() {
                            chosen = Some(kind);
                            ui.close();
                        }
                    }
                });

            if let Some(kind) = chosen {
                let position = state.screen_to_world(rect, anchor);
                messages.push(add_node_message(state, kind, position));
                state.context_menu_open = false;
                state.context_menu_pos = None;
            } else {
                state.context_menu_open = open;
                if !open {
                    state.context_menu_pos = None;
                }
            }
        }

        // Focus-safe shortcuts: never act while a text field or a numeric drag
        // value owns the keyboard.
        let keyboard_busy = keyboard_is_busy(ui);

        // Nuke binds `1` to "view this node".
        if ui.input(|input| input.key_pressed(Key::Num1)) && !keyboard_busy && !search_open {
            if let Some(selected) = state.primary_selected {
                messages.push(viewer_message(graph, workflow, selected));
            }
        }

        let delete_pressed =
            ui.input(|input| input.key_pressed(Key::Delete) || input.key_pressed(Key::Backspace));
        if delete_pressed
            && !keyboard_busy
            && !search_open
            && state.active_wire_drag.is_none()
            && state.box_selection.is_none()
        {
            let mut removals: Vec<UndoableCommand> = Vec::new();
            let mut ids: Vec<NodeId> = state.selected_nodes.iter().copied().collect();
            ids.sort();
            for id in ids {
                if let Some(command) = graph.removal_command(id) {
                    removals.push(command);
                }
            }
            if let Some(batch) = UndoableCommand::from_vec(removals) {
                messages.push(KernelMessage::Execute(batch));
            }
            state.selected_nodes.clear();
            state.primary_selected = None;
        }

        draw_grid(&painter, rect, state.pan, state.zoom);

        // A detaching wire hides its committed edge until release.
        let detached = match state.active_wire_drag {
            Some(WireDrag::Detaching {
                orig_to_node,
                orig_to_port,
                ..
            }) => Some((orig_to_node, orig_to_port)),
            _ => None,
        };

        for edge in graph.edges() {
            if detached == Some((edge.to_node, edge.to_port)) {
                continue;
            }
            let (Some(from_position), Some(to_position), Some(to_node)) = (
                state.node_position(edge.from_node),
                state.node_position(edge.to_node),
                graph.node(edge.to_node),
            ) else {
                continue;
            };
            let from_rect = node_screen_rect_at(state, from_position, rect);
            let to_rect = node_screen_rect_at(state, to_position, rect);
            let from = output_port(from_rect, state.zoom);
            let port_count = input_port_count(&to_node.kind);
            let to = input_port(to_rect, edge.to_port, port_count);
            let label = input_port_label(&to_node.kind, port_count, edge.to_port.0 as usize);
            let label = if label.is_empty() { None } else { Some(label) };
            draw_wire(&painter, from, to, state.zoom, WIRE, label);
        }

        for node in graph.nodes() {
            let Some(position) = state.node_position(node.id) else {
                continue;
            };
            let node_rect = node_screen_rect_at(state, position, rect);
            let active_port = match state.active_wire_drag {
                Some(WireDrag::FromInput { to_node, to_port }) if to_node == node.id => {
                    Some(to_port)
                }
                Some(WireDrag::Detaching {
                    orig_to_node,
                    orig_to_port,
                    ..
                }) if orig_to_node == node.id => Some(orig_to_port),
                _ => None,
            };
            let is_viewer = viewer == Some(node.id)
                && (workflow != ViewerWorkflow::Nuke || node.kind == "Viewer");
            draw_node(
                &painter,
                node_rect,
                node,
                state.selected_nodes.contains(&node.id),
                is_viewer,
                state.zoom,
                active_port,
            );
        }

        if let Some(pointer) = pointer_pos {
            if let Some((from, to)) = live_wire_points(state, graph, rect, pointer) {
                draw_wire(&painter, from, to, state.zoom, WIRE_ACTIVE, None);
            }
        }

        if let Some((start, current)) = state.box_selection {
            let selection = Rect::from_two_pos(start, current);
            painter.rect_filled(
                selection,
                0,
                Color32::from_rgba_unmultiplied(90, 190, 255, 28),
            );
            painter.rect_stroke(
                selection,
                0,
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(120, 200, 255, 170)),
                StrokeKind::Inside,
            );
        }

        if let Some(message) = draw_tab_search(ui, state, graph, workflow) {
            messages.push(message);
        }

        painter.text(
            rect.left_bottom() + Vec2::new(12.0, -10.0),
            Align2::LEFT_BOTTOM,
            "Tab: search  •  MMB: pan  •  Wheel: zoom  •  Shift: multi-select  •  Drag wire: connect/rip  •  RMB: menu  •  Del: remove",
            FontId::proportional(11.0),
            Color32::from_gray(110),
        );

        messages
    }
}

/// True when a text field or any focused widget owns the keyboard.
///
/// Deletion and the `1` hotkey must be inert while the artist is typing in a
/// knob field, otherwise `Backspace` would silently delete the selection.
fn keyboard_is_busy(ui: &Ui) -> bool {
    let ctx = ui.ctx();
    ctx.egui_wants_keyboard_input()
        || ctx.text_edit_focused()
        || ui.memory(|memory| memory.focused().is_some())
}

/// User-facing node kinds for the active workflow.
///
/// `Dummy` stays registered for cartridge tests but is never offered to an
/// artist. The physical `Viewer` only exists in the Nuke workflow.
pub fn spawnable_kinds(workflow: ViewerWorkflow) -> Vec<&'static str> {
    CONTEXT_MENU_KINDS
        .iter()
        .copied()
        .filter(|kind| *kind != "Dummy")
        .filter(|kind| *kind != "Viewer" || workflow.exposes_physical_viewer())
        .collect()
}

/// Case-insensitive substring filter over [`spawnable_kinds`].
pub fn matching_kinds(workflow: ViewerWorkflow, query: &str) -> Vec<&'static str> {
    let needle = query.trim().to_ascii_lowercase();
    spawnable_kinds(workflow)
        .into_iter()
        .filter(|kind| needle.is_empty() || kind.to_ascii_lowercase().contains(&needle))
        .collect()
}

/// Message that enforces the Nuke binding rule after a workflow switch.
///
/// Entering Nuke mode may only bind a physical `Viewer`. If the current target
/// is a plain node the binding moves to an existing `Viewer`, or clears when
/// the graph has none so the viewer goes blank.
pub fn nuke_workflow_binding_message(
    graph: &GraphMirror,
    current: Option<NodeId>,
) -> KernelMessage {
    if current
        .and_then(|id| graph.node(id))
        .is_some_and(|node| node.kind == "Viewer")
    {
        return KernelMessage::SetViewerNode(current);
    }
    let viewer = graph
        .nodes()
        .iter()
        .find(|node| node.kind == "Viewer")
        .map(|node| node.id);
    KernelMessage::SetViewerNode(viewer)
}

/// Draws the `Tab` quick-search popup and returns a spawn command if chosen.
fn draw_tab_search(
    ui: &mut Ui,
    state: &mut NodeCanvasState,
    _graph: &GraphMirror,
    workflow: ViewerWorkflow,
) -> Option<KernelMessage> {
    state.tab_search.as_ref()?;
    if ui.input(|input| input.key_pressed(Key::Escape)) {
        state.tab_search = None;
        return None;
    }

    let mut search = state.tab_search.take()?;
    let just_opened = search.just_opened;
    search.just_opened = false;
    // A stray tab/newline must never leak into the query string.
    search.query.retain(|c| c != '\t' && c != '\n' && c != '\r');

    let mut spawn: Option<&'static str> = None;

    let area = egui::Area::new(egui::Id::unique("tanova_tab_search"))
        .order(egui::Order::Foreground)
        .fixed_pos(search.screen_pos)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(200.0);
                let text = ui.add(
                    egui::TextEdit::singleline(&mut search.query)
                        .hint_text("Search nodes…")
                        .desired_width(190.0),
                );
                // Enter spawns the first match; otherwise keep the keyboard
                // inside the popup even after a Tab press or a canvas click.
                if ui.input(|input| input.key_pressed(Key::Enter)) {
                    spawn = matching_kinds(workflow, &search.query).first().copied();
                } else if !text.has_focus() {
                    text.request_focus();
                }
                ui.separator();
                let matches = matching_kinds(workflow, &search.query);
                if matches.is_empty() {
                    ui.weak("No matching node");
                } else {
                    for kind in matches {
                        if ui.button(kind).clicked() {
                            spawn = Some(kind);
                        }
                    }
                }
            });
        });

    // Clicking outside dismisses the popup, but never the click that opened it.
    let mut close = false;
    if !just_opened && ui.input(|input| input.pointer.any_pressed()) {
        if let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) {
            if !area.response.rect.contains(pointer) {
                close = true;
            }
        }
    }

    if let Some(kind) = spawn {
        state.tab_search = None;
        return Some(add_node_message(state, kind, search.world_pos));
    }
    if close {
        state.tab_search = None;
    } else {
        state.tab_search = Some(search);
    }
    None
}

/// Builds the viewer message for the active workflow.
fn viewer_message(graph: &GraphMirror, workflow: ViewerWorkflow, node: NodeId) -> KernelMessage {
    match workflow {
        ViewerWorkflow::Fusion => KernelMessage::SetViewerNode(Some(node)),
        ViewerWorkflow::Nuke => {
            let (x, y) = graph
                .node(node)
                .map(|node| (node.x, node.y))
                .unwrap_or((0.0, 0.0));
            KernelMessage::RouteNukeViewer {
                target_node: node,
                fallback_pos: (x, y + VIEWER_OFFSET),
            }
        }
    }
}

/// Builds an `AddNode` command using the canvas's monotonic node-id allocator.
fn add_node_message(state: &NodeCanvasState, kind: &str, position: Pos2) -> KernelMessage {
    KernelMessage::Execute(UndoableCommand::AddNode {
        id: NodeId(state.next_node_id),
        kind: kind.to_string(),
        x: position.x,
        y: position.y,
    })
}

/// Applies one click to the deterministic multi-selection.
fn apply_node_click(state: &mut NodeCanvasState, id: NodeId, shift: bool) {
    if shift {
        if state.selected_nodes.remove(&id) {
            if state.primary_selected == Some(id) {
                state.primary_selected = state.selected_nodes.iter().min().copied();
            }
        } else {
            state.selected_nodes.insert(id);
            state.primary_selected = Some(id);
        }
    } else {
        state.primary_selected = Some(id);
        // Clicking inside an existing multi-selection keeps it intact so the
        // following drag moves the whole group.
        if !state.selected_nodes.contains(&id) {
            state.selected_nodes.clear();
            state.selected_nodes.insert(id);
        }
    }
}

/// Cancels a live wire drag. Returns no messages by construction.
fn cancel_wire_drag(state: &mut NodeCanvasState) -> Vec<KernelMessage> {
    state.active_wire_drag = None;
    Vec::new()
}

/// Resolves a wire release into kernel commands.
///
/// `target_input` is the input port under the cursor (or the body of a node
/// that accepts inputs, defaulted to port 0); `source_node` is the node whose
/// output dot or body is under the cursor.
///
/// Returns a single [`UndoableCommand`], possibly a [`Batch`], so the whole
/// wire swap is undoable in one step.
fn resolve_wire_release(
    drag: WireDrag,
    target_input: Option<(NodeId, PortId)>,
    source_node: Option<NodeId>,
) -> Option<UndoableCommand> {
    match drag {
        WireDrag::FromOutput {
            from_node,
            from_port,
        } => match target_input {
            Some((to_node, to_port)) if to_node != from_node => {
                UndoableCommand::from_vec(vec![UndoableCommand::ConnectEdge {
                    from_node,
                    from_port,
                    to_node,
                    to_port,
                }])
            }
            _ => None,
        },
        WireDrag::FromInput { to_node, to_port } => match source_node {
            Some(from_node) if from_node != to_node => {
                UndoableCommand::from_vec(vec![UndoableCommand::ConnectEdge {
                    from_node,
                    from_port: PortId(0),
                    to_node,
                    to_port,
                }])
            }
            _ => None,
        },
        WireDrag::Detaching {
            from_node,
            from_port,
            orig_to_node,
            orig_to_port,
        } => {
            let disconnect = UndoableCommand::DisconnectEdge {
                from_node,
                from_port,
                to_node: orig_to_node,
                to_port: orig_to_port,
            };

            match target_input {
                Some((new_to_node, new_to_port)) if new_to_node != from_node => {
                    if new_to_node == orig_to_node && new_to_port == orig_to_port {
                        // Dropped back where it started: the committed edge is
                        // still valid, so this is a no-op.
                        None
                    } else {
                        UndoableCommand::from_vec(vec![
                            disconnect,
                            UndoableCommand::ConnectEdge {
                                from_node,
                                from_port,
                                to_node: new_to_node,
                                to_port: new_to_port,
                            },
                        ])
                    }
                }
                // Empty canvas, the source itself, or an invalid target.
                _ => UndoableCommand::from_vec(vec![disconnect]),
            }
        }
    }
}

/// Screen-space endpoints of the live wire, if a drag is active.
fn live_wire_points(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    rect: Rect,
    pointer: Pos2,
) -> Option<(Pos2, Pos2)> {
    match state.active_wire_drag? {
        WireDrag::FromOutput { from_node, .. } => {
            let position = state.node_position(from_node)?;
            Some((
                output_port(node_screen_rect_at(state, position, rect), state.zoom),
                pointer,
            ))
        }
        WireDrag::FromInput { to_node, to_port } => {
            let position = state.node_position(to_node)?;
            let node = graph.node(to_node)?;
            Some((
                pointer,
                input_port(
                    node_screen_rect_at(state, position, rect),
                    to_port,
                    input_port_count(&node.kind),
                ),
            ))
        }
        WireDrag::Detaching { from_node, .. } => {
            let position = state.node_position(from_node)?;
            Some((
                output_port(node_screen_rect_at(state, position, rect), state.zoom),
                pointer,
            ))
        }
    }
}

fn zoom_around(state: &mut NodeCanvasState, rect: Rect, pointer: Pos2, scroll: f32) {
    let old_zoom = state.zoom;
    let new_zoom = (old_zoom * (scroll * 0.0015).exp()).clamp(MIN_ZOOM, MAX_ZOOM);
    if (new_zoom - old_zoom).abs() <= f32::EPSILON {
        return;
    }

    let world = state.screen_to_world(rect, pointer);
    state.zoom = new_zoom;
    state.pan = pointer - rect.min - world.to_vec2() * new_zoom;
}

fn node_screen_rect(state: &NodeCanvasState, id: NodeId, rect: Rect) -> Rect {
    state.node_position(id).map_or(Rect::NOTHING, |position| {
        node_screen_rect_at(state, position, rect)
    })
}

fn node_screen_rect_at(state: &NodeCanvasState, position: Pos2, rect: Rect) -> Rect {
    let min = state.world_to_screen(rect, position);
    Rect::from_min_size(min, Vec2::new(NODE_W, NODE_H) * state.zoom)
}

/// Number of input ports a cartridge exposes to the canvas.
fn input_port_count(kind: &str) -> usize {
    match kind {
        "Constant" | "Read" => 0,
        "Merge2" => 3,
        "Grade" => 2,
        _ => 1,
    }
}

fn node_accepts_inputs(graph: &GraphMirror, id: NodeId) -> bool {
    graph
        .node(id)
        .is_some_and(|node| input_port_count(&node.kind) > 0)
}

/// Position of input `port` on the top edge of `rect`.
fn input_port(rect: Rect, port: PortId, count: usize) -> Pos2 {
    if count == 0 {
        return rect.center_top();
    }
    let slot = (port.0 as usize).min(count - 1);
    let step = rect.width() / (count as f32 + 1.0);
    Pos2::new(rect.left() + step * (slot as f32 + 1.0), rect.top())
}

/// Anchor for an input port label, clear of the port stem.
///
/// The stem rises [`PORT_LEAD`] pixels above the node's top edge, so the label
/// sits `3 * zoom` pixels above its tip. `Align2::CENTER_BOTTOM` then keeps
/// every glyph above the stem at any zoom level.
fn port_label_position(position: Pos2, zoom: f32) -> Pos2 {
    position - Vec2::new(0.0, (PORT_LEAD + 3.0) * zoom)
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
    for node in graph.nodes().iter().rev() {
        let node_rect = node_screen_rect(state, node.id, rect);
        let count = input_port_count(&node.kind);
        for port in 0..count {
            let position = input_port(node_rect, PortId(port as u16), count);
            if pointer.distance(position) <= radius {
                return Some((node.id, PortId(port as u16)));
            }
        }
    }
    None
}

fn hit_output_port(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    pointer: Pos2,
    rect: Rect,
) -> Option<NodeId> {
    let radius = port_hit_radius(state.zoom);
    graph
        .nodes()
        .iter()
        .rev()
        .find(|node| {
            let node_rect = node_screen_rect(state, node.id, rect);
            pointer.distance(output_port(node_rect, state.zoom)) <= radius
        })
        .map(|node| node.id)
}

/// Human label for an input port, keyed to the node's input semantics.
///
/// Merge-style nodes keep Nuke's `B`/`A`/`M` order. Two-input filters expose
/// port 0 as the main `in` pipe and port 1 as the optional mask, so a mask can
/// never be mistaken for the A pipe. Single-input nodes stay unlabeled.
pub fn input_port_label(kind: &str, input_count: usize, port_index: usize) -> &'static str {
    if kind == "Merge2" || input_count == 3 {
        return match port_index {
            0 => "B",
            1 => "A",
            2 => "M",
            _ => "",
        };
    }
    match input_count {
        2 => match port_index {
            0 => "in",
            1 => "M",
            _ => "",
        },
        _ => "",
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

#[allow(clippy::too_many_arguments)]
fn draw_node(
    painter: &Painter,
    rect: Rect,
    node: &CanvasNode,
    selected: bool,
    is_viewer: bool,
    zoom: f32,
    active_port: Option<PortId>,
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

    let output = output_port(rect, zoom);
    let stub = Stroke::new((1.5 * zoom).max(0.6), border);

    // Input ports: B (0), A (1), mask (2) laid out along the top edge.
    let count = input_port_count(&node.kind);
    let box_size = (9.0 * zoom).max(5.0);
    for port in 0..count {
        let port_id = PortId(port as u16);
        let position = input_port(rect, port_id, count);
        painter.line_segment(
            [position - Vec2::new(0.0, PORT_LEAD * zoom), position],
            stub,
        );
        let active = active_port == Some(port_id);
        let port_rect = Rect::from_center_size(position, Vec2::splat(box_size));
        painter.rect_filled(
            port_rect,
            CornerRadius::same(1),
            if active { WIRE_ACTIVE } else { border },
        );
        let label = input_port_label(&node.kind, count, port);
        if !label.is_empty() {
            painter.text(
                port_label_position(position, zoom),
                Align2::CENTER_BOTTOM,
                label,
                FontId::proportional((9.0 * zoom).clamp(7.0, 11.0)),
                Color32::from_gray(200),
            );
        }
    }

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
        rect.left_bottom() + Vec2::new(8.0 * zoom, -6.0 * zoom),
        Align2::LEFT_BOTTOM,
        format!("#{}", node.id.0),
        body_font,
        Color32::from_gray(190),
    );

    if crashed {
        painter.text(
            rect.right_top() + Vec2::new(-7.0 * zoom, 5.0 * zoom),
            Align2::RIGHT_TOP,
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
            rect.right_bottom() + Vec2::new(-58.0 * zoom, -16.0 * zoom),
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

    fn add_node(id: u64, x: f32, y: f32) -> UndoableCommand {
        UndoableCommand::AddNode {
            id: NodeId(id),
            kind: "Dummy".to_string(),
            x,
            y,
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
        graph.apply_event(&KernelEvent::Applied(add_node(1, 10.0, 20.0)));
        graph.apply_event(&KernelEvent::Applied(add_node(2, 10.0, 120.0)));
        graph.apply_event(&KernelEvent::Applied(connect(1, 2)));

        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(graph.next_node_id(), NodeId(3));

        graph.apply_event(&KernelEvent::Undone(connect(1, 2)));
        assert_eq!(graph.edge_count(), 0);

        graph.apply_event(&KernelEvent::Redone(connect(1, 2)));
        assert_eq!(graph.edge_count(), 1);

        graph.apply_event(&KernelEvent::Undone(add_node(2, 10.0, 120.0)));
        assert_eq!(graph.node_count(), 1);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn mirror_restores_full_state_when_a_removal_is_undone() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 10.0, 20.0)));
        graph.apply_event(&KernelEvent::Applied(add_node(2, 40.0, 160.0)));
        graph.apply_event(&KernelEvent::Applied(connect(1, 2)));

        let command = graph.removal_command(NodeId(2)).expect("snapshot");
        match &command {
            UndoableCommand::RemoveNode { knobs, edges, .. } => {
                assert_eq!(knobs.len(), 0);
                assert_eq!(edges.len(), 1);
            }
            other => panic!("expected RemoveNode, got {other:?}"),
        }

        graph.apply_event(&KernelEvent::Applied(command.clone()));
        assert_eq!(graph.node_count(), 1);
        assert_eq!(graph.edge_count(), 0);

        graph.apply_event(&KernelEvent::Undone(command));
        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(
            graph.node(NodeId(2)).map(|node| (node.x, node.y)),
            Some((40.0, 160.0))
        );
    }

    #[test]
    fn crash_events_mark_the_node() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 0.0, 0.0)));
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
    fn layout_uses_add_node_coordinates() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 32.0, 48.0)));
        graph.apply_event(&KernelEvent::Applied(add_node(2, 32.0, 172.0)));

        let mut state = NodeCanvasState::default();
        state.layout_nodes(&graph);

        assert_eq!(state.node_position(NodeId(1)), Some(Pos2::new(32.0, 48.0)));
        assert_eq!(state.node_position(NodeId(2)), Some(Pos2::new(32.0, 172.0)));
    }

    #[test]
    fn screen_and_world_round_trip_through_pan_and_zoom() {
        let state = NodeCanvasState {
            pan: Vec2::new(120.0, -30.0),
            zoom: 1.75,
            ..Default::default()
        };
        let viewport = Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(800.0, 600.0));
        let world = Pos2::new(42.0, -17.0);

        let screen = state.world_to_screen(viewport, world);
        let round_trip = state.screen_to_world(viewport, screen);

        assert!((round_trip.x - world.x).abs() < 1e-4);
        assert!((round_trip.y - world.y).abs() < 1e-4);
    }

    #[test]
    fn box_selection_maps_through_pan_and_zoom() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 0.0, 0.0)));
        graph.apply_event(&KernelEvent::Applied(add_node(2, 400.0, 400.0)));

        let mut state = NodeCanvasState {
            pan: Vec2::new(120.0, -30.0),
            zoom: 1.75,
            ..Default::default()
        };
        state.layout_nodes(&graph);
        let viewport = Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(1200.0, 900.0));

        // A box around the first node's world rect, expressed in screen space.
        let node_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(NODE_W, NODE_H));
        let start = state.world_to_screen(viewport, node_rect.min - Vec2::new(10.0, 10.0));
        let end = state.world_to_screen(viewport, node_rect.max + Vec2::new(10.0, 10.0));
        assert_eq!(
            state.nodes_in_screen_box(&graph, viewport, start, end),
            vec![NodeId(1)]
        );

        // A box covering both nodes.
        let start = state.world_to_screen(viewport, Pos2::new(-20.0, -20.0));
        let end = state.world_to_screen(viewport, Pos2::new(600.0, 600.0));
        assert_eq!(
            state.nodes_in_screen_box(&graph, viewport, start, end),
            vec![NodeId(1), NodeId(2)]
        );
    }

    #[test]
    fn shift_click_toggles_and_keeps_primary_deterministic() {
        let mut state = NodeCanvasState::default();
        apply_node_click(&mut state, NodeId(3), false);
        apply_node_click(&mut state, NodeId(5), true);

        assert_eq!(state.primary_selected, Some(NodeId(5)));
        assert_eq!(state.selected_nodes.len(), 2);

        apply_node_click(&mut state, NodeId(5), true);
        assert_eq!(
            state.primary_selected,
            Some(NodeId(3)),
            "removing the primary falls back to the lowest remaining id"
        );
        assert_eq!(state.selected_nodes.len(), 1);

        // A plain click inside the multi-selection keeps it for group drags.
        apply_node_click(&mut state, NodeId(3), false);
        assert_eq!(state.selected_nodes.len(), 1);
        apply_node_click(&mut state, NodeId(5), true);
        apply_node_click(&mut state, NodeId(5), false);
        assert_eq!(state.primary_selected, Some(NodeId(5)));
        assert_eq!(state.selected_nodes.len(), 2);
    }

    #[test]
    fn keyboard_guard_blocks_shortcuts_while_a_field_is_focused() {
        egui::__run_test_ui(|ui| {
            assert!(
                !keyboard_is_busy(ui),
                "an idle canvas must let Delete and 1 through"
            );

            // Simulates a knob field holding the keyboard.
            ui.memory_mut(|memory| memory.request_focus(egui::Id::unique(module_path!())));
            assert!(
                keyboard_is_busy(ui),
                "typing in a knob must never delete nodes"
            );

            // Clicking the canvas ends the edit, so shortcuts work again.
            ui.memory_mut(|memory| memory.stop_text_input());
            assert!(!keyboard_is_busy(ui));
        });
    }

    #[test]
    fn escape_cancels_a_detach_without_side_effects() {
        let mut state = NodeCanvasState {
            active_wire_drag: Some(WireDrag::Detaching {
                from_node: NodeId(1),
                from_port: PortId(0),
                orig_to_node: NodeId(2),
                orig_to_port: PortId(0),
            }),
            ..Default::default()
        };

        let messages = cancel_wire_drag(&mut state);

        assert!(messages.is_empty(), "cancelling must emit zero messages");
        assert!(state.active_wire_drag.is_none());
    }

    #[test]
    fn detaching_dropped_on_empty_space_disconnects_once() {
        let drag = WireDrag::Detaching {
            from_node: NodeId(1),
            from_port: PortId(0),
            orig_to_node: NodeId(2),
            orig_to_port: PortId(0),
        };

        assert_eq!(
            resolve_wire_release(drag, None, None),
            Some(UndoableCommand::DisconnectEdge {
                from_node: NodeId(1),
                from_port: PortId(0),
                to_node: NodeId(2),
                to_port: PortId(0),
            })
        );
    }

    #[test]
    fn detaching_dropped_on_a_new_input_moves_the_wire() {
        let drag = WireDrag::Detaching {
            from_node: NodeId(1),
            from_port: PortId(0),
            orig_to_node: NodeId(2),
            orig_to_port: PortId(0),
        };

        assert_eq!(
            resolve_wire_release(drag, Some((NodeId(3), PortId(1))), Some(NodeId(1))),
            Some(UndoableCommand::Batch(vec![
                UndoableCommand::DisconnectEdge {
                    from_node: NodeId(1),
                    from_port: PortId(0),
                    to_node: NodeId(2),
                    to_port: PortId(0),
                },
                UndoableCommand::ConnectEdge {
                    from_node: NodeId(1),
                    from_port: PortId(0),
                    to_node: NodeId(3),
                    to_port: PortId(1),
                },
            ]))
        );
    }

    #[test]
    fn detaching_dropped_back_on_the_same_port_is_a_no_op() {
        let drag = WireDrag::Detaching {
            from_node: NodeId(1),
            from_port: PortId(0),
            orig_to_node: NodeId(2),
            orig_to_port: PortId(0),
        };

        assert!(resolve_wire_release(drag, Some((NodeId(2), PortId(0))), None).is_none());
    }

    #[test]
    fn output_and_input_drags_connect_in_the_right_direction() {
        let from_output = resolve_wire_release(
            WireDrag::FromOutput {
                from_node: NodeId(1),
                from_port: PortId(0),
            },
            Some((NodeId(2), PortId(1))),
            None,
        );
        assert_eq!(
            from_output,
            Some(UndoableCommand::ConnectEdge {
                from_node: NodeId(1),
                from_port: PortId(0),
                to_node: NodeId(2),
                to_port: PortId(1),
            })
        );

        let from_input = resolve_wire_release(
            WireDrag::FromInput {
                to_node: NodeId(2),
                to_port: PortId(1),
            },
            None,
            Some(NodeId(1)),
        );
        assert_eq!(
            from_input,
            Some(UndoableCommand::ConnectEdge {
                from_node: NodeId(1),
                from_port: PortId(0),
                to_node: NodeId(2),
                to_port: PortId(1),
            })
        );

        // Self-connections are rejected in both directions.
        assert!(
            resolve_wire_release(
                WireDrag::FromOutput {
                    from_node: NodeId(2),
                    from_port: PortId(0),
                },
                Some((NodeId(2), PortId(0))),
                None,
            )
            .is_none()
        );
        assert!(
            resolve_wire_release(
                WireDrag::FromInput {
                    to_node: NodeId(2),
                    to_port: PortId(0),
                },
                None,
                Some(NodeId(2)),
            )
            .is_none()
        );
    }

    #[test]
    fn input_ports_spread_across_the_node_top() {
        let rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(120.0, 72.0));

        assert_eq!(input_port(rect, PortId(0), 1), Pos2::new(60.0, 0.0));
        assert_eq!(input_port(rect, PortId(0), 2), Pos2::new(40.0, 0.0));
        assert_eq!(input_port(rect, PortId(1), 2), Pos2::new(80.0, 0.0));
        assert_eq!(input_port(rect, PortId(2), 3), Pos2::new(90.0, 0.0));
    }

    #[test]
    fn port_counts_match_cartridge_inputs() {
        assert_eq!(input_port_count("Constant"), 0);
        assert_eq!(input_port_count("Read"), 0);
        assert_eq!(input_port_count("Merge2"), 3);
        assert_eq!(input_port_count("Grade"), 2);
        assert_eq!(input_port_count("Viewer"), 1);
    }

    #[test]
    fn switching_to_nuke_clears_a_non_viewer_binding() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 0.0, 0.0)));

        let message = nuke_workflow_binding_message(&graph, Some(NodeId(1)));
        assert!(
            matches!(message, KernelMessage::SetViewerNode(None)),
            "a non-Viewer must never stay bound in Nuke mode, got {message:?}"
        );
    }

    #[test]
    fn switching_to_nuke_binds_an_existing_viewer() {
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 0.0, 0.0)));
        graph.apply_event(&KernelEvent::Applied(UndoableCommand::AddNode {
            id: NodeId(2),
            kind: "Viewer".to_string(),
            x: 0.0,
            y: 120.0,
        }));

        let message = nuke_workflow_binding_message(&graph, Some(NodeId(1)));
        assert!(
            matches!(message, KernelMessage::SetViewerNode(Some(NodeId(2)))),
            "an existing Viewer takes over, got {message:?}"
        );

        let message = nuke_workflow_binding_message(&graph, Some(NodeId(2)));
        assert!(
            matches!(message, KernelMessage::SetViewerNode(Some(NodeId(2)))),
            "an already-correct binding is left alone, got {message:?}"
        );
    }

    #[test]
    fn user_spawn_list_hides_dummy_and_gates_viewer() {
        let nuke = spawnable_kinds(ViewerWorkflow::Nuke);
        assert!(!nuke.contains(&"Dummy"), "Dummy is a test-only cartridge");
        assert!(nuke.contains(&"Viewer"));

        let fusion = spawnable_kinds(ViewerWorkflow::Fusion);
        assert!(!fusion.contains(&"Dummy"));
        assert!(
            !fusion.contains(&"Viewer"),
            "Fusion has no physical Viewer node"
        );
    }

    #[test]
    fn tab_search_matches_grade_and_spawns_at_the_anchor() {
        assert_eq!(
            matching_kinds(ViewerWorkflow::Nuke, "gr"),
            vec!["Grade"],
            "typing `gr` must resolve to Grade"
        );

        let state = NodeCanvasState {
            pan: Vec2::new(40.0, 20.0),
            zoom: 1.5,
            ..Default::default()
        };
        let viewport = Rect::from_min_max(Pos2::new(10.0, 30.0), Pos2::new(900.0, 700.0));
        let anchor = Pos2::new(310.0, 330.0);
        let world = state.screen_to_world(viewport, anchor);

        let kind = matching_kinds(ViewerWorkflow::Nuke, "gr")
            .first()
            .copied()
            .expect("a match");
        match add_node_message(&state, kind, world) {
            KernelMessage::Execute(UndoableCommand::AddNode { kind, x, y, id, .. }) => {
                assert_eq!(kind, "Grade");
                assert_eq!(id, NodeId(1));
                assert!((x - world.x).abs() < 1e-4);
                assert!((y - world.y).abs() < 1e-4);
            }
            other => panic!("expected an AddNode command, got {other:?}"),
        }
    }

    #[test]
    fn search_open_flag_tracks_the_popup() {
        let mut state = NodeCanvasState::default();
        assert!(!state.is_search_open());

        state.tab_search = Some(TabSearchState {
            screen_pos: Pos2::ZERO,
            world_pos: Pos2::ZERO,
            query: String::new(),
            just_opened: true,
        });
        assert!(state.is_search_open());
    }

    #[test]
    fn port_labels_distinguish_masks_from_merge_pipes() {
        assert_eq!(input_port_label("Merge2", 3, 0), "B");
        assert_eq!(input_port_label("Merge2", 3, 1), "A");
        assert_eq!(input_port_label("Merge2", 3, 2), "M");

        assert_eq!(input_port_label("Grade", 2, 0), "in");
        assert_eq!(input_port_label("Grade", 2, 1), "M");
        assert_ne!(input_port_label("Grade", 2, 1), "A");

        assert_eq!(input_port_label("Blur", 1, 0), "");
    }

    #[test]
    fn port_labels_sit_above_the_stem_tip() {
        let port = Pos2::new(120.0, 80.0);
        for zoom in [0.25, 1.0, 2.5] {
            let label = port_label_position(port, zoom);
            let stem_tip = port.y - PORT_LEAD * zoom;
            assert_eq!(label.x, port.x, "labels stay centred on the port");
            assert!(
                stem_tip - label.y >= 3.0 * zoom - 1e-4,
                "zoom {zoom}: label bottom {} must clear stem tip {stem_tip}",
                label.y
            );
        }
    }

    #[test]
    fn physical_tab_opens_search_and_enter_spawns_grade() {
        let ctx = egui::Context::default();
        let graph = GraphMirror::default();
        let mut canvas = NodeCanvasState::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));

        // Frame 1: a toolbar button owns keyboard focus, as it would after a
        // click.
        let mut focused_after_frame_one = false;
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        ui.button("toolbar").request_focus();
                    });
                });
                focused_after_frame_one = ctx.memory(|memory| memory.focused().is_some());
            },
        )
        .drop_without_applying_deltas();
        assert!(
            focused_after_frame_one,
            "frame 1 should leave a button focused"
        );

        // Frame 2: a raw physical Tab event must open the quick search even
        // though a non-text button is focused.
        let mut messages = Vec::new();
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key: Key::Tab,
                    physical_key: Some(Key::Tab),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        messages =
                            NodeCanvas::show(ui, &mut canvas, &graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(
            canvas.is_search_open(),
            "physical Tab must open the quick-search popup"
        );
        assert!(messages.is_empty());

        // Frame 3: typing `gr` and pressing Enter spawns Grade and closes.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::Text("gr".to_string()),
                    egui::Event::Key {
                        key: Key::Enter,
                        physical_key: Some(Key::Enter),
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        messages =
                            NodeCanvas::show(ui, &mut canvas, &graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(!canvas.is_search_open(), "Enter closes the popup");
        match messages.first() {
            Some(KernelMessage::Execute(UndoableCommand::AddNode { kind, .. })) => {
                assert_eq!(kind, "Grade");
            }
            other => panic!("expected an AddNode for Grade, got {other:?}"),
        }
    }

    /// 3-frame test: dragging on empty canvas background starts a box selection,
    /// not a node drag. On release, nodes inside the selection box are selected.
    #[test]
    fn empty_canvas_drag_performs_box_selection() {
        let ctx = egui::Context::default();
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 500.0, 500.0)));
        let mut canvas = NodeCanvasState::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));

        let pointer_pos_1 = Pos2::new(38.0, 70.0);
        let pointer_pos_2 = Pos2::new(508.0, 440.0);

        // Frame 1: pointer moves to empty canvas coordinate (30, 30 world)
        // and presses primary.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::PointerMoved(pointer_pos_1),
                    egui::Event::PointerButton {
                        pos: pointer_pos_1,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let _ =
                            NodeCanvas::show(ui, &mut canvas, &graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(
            canvas.box_selection.is_some(),
            "box selection should start on empty canvas drag"
        );
        assert!(!canvas.dragging_nodes);

        // Frame 2: pointer drags to world (500, 400) while holding primary.
        // Screen pos = rect.min + (500, 400) = (508, 440).
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::PointerMoved(pointer_pos_2),
                    egui::Event::PointerButton {
                        pos: pointer_pos_2,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let _ =
                            NodeCanvas::show(ui, &mut canvas, &graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(
            canvas.box_selection.is_some(),
            "box selection should still be active during drag"
        );
        assert!(!canvas.dragging_nodes);

        // Frame 3: pointer releases primary.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::PointerButton {
                    pos: pointer_pos_2,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let _ =
                            NodeCanvas::show(ui, &mut canvas, &graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(
            canvas.box_selection.is_none(),
            "box selection should be cleared on release"
        );
        assert!(!canvas.dragging_nodes);
        // No nodes are inside the selection box (node is at world (500,500)
        // which is outside the selection box from world (30,30) to (500,400)).
        assert!(canvas.selected_nodes.is_empty());
    }

    /// 3-frame test: dragging on a node body moves the node, never starts box
    /// selection.
    #[test]
    fn node_body_drag_moves_node_without_box_selection() {
        let ctx = egui::Context::default();
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 100.0, 100.0)));
        assert!(
            graph.node(NodeId(1)).is_some(),
            "NodeId(1) should be in graph"
        );
        let mut canvas = NodeCanvasState::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));

        // Lay the canvas out once so the node's screen position is known.
        let _ = run_canvas_pass(&ctx, screen, vec![], &mut canvas, &graph);
        let rect = canvas
            .last_canvas_rect
            .expect("the canvas must be laid out");
        // Frame 1: pointer inside NodeId(1)'s body and presses.
        let pointer_pos_1 = canvas.world_to_screen(rect, Pos2::new(120.0, 120.0));
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::PointerMoved(pointer_pos_1),
                    egui::Event::PointerButton {
                        pos: pointer_pos_1,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let _ =
                            NodeCanvas::show(ui, &mut canvas, &graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(
            canvas.dragging_nodes,
            "node drag should be latched on press"
        );
        assert!(
            canvas.box_selection.is_none(),
            "no box selection on node press"
        );

        // Frame 2: pointer drags by (50, 20) while holding primary. Only the
        // move is reported; re-sending the press would re-latch the drag.
        let pointer_pos_2 = pointer_pos_1 + Vec2::new(50.0, 20.0);
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerMoved(pointer_pos_2)],
            &mut canvas,
            &graph,
        );
        assert!(
            canvas.box_selection.is_none(),
            "box selection should not start on node drag"
        );
        assert!(canvas.dragging_nodes, "node drag should be latched");

        // Check that a MoveNode command was emitted for NodeId(1).
        let move_msgs: Vec<_> = messages
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    KernelMessage::Execute(UndoableCommand::MoveNode { id, .. }) if *id == NodeId(1)
                )
            })
            .collect();
        assert!(
            !move_msgs.is_empty(),
            "MoveNode command should be emitted for node drag"
        );

        // Frame 3: pointer releases primary.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::PointerButton {
                    pos: pointer_pos_2,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let _ =
                            NodeCanvas::show(ui, &mut canvas, &graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(!canvas.dragging_nodes, "drag should end on release");
    }

    /// Pre-select two nodes, drag on one body, and assert a single Batch MoveNode
    /// containing both nodes is emitted, with no box selection.
    #[test]
    fn group_node_drag_moves_all_selected_nodes_in_batch() {
        let ctx = egui::Context::default();
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 100.0, 100.0)));
        graph.apply_event(&KernelEvent::Applied(add_node(2, 300.0, 300.0)));
        let mut canvas = NodeCanvasState::default();
        // Pre-select both nodes.
        canvas.selected_nodes.insert(NodeId(1));
        canvas.selected_nodes.insert(NodeId(2));
        canvas.primary_selected = Some(NodeId(1));
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));

        // Lay the canvas out once so the node's screen position is known.
        let _ = run_canvas_pass(&ctx, screen, vec![], &mut canvas, &graph);
        let rect = canvas
            .last_canvas_rect
            .expect("the canvas must be laid out");
        // Frame 1: pointer inside NodeId(1)'s body and presses.
        let pointer_pos_1 = canvas.world_to_screen(rect, Pos2::new(120.0, 120.0));
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::PointerMoved(pointer_pos_1),
                    egui::Event::PointerButton {
                        pos: pointer_pos_1,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let _ =
                            NodeCanvas::show(ui, &mut canvas, &graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(canvas.dragging_nodes, "group drag should be latched");

        // Frame 2: pointer drags by (50, 20) while holding primary. Only the
        // move is reported; re-sending the press would re-latch the drag.
        let pointer_pos_2 = pointer_pos_1 + Vec2::new(50.0, 20.0);
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerMoved(pointer_pos_2)],
            &mut canvas,
            &graph,
        );
        assert!(
            canvas.box_selection.is_none(),
            "box selection should not start on group drag"
        );

        // Find the Batch MoveNode command.
        let batch_msgs: Vec<_> = messages
            .iter()
            .filter(|m| match m {
                KernelMessage::Execute(UndoableCommand::Batch(cmds)) => cmds
                    .iter()
                    .map(|c| match c {
                        UndoableCommand::MoveNode { id, .. } => *id,
                        _ => NodeId(0),
                    })
                    .eq([NodeId(1), NodeId(2)]),
                _ => false,
            })
            .collect();
        assert!(
            !batch_msgs.is_empty(),
            "Batch MoveNode for both nodes should be emitted"
        );
    }

    /// A press that starts outside the canvas rect (e.g. on a `DragValue` in
    /// `PropertiesPanel`) must not touch canvas selection state.
    ///
    /// Clicking a knob otherwise enters the empty-canvas branch, and the
    /// release clears `selected_nodes`, closing the panel on the exact frame
    /// `DragValue` switches into click-to-type mode.
    #[test]
    fn click_outside_canvas_rect_preserves_selection() {
        let ctx = egui::Context::default();
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 100.0, 100.0)));
        graph.apply_event(&KernelEvent::Applied(add_node(2, 300.0, 300.0)));
        let mut canvas = NodeCanvasState::default();
        canvas.selected_nodes.insert(NodeId(2));
        canvas.primary_selected = Some(NodeId(2));
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));

        // The canvas is laid out inside a 600x400 child Ui, so this press lands
        // in the strip to its right — where the properties panel lives in the
        // real app.
        let canvas_rect = Rect::from_min_size(Pos2::new(8.0, 8.0), Vec2::new(600.0, 400.0));
        let outside = canvas_rect.max + egui::vec2(80.0, -50.0);

        // Frame 1: primary press outside the canvas rect.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::PointerMoved(outside),
                    egui::Event::PointerButton {
                        pos: outside,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(600.0, 400.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let _ = NodeCanvas::show(
                                ui,
                                &mut canvas,
                                &graph,
                                None,
                                ViewerWorkflow::Nuke,
                            );
                        },
                    );
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(
            canvas.box_selection.is_none(),
            "a press outside the canvas must not start a box selection"
        );
        assert!(!canvas.dragging_nodes);
        assert_eq!(canvas.primary_selected, Some(NodeId(2)));

        // Frame 2: release at the same outside coordinate.
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::PointerButton {
                    pos: outside,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(600.0, 400.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let _ = NodeCanvas::show(
                                ui,
                                &mut canvas,
                                &graph,
                                None,
                                ViewerWorkflow::Nuke,
                            );
                        },
                    );
                });
            },
        )
        .drop_without_applying_deltas();

        assert_eq!(
            canvas.primary_selected,
            Some(NodeId(2)),
            "clicking outside the canvas must keep the primary selection"
        );
        assert!(
            canvas.selected_nodes.contains(&NodeId(2)),
            "clicking outside the canvas must keep the selected nodes"
        );
        assert!(canvas.box_selection.is_none());
        assert!(!canvas.dragging_nodes);
    }

    /// Runs one `NodeCanvas::show` pass and returns the messages it emitted.
    fn run_canvas_pass(
        ctx: &egui::Context,
        screen: Rect,
        events: Vec<egui::Event>,
        canvas: &mut NodeCanvasState,
        graph: &GraphMirror,
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
                        messages = NodeCanvas::show(ui, canvas, graph, None, ViewerWorkflow::Nuke);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        messages
    }

    /// Mirrors an emitted command into the graph, the DAG, and the undo stack,
    /// exactly as `TanovaApp::dispatch` plus the kernel would.
    fn feed_commands(
        messages: &[KernelMessage],
        mirror: &mut GraphMirror,
        dag: &mut tanova_kernel::Dag,
        undo: &mut tanova_kernel::UndoStack,
        emitted: &mut Vec<UndoableCommand>,
    ) {
        for message in messages {
            if let KernelMessage::Execute(command) = message {
                mirror.apply_command(command);
                let enriched = dag
                    .apply_command(command.clone())
                    .expect("the dag should accept a move");
                undo.push(enriched);
                emitted.push(command.clone());
            }
        }
    }

    /// Flattens a `MoveNode` / `Batch` command into `(id, from, to)` tuples.
    fn collect_moves(command: &UndoableCommand, out: &mut Vec<(NodeId, f32, f32, f32, f32)>) {
        match command {
            UndoableCommand::MoveNode {
                id,
                from_x,
                from_y,
                to_x,
                to_y,
            } => out.push((*id, *from_x, *from_y, *to_x, *to_y)),
            UndoableCommand::Batch(cmds) => {
                for cmd in cmds {
                    collect_moves(cmd, out);
                }
            }
            _ => {}
        }
    }

    /// Drives the real `NodeCanvas::show` drag path and proves that one `Undo`
    /// returns a single node or a selected group to the press position.
    ///
    /// The two drag legs are separated by `break_coalescing()`, so this also
    /// covers a hand pause longer than the 400 ms window: the latched drag
    /// origin must still merge the gesture into one undo step.
    #[test]
    fn canvas_single_and_group_drag_emits_true_start_origin_and_undoes_cleanly() {
        use tanova_kernel::{Dag, UndoStack};

        let ctx = egui::Context::default();
        let mut mirror = GraphMirror::default();
        mirror.apply_event(&KernelEvent::Applied(add_node(1, 100.0, 100.0)));
        mirror.apply_event(&KernelEvent::Applied(add_node(2, 300.0, 300.0)));

        let mut dag = Dag::new();
        dag.add_node_at(NodeId(1), "Dummy", 100.0, 100.0)
            .expect("dag node #1");
        dag.add_node_at(NodeId(2), "Dummy", 300.0, 300.0)
            .expect("dag node #2");
        let mut undo_stack = UndoStack::new();
        let mut emitted: Vec<UndoableCommand> = Vec::new();

        let mut canvas = NodeCanvasState::default();
        canvas.selected_nodes.insert(NodeId(1));
        canvas.selected_nodes.insert(NodeId(2));
        canvas.primary_selected = Some(NodeId(1));

        let start_1 = (100.0f32, 100.0f32);
        let start_2 = (300.0f32, 300.0f32);
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));

        // Lay the canvas out once so the node's screen position is known.
        let _ = run_canvas_pass(&ctx, screen, vec![], &mut canvas, &mirror);
        let rect = canvas
            .last_canvas_rect
            .expect("the canvas must be laid out");
        let press = canvas.world_to_screen(rect, Pos2::new(120.0, 120.0));
        let first_stop = press + Vec2::new(30.0, 15.0);
        let second_stop = press + Vec2::new(80.0, 40.0);

        // Frame 1: press on node #1's body with both nodes selected.
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![
                egui::Event::PointerMoved(press),
                egui::Event::PointerButton {
                    pos: press,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
            &mut canvas,
            &mirror,
        );
        assert!(canvas.dragging_nodes, "press must latch the drag");
        assert!(
            messages.is_empty(),
            "the press frame must not emit a move, got {messages:?}"
        );

        // Frame 2: drag by (+30, +15).
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerMoved(first_stop)],
            &mut canvas,
            &mirror,
        );
        assert!(
            !messages.is_empty(),
            "a moving frame must emit a move command"
        );
        feed_commands(
            &messages,
            &mut mirror,
            &mut dag,
            &mut undo_stack,
            &mut emitted,
        );
        // Simulate a > 400 ms hand pause while still holding the button.
        undo_stack.break_coalescing();

        // Frame 3: hold still. No command may be emitted.
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerMoved(first_stop)],
            &mut canvas,
            &mirror,
        );
        assert!(
            messages.is_empty(),
            "a stationary frame must not emit a move, got {messages:?}"
        );

        // Frame 4: drag another (+50, +25).
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerMoved(second_stop)],
            &mut canvas,
            &mirror,
        );
        feed_commands(
            &messages,
            &mut mirror,
            &mut dag,
            &mut undo_stack,
            &mut emitted,
        );

        // Frame 5: release.
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerButton {
                pos: second_stop,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
            &mut canvas,
            &mirror,
        );
        assert!(
            messages.is_empty(),
            "the release frame must not emit a move, got {messages:?}"
        );
        assert!(!canvas.dragging_nodes, "release must clear the latch");
        assert!(canvas.drag_start_positions.is_empty());

        // Every emitted move must report the true press origin.
        let mut moves = Vec::new();
        for command in &emitted {
            collect_moves(command, &mut moves);
        }
        assert!(!moves.is_empty(), "the drag must emit move commands");
        for (id, from_x, from_y, to_x, to_y) in &moves {
            let origin = match id {
                NodeId(1) => start_1,
                NodeId(2) => start_2,
                other => panic!("unexpected node in a drag command: {other:?}"),
            };
            assert!(
                (from_x - origin.0).abs() < 1e-3 && (from_y - origin.1).abs() < 1e-3,
                "{id:?} reported from ({from_x}, {from_y}) instead of its drag start {origin:?}"
            );
            assert!(
                (to_x - from_x).abs() > 1e-6 || (to_y - from_y).abs() > 1e-6,
                "{id:?} emitted a no-op move"
            );
        }

        // The mirror followed the drag to start + (80, 40).
        let dragged_1 = mirror.node(NodeId(1)).expect("node #1 in mirror");
        let dragged_2 = mirror.node(NodeId(2)).expect("node #2 in mirror");
        assert!((dragged_1.x - 180.0).abs() < 1e-3 && (dragged_1.y - 140.0).abs() < 1e-3);
        assert!((dragged_2.x - 380.0).abs() < 1e-3 && (dragged_2.y - 340.0).abs() < 1e-3);

        // One undo must restore the whole group despite the pause.
        let undone = undo_stack.undo().expect("the gesture must be one step");
        dag.undo_command(&undone)
            .expect("the dag applies the inverse");
        mirror.apply_event(&KernelEvent::Undone(undone));
        assert!(
            !undo_stack.can_undo(),
            "the whole drag must collapse into a single undo step"
        );

        for (id, (start_x, start_y)) in [(NodeId(1), start_1), (NodeId(2), start_2)] {
            let node = mirror.node(id).expect("node in mirror");
            assert!(
                (node.x - start_x).abs() < 1e-3 && (node.y - start_y).abs() < 1e-3,
                "undo should restore {id:?} to ({start_x}, {start_y}), got ({}, {})",
                node.x,
                node.y
            );
        }

        // The next canvas frame must re-lay out from the mirror so the undo is
        // visible on screen, not just in the mirror.
        let _ = run_canvas_pass(&ctx, screen, vec![], &mut canvas, &mirror);
        let laid_out = canvas
            .node_position(NodeId(1))
            .expect("node #1 must have a canvas position");
        assert!(
            (laid_out.x - start_1.0).abs() < 1e-3 && (laid_out.y - start_1.1).abs() < 1e-3,
            "canvas must follow the undo back to {start_1:?}, got {laid_out:?}"
        );
    }
}
