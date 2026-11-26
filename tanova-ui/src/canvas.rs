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
    AnimationCurve, EdgeSnapshot, KernelEvent, KernelMessage, KnobTable, KnobValue, NodeId, PortId,
    ProjectFormat, UndoableCommand, find_cartridge,
};

use crate::theme::{StudioTheme, ThemePalette, backdrop_tint_color, node_category, palette};
use crate::workflow::ViewerWorkflow;

const NODE_W: f32 = 168.0;
const NODE_H: f32 = 72.0;
/// Canvas size of a standard node card.
pub const NODE_SIZE: egui::Vec2 = egui::vec2(NODE_W, NODE_H);
/// Size of the `22x22` `Dot` routing elbow.
///
/// Slightly larger than the original `18x18` so the circular body is a
/// comfortable drag handle; its stem tips still own the wire hit-test.
pub const DOT_SIZE: egui::Vec2 = egui::vec2(22.0, 22.0);
/// Gap below a card's bottom edge when auto-chaining. Standard cards are
/// [`NODE_H`] tall, so this keeps the historical 90px vertical pitch.
const CHAIN_GAP: f32 = 18.0;
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
    "CheckerBoard2",
    "Grade",
    "ColorCorrect",
    "Clamp",
    "Saturation",
    "Invert",
    "Premult",
    "Unpremult",
    "Shuffle",
    "Transform",
    "Reformat",
    "Crop",
    "Blur",
    "Merge2",
    "Switch",
    "TimeOffset",
    "FrameHold",
    "Write",
    "Viewer",
    "Dot",
    "BackdropNode",
    "Mirror",
    "Ramp",
    "CornerPin2D",
    "Defocus",
    "Glow",
];

const WIRE_ACTIVE: Color32 = Color32::from_rgb(245, 220, 120);
/// The bold red `X` drawn across a bypassed node.
const BYPASS_X: Color32 = Color32::from_rgb(230, 95, 70);
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
    /// UI-side copy of the node's animation curves, updated from `KernelEvent`s.
    pub curves: BTreeMap<String, Vec<AnimationCurve>>,
    /// Canvas-space X position carried by the `AddNode` command.
    pub x: f32,
    /// Canvas-space Y position carried by the `AddNode` command.
    pub y: f32,
    /// `true` when the node is bypassed (`D` key) and passes its input
    /// straight through.
    pub disabled: bool,
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
    /// Per-node evaluation timing from `KernelEvent::PerfSnapshot`.
    pub node_perf: std::collections::HashMap<NodeId, (u64, bool)>,
}

impl GraphMirror {
    /// Records the active project format so newly spawned sources can inherit
    /// it instead of the cartridge's static default.
    pub fn set_project_format(&mut self, format: ProjectFormat) {
        self.project_format = Some(format);
    }

    /// Clears all nodes and edges, preserving `project_format`.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.edges.clear();
        self.node_perf.clear();
    }

    /// Rebuilds the mirror directly from a kernel `Dag`, populating nodes with
    /// their exact `id`, `kind`, `x`, `y`, and `knobs`, plus all edges, so a
    /// loaded `.nk` script immediately displays on the canvas.
    pub fn from_dag(
        dag: &tanova_kernel::Dag,
        _viewer_node: Option<NodeId>,
        project_format: Option<ProjectFormat>,
    ) -> Self {
        let nodes: Vec<CanvasNode> = dag
            .nodes()
            .map(|node| CanvasNode {
                id: node.id,
                kind: node.kind.clone(),
                crashed: None,
                knobs: node.knobs.clone(),
                curves: node.curves.clone(),
                x: node.x,
                y: node.y,
                disabled: node.disabled,
            })
            .collect();
        let edges: Vec<CanvasEdge> = dag
            .edges()
            .map(|edge| CanvasEdge {
                from_node: edge.from_node,
                from_port: edge.from_port,
                to_node: edge.to_node,
                to_port: edge.to_port,
            })
            .collect();
        Self {
            nodes,
            edges,
            project_format,
            node_perf: std::collections::HashMap::new(),
        }
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

    /// Alias for [`GraphMirror::node`] used by spawn helpers.
    pub fn find_node(&self, id: NodeId) -> Option<&CanvasNode> {
        self.node(id)
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Active project format, if any.
    pub fn project_format(&self) -> Option<ProjectFormat> {
        self.project_format
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

    /// Keyframe times (rounded, sorted, deduplicated) used by the timeline.
    ///
    /// Reads `selected`'s curves, or every node's curves when nothing is
    /// selected.
    pub fn keyframe_times(&self, selected: Option<NodeId>) -> Vec<i64> {
        let mut times = Vec::new();
        let mut collect = |node: &CanvasNode| {
            for curves in node.curves.values() {
                for curve in curves {
                    times.extend(curve.keyframe_times());
                }
            }
        };
        match selected {
            Some(id) => {
                if let Some(node) = self.node(id) {
                    collect(node);
                }
            }
            None => {
                for node in &self.nodes {
                    collect(node);
                }
            }
        }
        times.sort_unstable();
        times.dedup();
        times
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
            disabled: node.disabled,
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
            | KernelEvent::ViewerChanged(_)
            | KernelEvent::DirtyStateChanged { .. }
            | KernelEvent::ScriptSaved { .. }
            | KernelEvent::ScriptError { .. }
            | KernelEvent::WriteFrameRendered { .. }
            | KernelEvent::WriteCompleted { .. }
            | KernelEvent::MemoryStats { .. } => {}
            KernelEvent::PerfSnapshot(samples) => {
                // Merge, never wipe: a cached upstream node returns early and
                // is not sampled, so a later snapshot only covers the branches
                // it actually touched. Clearing here would erase the startup
                // bars for every untouched node.
                for sample in samples {
                    self.node_perf
                        .insert(sample.node, (sample.eval_us, sample.from_cache));
                }
            }
        }
    }

    /// Applies the effective change of one command.
    pub fn apply_command(&mut self, command: &UndoableCommand) {
        match command {
            UndoableCommand::AddNode { id, kind, x, y } => {
                if let Some(existing) = self.nodes.iter_mut().find(|node| node.id == *id) {
                    // Optimistic dispatch may apply the same `AddNode` before
                    // the kernel's `Applied` event arrives. Update the
                    // coordinates in place and preserve the initialized knobs
                    // (including the project-format stamp).
                    existing.kind = kind.clone();
                    existing.x = *x;
                    existing.y = *y;
                } else {
                    let mut knobs = find_cartridge(kind)
                        .map(|descriptor| (descriptor.default_knobs)())
                        .unwrap_or_default();
                    if let Some(format) = self.project_format {
                        tanova_kernel::Dag::stamp_project_format_defaults(kind, &mut knobs, format);
                    }
                    self.nodes.push(CanvasNode {
                        id: *id,
                        kind: kind.clone(),
                        crashed: None,
                        knobs,
                        curves: BTreeMap::new(),
                        x: *x,
                        y: *y,
                        disabled: false,
                    });
                }
            }
            UndoableCommand::RemoveNode { id, .. } => {
                self.nodes.retain(|node| node.id != *id);
                self.edges
                    .retain(|edge| edge.from_node != *id && edge.to_node != *id);
                self.node_perf.remove(id);
            }
            UndoableCommand::RestoreNode {
                id,
                kind,
                x,
                y,
                disabled,
                knobs,
                edges,
            } => {
                if !self.nodes.iter().any(|node| node.id == *id) {
                    self.nodes.push(CanvasNode {
                        id: *id,
                        kind: kind.clone(),
                        crashed: None,
                        knobs: knobs.as_ref().clone(),
                        curves: BTreeMap::new(),
                        x: *x,
                        y: *y,
                        disabled: *disabled,
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
                // One input port can only carry one wire. Replacing the old
                // edge (rather than appending) keeps optimistic dispatch and
                // the kernel's `Applied` event idempotent.
                self.edges
                    .retain(|edge| !(edge.to_node == *to_node && edge.to_port == *to_port));
                self.edges.push(CanvasEdge {
                    from_node: *from_node,
                    from_port: *from_port,
                    to_node: *to_node,
                    to_port: *to_port,
                });
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
            UndoableCommand::SetKnobCurve {
                node, name, curves, ..
            } => {
                if let Some(node) = self
                    .nodes
                    .iter_mut()
                    .find(|candidate| candidate.id == *node)
                {
                    if curves.is_empty() {
                        node.curves.remove(name);
                    } else {
                        node.curves.insert(name.clone(), curves.clone());
                    }
                }
            }
            UndoableCommand::SetDisabled { node, disabled } => {
                if let Some(node) = self
                    .nodes
                    .iter_mut()
                    .find(|candidate| candidate.id == *node)
                {
                    node.disabled = *disabled;
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
    ToggleBypass(NodeId),
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
    /// Selection snapshot captured the moment Tab opens, so spawning from the
    /// popup wraps the box-selected nodes instead of an empty set (the canvas
    /// pointer handler runs before the popup is drawn and can otherwise wipe
    /// `selected_nodes`).
    pub selected_snapshot: HashSet<NodeId>,
    /// Primary selection snapshot captured the moment Tab opens.
    pub primary_snapshot: Option<NodeId>,
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
    /// The two bookmarked viewer inputs for Nuke-style A/B comparison.
    ///
    /// Slot 0 is bound by `1`, slot 1 by `2`. Each slot stores the *routed
    /// target* (the node the physical Viewer looks through), never the Viewer
    /// node itself, so switching slots only re-routes the viewer.
    pub viewer_slots: [Option<NodeId>; 2],
    /// Which viewer slot the viewer panel is currently showing (`0` or `1`).
    pub active_viewer_slot: usize,
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
    /// Active corner-resize gesture on a `BackdropNode`.
    ///
    /// Stores `(backdrop_id, initial_bdwidth, initial_bdheight)` so release can
    /// emit one `Batch` of two `SetKnob`s whose `previous` values are the size
    /// the artist started the drag from, making `Ctrl+Z` undo the resize in a
    /// single step.
    pub resizing_backdrop: Option<(NodeId, f32, f32)>,
    /// Live `(backdrop_id, width, height)` preview while a corner-resize drag is
    /// in flight. Committed to the kernel as one `Batch` on release.
    resize_preview: Option<(NodeId, f32, f32)>,
    /// Active studio theme for card colors, canvas background, pins, and wires.
    pub theme: StudioTheme,
    /// Whether to draw the Node Graph Minimap inset in the bottom-right corner.
    pub show_minimap: bool,
    /// True while the artist is dragging inside the minimap to pan the canvas.
    dragging_minimap: bool,
    /// Set when `[Fit]` is clicked; applied after `canvas_rect` is allocated.
    pub fit_requested: bool,
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
            viewer_slots: [None, None],
            active_viewer_slot: 0,
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
            resizing_backdrop: None,
            resize_preview: None,
            theme: StudioTheme::Classic,
            show_minimap: false,
            dragging_minimap: false,
            fit_requested: false,
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

    /// Resets all transient canvas state for a new or loaded script, clearing
    /// selection, drags, box selection, search, pan/zoom, and rebasing the
    /// node-id allocator on the mirror's highest id.
    pub fn reset_for_script(&mut self, mirror: &GraphMirror) {
        self.selected_nodes.clear();
        self.primary_selected = None;
        self.viewer_slots = [None, None];
        self.active_viewer_slot = 0;
        self.active_wire_drag = None;
        self.box_selection = None;
        self.dragging_nodes = false;
        self.resizing_backdrop = None;
        self.resize_preview = None;
        self.tab_search = None;
        self.pan = Vec2::ZERO;
        self.zoom = 1.0;
        self.drag_start_positions.clear();
        self.box_dragging = false;
        self.panning = false;
        self.context_menu_pos = None;
        self.context_menu_open = false;
        self.node_menu_pos = None;
        self.node_menu_node = None;
        self.node_menu_open = false;
        self.next_node_id = mirror
            .nodes()
            .iter()
            .map(|node| node.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
    }

    /// `true` while the `Tab` quick-search popup owns the keyboard.
    pub fn is_search_open(&self) -> bool {
        self.tab_search.is_some()
    }

    /// Raises the node-id high-water mark so externally allocated ids can never
    /// collide with the ids the canvas will hand out next.
    pub fn sync_next_node_id(&mut self, floor: u64) {
        self.next_node_id = self.next_node_id.max(floor);
    }

    /// Resolves viewer-slot key `slot_idx` (`0` for `1`, `1` for `2`) into the
    /// node the viewer should route to.
    ///
    /// The A/B guard keeps a selected node's knobs open while flipping between
    /// two comparisons: if the selected node is already bookmarked in the
    /// *other* slot and that slot is active, pressing this slot's key re-shows
    /// the target already stored here instead of overwriting it with the
    /// selection.
    pub fn select_viewer_slot(&mut self, slot_idx: usize) -> Option<NodeId> {
        let slot_idx = slot_idx.min(1);
        let other_slot = 1 - slot_idx;

        if let Some(sel_id) = self.primary_selected {
            if self.viewer_slots[other_slot] == Some(sel_id)
                && self.active_viewer_slot == other_slot
            {
                if let Some(existing_target) = self.viewer_slots[slot_idx] {
                    self.active_viewer_slot = slot_idx;
                    return Some(existing_target);
                }
            }
            self.viewer_slots[slot_idx] = Some(sel_id);
            self.active_viewer_slot = slot_idx;
            Some(sel_id)
        } else if let Some(bookmarked_id) = self.viewer_slots[slot_idx] {
            self.active_viewer_slot = slot_idx;
            Some(bookmarked_id)
        } else {
            None
        }
    }

    /// Hands out the next never-reused node id.
    ///
    /// Used by sub-graph paste, which allocates ids on the UI thread so it can
    /// build one atomic `Batch` of `AddNode`/`SetKnob`/`ConnectEdge` commands.
    pub fn allocate_node_id(&mut self) -> NodeId {
        let id = NodeId(self.next_node_id);
        self.next_node_id = self.next_node_id.saturating_add(1);
        id
    }

    /// Frames the selection (or every node when nothing is selected) inside
    /// `viewport_rect`, Nuke's `F` key.
    ///
    /// Zoom is clamped to `0.25..=1.5` so a two-node selection never zooms to
    /// a comical 800% and a huge graph stays legible.
    pub fn frame_nodes(&mut self, mirror: &GraphMirror, viewport_rect: Rect) {
        let mut targets: Vec<NodeId> = if self.selected_nodes.is_empty() {
            mirror.nodes().iter().map(|node| node.id).collect()
        } else {
            mirror
                .nodes()
                .iter()
                .filter(|node| self.selected_nodes.contains(&node.id))
                .map(|node| node.id)
                .collect()
        };
        targets.sort();

        let mut bounds: Option<Rect> = None;
        for id in targets {
            let Some(position) = self
                .positions
                .get(&id)
                .copied()
                .or_else(|| mirror.node(id).map(|node| Pos2::new(node.x, node.y)))
            else {
                continue;
            };
            let size = mirror
                .node(id)
                .map_or(Vec2::new(NODE_W, NODE_H), node_world_size);
            let card = Rect::from_min_size(position, size);
            bounds = Some(bounds.map_or(card, |current| current.union(card)));
        }

        let Some(bounds) = bounds else {
            self.pan = Vec2::ZERO;
            self.zoom = 1.0;
            return;
        };

        const MARGIN: f32 = 40.0;
        let available = Vec2::new(
            (viewport_rect.width() - MARGIN * 2.0).max(1.0),
            (viewport_rect.height() - MARGIN * 2.0).max(1.0),
        );
        let fit = (available.x / bounds.width().max(1.0))
            .min(available.y / bounds.height().max(1.0))
            .clamp(0.25, 1.5);
        self.zoom = fit;

        // Solve `world_to_screen(bounds.center()) == viewport.center()`.
        self.pan = viewport_rect.center() - viewport_rect.min - bounds.center().to_vec2() * fit;
    }

    /// Canvas-space position of a node.
    pub fn node_position(&self, id: NodeId) -> Option<Pos2> {
        self.positions.get(&id).copied()
    }

    /// Pan the canvas so that `world_pos` lands at the center of `canvas_rect`,
    /// at the current zoom level. Derived directly from `world_to_screen` so
    /// that `world_to_screen(world_pos, canvas_rect, self)` equals
    /// `canvas_rect.center()` within `1e-3` px at any zoom. Guards against
    /// non-finite values.
    pub fn pan_to_center_world(&mut self, world_pos: Pos2, canvas_rect: Rect) {
        if !world_pos.is_finite() || !canvas_rect.is_finite() || self.zoom <= 0.0 {
            return;
        }
        // world_to_screen: screen = canvas_min + pan + world * zoom
        // We want screen == canvas_rect.center(), so:
        // pan = canvas_rect.center() - canvas_rect.min - world_pos * zoom
        self.pan = canvas_rect.center() - canvas_rect.min - world_pos.to_vec2() * self.zoom;
        // Guard against drift into non-finite territory.
        if !self.pan.is_finite() {
            self.pan = Vec2::ZERO;
        }
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
                    Rect::from_min_size(*position, node_world_size(node)).intersects(selection)
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
    ///
    /// When `modal_open` is `true`, canvas keyboard shortcuts (`1`, `Delete`,
    /// `Backspace`) are suppressed so a file modal or unsaved-changes dialog
    /// owns the keyboard.
    pub fn show(
        ui: &mut Ui,
        state: &mut NodeCanvasState,
        graph: &GraphMirror,
        viewer: Option<NodeId>,
        workflow: ViewerWorkflow,
        modal_open: bool,
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
                    ui.separator();
                    let map_response = ui.selectable_label(state.show_minimap, "[Map]");
                    if map_response.clicked() {
                        state.show_minimap = !state.show_minimap;
                    }
                    ui.separator();
                    if ui.button("[Fit]").clicked() {
                        state.fit_requested = true;
                    }
                });
            ui.separator();
            if ui.button("View (1)").clicked() {
                if let Some(selected) = state.primary_selected {
                    messages.push(viewer_message(graph, workflow, selected));
                }
            }
            if let Some(viewer) = viewer {
                let slot = if state.active_viewer_slot == 0 { 1 } else { 2 };
                ui.colored_label(VIEWER, format!("VIEWER {slot}: #{}", viewer.0));
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

        // `[Fit]` was clicked in the header: frame all nodes inside the
        // newly allocated canvas rect (same logic as pressing `F`).
        if state.fit_requested {
            state.frame_nodes(graph, rect);
            state.fit_requested = false;
        }
        // Deliberately not `Sense::FOCUSABLE`: the canvas must never hold the
        // keyboard, otherwise `memory.focused()` would stay set and every
        // Delete/`1` press would look like the artist is typing in a field.
        let response = ui.allocate_rect(rect, Sense::CLICK | Sense::DRAG);
        let mut painter = ui.painter_at(rect);

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

        // Minimap input priority: compute the minimap rect here (same geometry
        // as the drawing pass) so a click/drag inside it is handled before
        // canvas panning, node hit-testing, or box selection.
        let mut minimap_rect: Option<Rect> = None;
        if state.show_minimap && !graph.nodes().is_empty() {
            let minimap_size = Vec2::new(170.0, 110.0);
            let margin = 8.0;
            minimap_rect = Some(Rect::from_min_size(
                Pos2::new(
                    rect.max.x - minimap_size.x - margin,
                    rect.max.y - minimap_size.y - margin,
                ),
                minimap_size,
            ));
        }
        let pointer_over_minimap = minimap_rect
            .as_ref()
            .is_some_and(|r| pointer_pos.map(|p| r.contains(p)).unwrap_or(false));

        // Track minimap drag state: latched on press inside the minimap,
        // released when the primary button is no longer down.
        if primary_pressed && pointer_over_minimap {
            state.dragging_minimap = true;
        }
        if !primary_down {
            state.dragging_minimap = false;
        }

        // While the minimap is being dragged (or hovered/clicked), suppress
        // ALL canvas interactions: node hit-testing, node dragging, wire
        // dragging, canvas panning, and marquee box selection.
        let minimap_active = state.dragging_minimap || pointer_over_minimap;

        // Minimap click/drag: pan the canvas viewport to center the clicked
        // world point. Clamp the pointer to the minimap rect so dragging past
        // the border still works cleanly.
        if (primary_down || primary_pressed) && minimap_active {
            if let (Some(mr), Some(pointer)) = (minimap_rect, pointer_pos) {
                // Clamp pointer into the minimap rect.
                let clamped = pointer.clamp(mr.min, mr.max);
                // Compute minimap world bounds strictly from nodes.
                let bounds = minimap_world_bounds(graph, state, mr);
                if let Some((world_min, _world_max, scale, offset)) = bounds {
                    // Invert the uniform minimap transform to world space.
                    // minimap_pos = mr.min + offset + (world - world_min) * scale
                    // => world = world_min + (minimap_pos - mr.min - offset) / scale
                    let t = (clamped - mr.min - offset) / scale;
                    let target_world = world_min + t;
                    // Exact centering: after this, world_to_screen(target_world)
                    // == rect.center().
                    state.pan_to_center_world(target_world, rect);
                    ui.ctx().request_repaint();
                }
            }
        }

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

        // Prune deleted nodes from the A/B viewer slots.
        for slot in &mut state.viewer_slots {
            if slot.is_some_and(|id| graph.node(id).is_none()) {
                *slot = None;
            }
        }

        // Seed slot 1 from the current viewer binding the first time the canvas
        // sees a graph. Nuke stores the node the physical Viewer looks through;
        // Fusion stores the viewer target directly.
        if state.viewer_slots[0].is_none() {
            if let Some(viewer_id) = viewer {
                match workflow {
                    ViewerWorkflow::Fusion => state.viewer_slots[0] = Some(viewer_id),
                    ViewerWorkflow::Nuke => {
                        if graph
                            .node(viewer_id)
                            .is_some_and(|node| node.kind == "Viewer")
                        {
                            if let Some(edge) = graph.incoming_edge(viewer_id, PortId(0)) {
                                state.viewer_slots[0] = Some(edge.from_node);
                            }
                        }
                    }
                }
            }
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

        let over_canvas =
            pointer_pos.is_some_and(|pointer| rect.contains(pointer)) || pointer_pos.is_none();
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
                    selected_snapshot: state.selected_nodes.clone(),
                    primary_snapshot: state.primary_selected,
                });
            }
        }
        let search_open = state.is_search_open();
        // True when any spawn UI (Tab popup, context menu, node menu) is open.
        // While any of these is open the canvas must not process pointer
        // press/down/release, otherwise a popup click can clear the selection
        // before the spawn is handled (frame-order bug).
        let spawn_ui_open = search_open || state.context_menu_open || state.node_menu_open;

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
        if primary_pressed && press_starts_inside && !spawn_ui_open && !minimap_active {
            if let Some(focused_id) = ui.ctx().memory(|mem| mem.focused()) {
                ui.ctx().memory_mut(|mem| mem.surrender_focus(focused_id));
            }
        }

        if primary_pressed && press_starts_inside && !spawn_ui_open && !minimap_active {
            if let Some(pointer) = pointer_pos {
                // A `Dot`'s circular body is a node drag handle. Test it before
                // the port hit-test so its compact geometry can never turn a
                // body click into a stray wire drag.
                if let Some(node) = hit_dot_body(state, graph, pointer, rect) {
                    apply_node_click(state, node, shift);
                    state.dragging_nodes = true;
                    state.box_selection = None;
                    state.box_dragging = false;
                    latch_drag_start(state, graph);
                } else if let Some((to_node, to_port)) = hit_input_port(state, graph, pointer, rect)
                {
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
                    state.box_dragging = false;
                    state.dragging_nodes = false;
                } else if let Some(from_node) = hit_output_port(state, graph, pointer, rect) {
                    state.active_wire_drag = Some(WireDrag::FromOutput {
                        from_node,
                        from_port: PortId(0),
                    });
                    state.box_selection = None;
                    state.box_dragging = false;
                    state.dragging_nodes = false;
                } else if let Some(node) = hit_node(state, graph, pointer, rect) {
                    apply_node_click(state, node, shift);
                    state.dragging_nodes = true;
                    state.box_selection = None;
                    state.box_dragging = false;
                    // Latch the pre-drag position of every selected node from
                    // the mirror. Every frame of this gesture reports these as
                    // `from`, so undo returns the whole group to this exact
                    // spot even if the moves never coalesce.
                    latch_drag_start(state, graph);
                } else if let Some(backdrop) =
                    hit_backdrop_resize_handle(state, graph, pointer, rect)
                {
                    // Bottom-right grip: begin a corner-resize gesture.
                    if !state.selected_nodes.contains(&backdrop) {
                        state.selected_nodes.clear();
                        state.selected_nodes.insert(backdrop);
                    }
                    state.primary_selected = Some(backdrop);
                    let node = graph.node(backdrop);
                    let (w, h) = node
                        .map(|node| {
                            let size = node_world_size(node);
                            (size.x, size.y)
                        })
                        .unwrap_or((320.0, 200.0));
                    state.resizing_backdrop = Some((backdrop, w, h));
                    state.resize_preview = Some((backdrop, w, h));
                    state.dragging_nodes = false;
                    state.box_selection = None;
                    state.box_dragging = false;
                } else if let Some(backdrop) = hit_backdrop_header(state, graph, pointer, rect) {
                    // Header bar: drag the backdrop and everything it encloses.
                    apply_node_click(state, backdrop, shift);
                    state.box_selection = None;
                    state.box_dragging = false;
                    if state.selected_nodes.contains(&backdrop) {
                        state.dragging_nodes = true;
                        // Capture the enclosed cards once, at mouse-down. The
                        // drag loop only ever moves this latched set.
                        latch_drag_start(state, graph);
                    } else {
                        // Shift-click deselected the backdrop: no drag starts.
                        state.dragging_nodes = false;
                        state.drag_start_positions.clear();
                    }
                } else {
                    // Empty background: start a world-space box selection.
                    state.dragging_nodes = false;
                    state.drag_start_positions.clear();
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

        if primary_down && !spawn_ui_open {
            if state.box_dragging {
                if let Some(pointer) = pointer_pos {
                    if let Some((start, _)) = state.box_selection {
                        state.box_selection = Some((start, pointer));
                    }
                }
            } else if let Some((backdrop, _initial_w, _initial_h)) = state.resizing_backdrop {
                // Corner resize: track the pointer in world space. The size is
                // only committed to the kernel on release (one undo step).
                if let Some(pointer) = pointer_pos {
                    if let Some(position) = state.node_position(backdrop) {
                        let world = state.screen_to_world(rect, pointer);
                        let width = (world.x - position.x).max(140.0);
                        let height = (world.y - position.y).max(90.0);
                        state.resize_preview = Some((backdrop, width, height));
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

        if primary_released && !spawn_ui_open {
            // Commit a corner-resize as one atomic `Batch` of two `SetKnob`s.
            if let Some((backdrop, initial_w, initial_h)) = state.resizing_backdrop.take() {
                if let Some((id, width, height)) = state.resize_preview.take() {
                    if id == backdrop && (width != initial_w || height != initial_h) {
                        messages.push(KernelMessage::Execute(UndoableCommand::Batch(vec![
                            UndoableCommand::SetKnob {
                                node: backdrop,
                                knob: "bdwidth".to_string(),
                                value: KnobValue::Float(width as f64),
                                previous: KnobValue::Float(initial_w as f64),
                            },
                            UndoableCommand::SetKnob {
                                node: backdrop,
                                knob: "bdheight".to_string(),
                                value: KnobValue::Float(height as f64),
                                previous: KnobValue::Float(initial_h as f64),
                            },
                        ])));
                    }
                }
            }

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
            // releasing only clears the latched drag. One special case: a sole
            // unconnected node dropped on a wire splices itself into that pipe.
            if state.dragging_nodes {
                state.dragging_nodes = false;
                let splice = dragged_node_splice(state, graph, rect);
                state.drag_start_positions.clear();
                if let Some(batch) = splice {
                    messages.push(KernelMessage::Execute(batch));
                }
            }
        }

        if middle_down && !minimap_active {
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
                        // The Viewer is a physical binding, never bypassable.
                        let is_viewer = graph
                            .node(node_id)
                            .is_some_and(|node| node.kind == "Viewer");
                        if !is_viewer && ui.button("Toggle Bypass (D)").clicked() {
                            action = Some(NodeMenuAction::ToggleBypass(node_id));
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
                Some(NodeMenuAction::ToggleBypass(id)) => {
                    if let Some(node) = graph.node(id) {
                        messages.push(KernelMessage::Execute(UndoableCommand::SetDisabled {
                            node: id,
                            disabled: !node.disabled,
                        }));
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
                messages.push(spawn_node_message(graph, state, kind, position));
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

        // Nuke binds `1` / `2` to the two viewer comparison slots.
        if !keyboard_busy && !search_open && !modal_open {
            let viewer_slot_key = ui.ctx().input_mut(|input| {
                if input.consume_key(Modifiers::NONE, Key::Num1) {
                    Some(0)
                } else if input.consume_key(Modifiers::NONE, Key::Num2) {
                    Some(1)
                } else {
                    None
                }
            });
            if let Some(slot) = viewer_slot_key {
                // A `BackdropNode` is a canvas container, never a viewer target.
                let primary_is_backdrop = state
                    .primary_selected
                    .and_then(|id| graph.node(id))
                    .is_some_and(|node| is_backdrop_node(&node.kind));
                if !primary_is_backdrop {
                    if let Some(target) = state.select_viewer_slot(slot) {
                        messages.push(viewer_message(graph, workflow, target));
                    }
                }
            }
        }

        let delete_pressed =
            ui.input(|input| input.key_pressed(Key::Delete) || input.key_pressed(Key::Backspace));
        if delete_pressed
            && !keyboard_busy
            && !search_open
            && !modal_open
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

        // Nuke's `D` toggles bypass on every selected node. Strictly
        // `Modifiers::NONE` so `Ctrl+D` (duplicate) is never consumed here.
        if !keyboard_busy
            && !search_open
            && !modal_open
            && state.active_wire_drag.is_none()
            && state.box_selection.is_none()
        {
            let bypass_pressed = ui
                .ctx()
                .input_mut(|input| input.consume_key(Modifiers::NONE, Key::D));
            if bypass_pressed {
                let mut ids: Vec<NodeId> = state.selected_nodes.iter().copied().collect();
                ids.sort();
                let mut toggles: Vec<UndoableCommand> = Vec::new();
                for id in ids {
                    let Some(node) = graph.node(id) else {
                        continue;
                    };
                    // The physical Viewer is a binding, not an operator.
                    if node.kind == "Viewer" {
                        continue;
                    }
                    toggles.push(UndoableCommand::SetDisabled {
                        node: id,
                        disabled: !node.disabled,
                    });
                }
                // One `from_vec` batch means one `Ctrl+Z` restores the whole
                // multi-node bypass toggle.
                if let Some(batch) = UndoableCommand::from_vec(toggles) {
                    messages.push(KernelMessage::Execute(batch));
                }
            }

            // `F` frames the selection, or the whole graph when nothing is
            // selected.
            let frame_pressed = ui
                .ctx()
                .input_mut(|input| input.consume_key(Modifiers::NONE, Key::F));
            if frame_pressed {
                state.frame_nodes(graph, rect);
            }
        }

        // `Shift+X` swaps the `B` / `A` inputs of every selected multi-input
        // node. Collecting the disconnects and reconnects into one batch means
        // a single `Ctrl+Z` swaps the pipes straight back.
        if !keyboard_busy
            && !search_open
            && !modal_open
            && state.active_wire_drag.is_none()
            && state.box_selection.is_none()
        {
            let swap_pressed = ui
                .ctx()
                .input_mut(|input| input.consume_key(Modifiers::SHIFT, Key::X));
            if swap_pressed {
                let mut ids: Vec<NodeId> = state.selected_nodes.iter().copied().collect();
                ids.sort();
                let mut swaps: Vec<UndoableCommand> = Vec::new();
                for id in ids {
                    let accepts_two = graph
                        .node(id)
                        .is_some_and(|node| input_port_count(&node.kind) >= 2);
                    if !accepts_two {
                        continue;
                    }
                    let edge_0 = graph.incoming_edge(id, PortId(0));
                    let edge_1 = graph.incoming_edge(id, PortId(1));
                    match (edge_0, edge_1) {
                        (Some(a), Some(b)) => {
                            swaps.push(UndoableCommand::DisconnectEdge {
                                from_node: a.from_node,
                                from_port: a.from_port,
                                to_node: a.to_node,
                                to_port: a.to_port,
                            });
                            swaps.push(UndoableCommand::DisconnectEdge {
                                from_node: b.from_node,
                                from_port: b.from_port,
                                to_node: b.to_node,
                                to_port: b.to_port,
                            });
                            swaps.push(UndoableCommand::ConnectEdge {
                                from_node: a.from_node,
                                from_port: a.from_port,
                                to_node: id,
                                to_port: PortId(1),
                            });
                            swaps.push(UndoableCommand::ConnectEdge {
                                from_node: b.from_node,
                                from_port: b.from_port,
                                to_node: id,
                                to_port: PortId(0),
                            });
                        }
                        (Some(a), None) => {
                            swaps.push(UndoableCommand::DisconnectEdge {
                                from_node: a.from_node,
                                from_port: a.from_port,
                                to_node: a.to_node,
                                to_port: a.to_port,
                            });
                            swaps.push(UndoableCommand::ConnectEdge {
                                from_node: a.from_node,
                                from_port: a.from_port,
                                to_node: id,
                                to_port: PortId(1),
                            });
                        }
                        (None, Some(b)) => {
                            swaps.push(UndoableCommand::DisconnectEdge {
                                from_node: b.from_node,
                                from_port: b.from_port,
                                to_node: b.to_node,
                                to_port: b.to_port,
                            });
                            swaps.push(UndoableCommand::ConnectEdge {
                                from_node: b.from_node,
                                from_port: b.from_port,
                                to_node: id,
                                to_port: PortId(0),
                            });
                        }
                        (None, None) => {}
                    }
                }
                if let Some(batch) = UndoableCommand::from_vec(swaps) {
                    messages.push(KernelMessage::Execute(batch));
                }
            }
        }

        // Default Nuke single-key spawning. Strict `Modifiers::NONE` keeps
        // Ctrl+C / Ctrl+V / Ctrl+D for the app's clipboard shortcuts.
        let spawn_guard = over_canvas
            && pointer_pos.is_none_or(|pointer| {
                ui.ctx()
                    .layer_id_at(pointer)
                    .is_none_or(|layer| layer == ui.layer_id())
            })
            && !ui.ctx().egui_wants_keyboard_input()
            && !ui.memory(|memory| memory.focused().is_some())
            && !search_open
            && state.active_wire_drag.is_none()
            && !modal_open;
        if spawn_guard {
            // `Shift+B` spawns a `BackdropNode` (plain `B` is `Blur`). Check it
            // before the plain-key list so the shift modifier isn't ignored.
            let mut spawn_kind: Option<&'static str> = None;
            if ui.ctx().input_mut(|input| {
                input.modifiers.shift && input.consume_key(Modifiers::SHIFT, Key::B)
            }) {
                spawn_kind = Some("BackdropNode");
            } else {
                let spawn_key = ui.ctx().input_mut(|input| {
                    [
                        Key::R,
                        Key::W,
                        Key::C,
                        Key::G,
                        Key::B,
                        Key::T,
                        Key::M,
                        Key::Period,
                        Key::Z,
                    ]
                    .into_iter()
                    .find(|key| input.consume_key(Modifiers::NONE, *key))
                });
                if let Some(key) = spawn_key {
                    spawn_kind = nuke_key_to_node_kind(key);
                }
            }
            if let Some(kind) = spawn_kind {
                let anchor_screen = pointer_pos
                    .unwrap_or_else(|| rect.center())
                    .clamp(rect.min, rect.max);
                let world_pos = state.screen_to_world(rect, anchor_screen);
                messages.push(spawn_node_message(graph, state, kind, world_pos));
            }
        }

        let pal = palette(state.theme);

        draw_grid(&painter, rect, state.pan, state.zoom, &pal);

        // PASS 1 — Draw all `BackdropNode`s first, behind wires and nodes.
        // They are canvas containers, not part of the image pipeline.
        for node in graph.nodes() {
            if !is_backdrop_node(&node.kind) {
                continue;
            }
            let Some(position) = state.node_position(node.id) else {
                continue;
            };
            let node_size = match state.resize_preview {
                Some((id, w, h)) if id == node.id => Vec2::new(w, h),
                _ => node_world_size(node),
            };
            let node_rect = node_screen_rect_at(state, position, node_size, rect);
            draw_backdrop(
                &painter,
                node_rect,
                node,
                state.zoom,
                &pal,
                state.selected_nodes.contains(&node.id),
            );
        }

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
            let from_size = graph
                .node(edge.from_node)
                .map_or(Vec2::new(NODE_W, NODE_H), node_world_size);
            let to_size = node_world_size(to_node);
            let from_rect = node_screen_rect_at(state, from_position, from_size, rect);
            let to_rect = node_screen_rect_at(state, to_position, to_size, rect);
            let from = output_port(from_rect, state.zoom);
            let port_count = input_port_count(&to_node.kind);
            let to = input_port(to_rect, &to_node.kind, port_count, edge.to_port);
            let side = input_port_side(&to_node.kind, port_count, edge.to_port.0 as usize);
            let label = input_port_label(&to_node.kind, port_count, edge.to_port.0 as usize);
            let label = if label.is_empty() { None } else { Some(label) };
            let wire_color = match side {
                PortSide::Top => pal.wire_default,
                PortSide::Left => pal.pin_a,
                PortSide::Right => pal.pin_m,
            };
            draw_wire(&painter, from, to, side, state.zoom, wire_color, label);
        }

        // PASS 3 — Draw all non-`BackdropNode`s and `Dot`s on top of wires.
        for node in graph.nodes() {
            if is_backdrop_node(&node.kind) {
                continue; // drawn in pass 1
            }
            let Some(position) = state.node_position(node.id) else {
                continue;
            };
            let node_size = node_world_size(node);
            let node_rect = node_screen_rect_at(state, position, node_size, rect);
            if is_dot_node(&node.kind) {
                draw_dot(
                    &painter,
                    node_rect,
                    state.selected_nodes.contains(&node.id),
                    state.zoom,
                    &pal,
                );
                continue;
            }
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
            let viewer_slot_label = is_viewer.then_some(if state.active_viewer_slot == 0 {
                "VIEWER 1"
            } else {
                "VIEWER 2"
            });
            draw_node(
                &painter,
                node_rect,
                node,
                state.selected_nodes.contains(&node.id),
                viewer_slot_label,
                state.zoom,
                &pal,
                active_port,
                graph.node_perf.get(&node.id).copied(),
            );
        }

        if let Some(pointer) = pointer_pos {
            if let Some((from, to, side)) = live_wire_points(state, graph, rect, pointer) {
                let wire_color = match side {
                    PortSide::Top => pal.wire_default,
                    PortSide::Left => pal.pin_a,
                    PortSide::Right => pal.pin_m,
                };
                draw_wire(&painter, from, to, side, state.zoom, wire_color, None);
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

        // Node Graph Minimap: semi-transparent dark inset in the bottom-right
        // corner. Click/drag inside it pans the canvas viewport (checked before
        // node hit-testing and box selection so it never steals from those).
        if state.show_minimap && !graph.nodes().is_empty() {
            let minimap_size = Vec2::new(170.0, 110.0);
            let margin = 8.0;
            let minimap_rect = Rect::from_min_size(
                Pos2::new(
                    rect.max.x - minimap_size.x - margin,
                    rect.max.y - minimap_size.y - margin,
                ),
                minimap_size,
            );
            painter.rect_filled(
                minimap_rect,
                4.0,
                Color32::from_rgba_unmultiplied(16, 18, 24, 220),
            );
            painter.rect_stroke(
                minimap_rect,
                4.0,
                Stroke::new(1.0, Color32::from_rgb(60, 65, 75)),
                StrokeKind::Inside,
            );

            // World-space bounds and uniform transform for the minimap.
            if let Some((world_min, _world_max, scale, offset)) =
                minimap_world_bounds(graph, state, minimap_rect)
            {
                // Map a world point into minimap screen space.
                let world_to_minimap = |world: Pos2| {
                    minimap_rect.min + offset + (world - world_min) * Vec2::splat(scale)
                };

                // Viewport indicator box (what the main canvas shows).
                let view_world_min = state.screen_to_world(rect, rect.min);
                let view_world_max = state.screen_to_world(rect, rect.max);
                let vp_min = world_to_minimap(view_world_min);
                let vp_max = world_to_minimap(view_world_max);
                let vp_rect = Rect::from_two_pos(vp_min, vp_max);
                let vp_intersects = vp_rect.intersects(minimap_rect);

                painter.set_clip_rect(minimap_rect);
                painter.rect_filled(
                    vp_rect,
                    2.0,
                    Color32::from_rgba_unmultiplied(90, 190, 255, 100),
                );
                painter.rect_stroke(
                    vp_rect,
                    2.0,
                    Stroke::new(1.5, Color32::from_rgb(120, 200, 255)),
                    StrokeKind::Inside,
                );

                // Draw category-colored node rectangles.
                for node in graph.nodes() {
                    let Some(pos) = state.node_position(node.id) else {
                        continue;
                    };
                    let size = node_world_size(node);
                    let n_min = world_to_minimap(pos);
                    let n_max = world_to_minimap(pos + size);
                    let is_selected = state.selected_nodes.contains(&node.id);
                    let node_color = match node_category(&node.kind) {
                        crate::theme::NodeCategory::Color => Color32::from_rgb(220, 80, 80),
                        crate::theme::NodeCategory::Merge => Color32::from_rgb(80, 160, 220),
                        crate::theme::NodeCategory::Filter => Color32::from_rgb(120, 200, 120),
                        crate::theme::NodeCategory::Transform => Color32::from_rgb(220, 160, 80),
                        crate::theme::NodeCategory::Io => Color32::from_rgb(160, 120, 220),
                        crate::theme::NodeCategory::Viewer => Color32::from_rgb(220, 120, 160),
                        _ => Color32::from_rgb(160, 160, 160),
                    };
                    painter.rect_filled(Rect::from_two_pos(n_min, n_max), 2.0, node_color);
                    if is_selected {
                        painter.rect_stroke(
                            Rect::from_two_pos(n_min, n_max),
                            2.0,
                            Stroke::new(1.5, Color32::from_rgb(255, 200, 0)),
                            StrokeKind::Inside,
                        );
                    }
                }
                painter.set_clip_rect(rect);

                // Off-screen edge marker: if the viewport does not intersect the
                // minimap (camera panned far away), paint a bright blue/cyan 6px
                // clamped indicator pill along the nearest edge/corner pointing
                // toward the off-screen viewport.
                if !vp_intersects {
                    let marker_color = Color32::from_rgb(0, 255, 255);
                    let marker_thickness = 6.0;
                    let center = minimap_rect.center();
                    let vp_center = vp_rect.center();
                    let direction = (vp_center - center).normalized();
                    // Clamp the indicator to the minimap edge (manual clamp).
                    let edge_point = Pos2::new(
                        vp_center.x.clamp(minimap_rect.min.x, minimap_rect.max.x),
                        vp_center.y.clamp(minimap_rect.min.y, minimap_rect.max.y),
                    );
                    // Draw a short line from the edge toward the viewport direction.
                    let end = edge_point + direction * 24.0;
                    painter.line_segment(
                        [edge_point, end],
                        Stroke::new(marker_thickness, marker_color),
                    );
                    // Also draw a dot at the clamped edge point for visibility.
                    painter.circle_filled(edge_point, 4.0, marker_color);
                }
            }
        }

        // Non-blocking per-node timing tooltip. Drawn as an `interactable(false)`
        // area so it can never steal a click, drag, or context menu from the
        // canvas hit-test chain above.
        let hover_quiet = !spawn_ui_open
            && !minimap_active
            && state.active_wire_drag.is_none()
            && !state.dragging_nodes
            && state.box_selection.is_none()
            && state.resizing_backdrop.is_none();
        if hover_quiet
            && let Some(pointer) = ui.ctx().input(|input| input.pointer.hover_pos())
            && rect.contains(pointer)
            && hit_input_port(state, graph, pointer, rect).is_none()
            && hit_output_port(state, graph, pointer, rect).is_none()
            && let Some(node) = graph.nodes().iter().rev().find(|node| {
                !is_backdrop_node(&node.kind)
                    && node_screen_rect(state, node.id, rect, graph).contains(pointer)
            })
            && let Some(&(eval_us, from_cache)) = graph.node_perf.get(&node.id)
        {
            let ms = eval_us as f64 / 1000.0;
            let state_str = if from_cache { "cached" } else { "computed" };
            let text = format!(
                "{} #{} | {:.2} ms ({})",
                node.kind, node.id.0, ms, state_str
            );
            draw_node_perf_tooltip(ui, pointer, &text);
        }

        if let Some(message) = draw_tab_search(ui, state, graph, workflow) {
            messages.push(message);
        }

        // Bottom-left shortcut hint. A framed pill keeps it readable over node
        // cards instead of colliding with their labels.
        let hint = "Keys: R W C G B T M . Z | Tab: search | 1/2: view | D: bypass | F: frame | Shift+X: swap";
        let hint_color = Color32::from_gray(165);
        let galley =
            painter.layout_no_wrap(hint.to_string(), FontId::proportional(11.0), hint_color);
        let hint_anchor = rect.left_bottom() + Vec2::new(12.0, -10.0);
        let text_rect = Rect::from_min_size(
            Pos2::new(hint_anchor.x, hint_anchor.y - galley.size().y),
            galley.size(),
        );
        painter.rect(
            text_rect.expand2(Vec2::new(8.0, 4.0)),
            9.0,
            Color32::from_rgba_unmultiplied(16, 18, 22, 215),
            Stroke::new(1.0, Color32::from_rgb(55, 60, 70)),
            StrokeKind::Inside,
        );
        painter.galley(text_rect.min, galley, hint_color);

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
    graph: &GraphMirror,
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
                    // The spawn list is long enough to overflow a short
                    // window; cap it so the popup keeps a stable, on-screen
                    // size instead of being pushed around by the constraint.
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for kind in matches {
                                if ui.button(kind).clicked() {
                                    spawn = Some(kind);
                                }
                            }
                        });
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
        // Restore the selection that existed when Tab opened, so spawning from
        // the popup wraps the box-selected nodes instead of an empty set. The
        // canvas pointer handler runs before this popup and would otherwise
        // have cleared `selected_nodes` on an empty-canvas click.
        state.selected_nodes = search.selected_snapshot.clone();
        state.primary_selected = search.primary_snapshot;
        return Some(spawn_node_message(graph, state, kind, search.world_pos));
    }
    if close {
        state.tab_search = None;
    } else {
        state.tab_search = Some(search);
    }
    None
}

/// Builds the viewer message for the active workflow.
pub fn viewer_message(
    graph: &GraphMirror,
    workflow: ViewerWorkflow,
    node: NodeId,
) -> KernelMessage {
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
///
/// Kept for direct callers and unit tests. Interactive spawns go through
/// [`spawn_node_message`], which advances selection and auto-chains.
fn add_node_message(state: &NodeCanvasState, kind: &str, position: Pos2) -> KernelMessage {
    KernelMessage::Execute(UndoableCommand::AddNode {
        id: NodeId(state.next_node_id),
        kind: kind.to_string(),
        x: position.x,
        y: position.y,
    })
}

/// Infers a full-resolution format from the first size knobs on the main
/// input chain, falling back to the project format.
pub fn infer_upstream_format(mirror: &GraphMirror, start_node: NodeId) -> Option<(u32, u32)> {
    let mut node_id = start_node;
    let mut visited = HashSet::new();
    for _ in 0..64 {
        if !visited.insert(node_id) {
            break;
        }
        let Some(node) = mirror.node(node_id) else {
            break;
        };
        if let (Some(KnobValue::Int(w)), Some(KnobValue::Int(h))) = (
            node.knobs.get("format_width"),
            node.knobs.get("format_height"),
        ) {
            return Some(((*w).clamp(1, 8192) as u32, (*h).clamp(1, 8192) as u32));
        }
        let Some(edge) = mirror.incoming_edge(node_id, PortId(0)) else {
            break;
        };
        node_id = edge.from_node;
    }
    mirror
        .project_format()
        .map(|format| (format.width, format.height))
}

pub(crate) fn corner_pin_format_knobs(
    (width, height): (u32, u32),
) -> [(&'static str, KnobValue); 4] {
    let w = width as f32;
    let h = height as f32;
    [
        ("to1", [0.0; 4]),
        ("to2", [w, 0.0, 0.0, 0.0]),
        ("to3", [w, h, 0.0, 0.0]),
        ("to4", [0.0, h, 0.0, 0.0]),
    ]
    .map(|(name, coords)| (name, KnobValue::Color(coords)))
}

/// Spawns `kind` at the cursor, or auto-chains it below the active node.
///
/// `BackdropNode` wraps the currently selected non-backdrop nodes in an
/// atomic `AddNode` + `SetKnob`(`bdwidth`/`bdheight`) batch.
///
/// Processor and combiner nodes (`input_count >= 1`) chain vertically under a
/// selected non-Viewer, non-Backdrop node, center-aligned so `Dot` nodes and
/// standard cards share a dead-straight vertical spine. Sources (`Read`,
/// `Constant`) and the `Viewer` always spawn unconnected at the cursor. The
/// new node becomes the active selection so the next key in a fast
/// `G` -> `B` -> `T` sequence chains from it.
pub fn spawn_node_message(
    mirror: &GraphMirror,
    state: &mut NodeCanvasState,
    kind: &str,
    cursor_world: Pos2,
) -> KernelMessage {
    let max_id = mirror
        .nodes()
        .iter()
        .map(|node| node.id.0)
        .max()
        .unwrap_or(0);
    state.next_node_id = state.next_node_id.max(max_id.saturating_add(1));

    let new_id = NodeId(state.next_node_id);
    let input_count = input_port_count(kind);

    // Case 0: `BackdropNode` wraps the selected non-backdrop nodes.
    if kind == "BackdropNode" {
        let selected_non_backdrop: Vec<NodeId> = state
            .selected_nodes
            .iter()
            .copied()
            .filter(|id| {
                !mirror
                    .node(*id)
                    .is_some_and(|node| is_backdrop_node(&node.kind))
            })
            .collect();
        if !selected_non_backdrop.is_empty() {
            let mut bounds: Option<Rect> = None;
            for id in &selected_non_backdrop {
                let Some(node) = mirror.node(*id) else {
                    continue;
                };
                // Use the mirror's authoritative world coordinates rather than
                // the UI-side position cache, so the backdrop wraps the nodes
                // where they actually are even after an undo/redo.
                let position = Pos2::new(node.x, node.y);
                let size = node_world_size(node);
                let card = Rect::from_min_size(position, size);
                bounds = Some(bounds.map_or(card, |current| current.union(card)));
            }
            if let Some(bounds) = bounds {
                let x = bounds.min.x - 36.0;
                let y = bounds.min.y - 44.0;
                let bdwidth = (bounds.width() + 72.0).max(200.0);
                let bdheight = (bounds.height() + 76.0).max(140.0);
                state.next_node_id = state.next_node_id.saturating_add(1);
                state.selected_nodes.clear();
                state.selected_nodes.insert(new_id);
                state.primary_selected = Some(new_id);
                return KernelMessage::Execute(UndoableCommand::Batch(vec![
                    UndoableCommand::AddNode {
                        id: new_id,
                        kind: "BackdropNode".to_string(),
                        x,
                        y,
                    },
                    UndoableCommand::SetKnob {
                        node: new_id,
                        knob: "bdwidth".to_string(),
                        value: KnobValue::Float(bdwidth as f64),
                        previous: KnobValue::Float(320.0),
                    },
                    UndoableCommand::SetKnob {
                        node: new_id,
                        knob: "bdheight".to_string(),
                        value: KnobValue::Float(bdheight as f64),
                        previous: KnobValue::Float(200.0),
                    },
                ]));
            }
        }
        // Fall through to unconnected spawn when nothing is selected.
    }

    // Case 1: chain below the selected processor/combiner (center-aligned).
    if let Some(up_id) = state.primary_selected {
        if let Some(up_node) = mirror.find_node(up_id) {
            if up_node.kind != "Viewer"
                && kind != "Viewer"
                && input_count >= 1
                && !is_backdrop_node(&up_node.kind)
            {
                let up_size = node_world_size(up_node);
                let new_size = if is_dot_node(kind) {
                    DOT_SIZE
                } else {
                    NODE_SIZE
                };
                let x = up_node.x + (up_size.x - new_size.x) * 0.5;

                // `Dot` and `Blur` are splice tools. When the selected card
                // already feeds a downstream node on its main pipe, insert the
                // new node into that wire instead of leaving the old wire
                // dangling. Every other cartridge keeps the historical
                // leaf/branch behavior so `G`/`B`/`T` chains stay at the 90px
                // pitch the Milestone 17 tests pin down.
                let splice = is_dot_node(kind) || kind == "Blur";
                let downstream: Vec<CanvasEdge> = if splice {
                    mirror
                        .edges()
                        .iter()
                        .copied()
                        .filter(|edge| edge.from_node == up_id && edge.from_port == PortId(0))
                        .collect()
                } else {
                    Vec::new()
                };

                if !downstream.is_empty() {
                    // True pipe splice: keep the new node centered on the
                    // spine, never nudge it sideways.
                    let spacing = if is_dot_node(kind) || is_dot_node(&up_node.kind) {
                        28.0
                    } else {
                        38.0
                    };
                    let y = up_node.y + up_size.y + spacing;

                    let mut commands: Vec<UndoableCommand> = vec![
                        UndoableCommand::AddNode {
                            id: new_id,
                            kind: kind.to_string(),
                            x,
                            y,
                        },
                        UndoableCommand::ConnectEdge {
                            from_node: up_id,
                            from_port: PortId(0),
                            to_node: new_id,
                            to_port: PortId(0),
                        },
                    ];
                    for edge in &downstream {
                        commands.push(UndoableCommand::DisconnectEdge {
                            from_node: edge.from_node,
                            from_port: edge.from_port,
                            to_node: edge.to_node,
                            to_port: edge.to_port,
                        });
                        commands.push(UndoableCommand::ConnectEdge {
                            from_node: new_id,
                            from_port: PortId(0),
                            to_node: edge.to_node,
                            to_port: edge.to_port,
                        });
                    }

                    // Push the downstream column down so the freshly spliced
                    // node never overlaps its neighbour. All moves ride in the
                    // same atomic `Batch`, so one `Ctrl+Z` reverses the whole
                    // splice.
                    let new_center_x = x + new_size.x * 0.5;
                    let in_column = |node: &CanvasNode| {
                        !is_backdrop_node(&node.kind)
                            && node.id != up_id
                            && (node.x + node_world_size(node).x * 0.5 - new_center_x).abs() < 65.0
                    };
                    let overlapping_y = mirror
                        .nodes()
                        .iter()
                        .filter(|node| in_column(node))
                        .filter(|node| node.y >= up_node.y + up_size.y)
                        .filter(|node| node.y < y + new_size.y + 24.0)
                        .map(|node| node.y)
                        .fold(f32::INFINITY, f32::min);
                    if overlapping_y.is_finite() {
                        let push_dy = (y + new_size.y + spacing) - overlapping_y;
                        if push_dy > 0.0 {
                            let mut pushed: Vec<(NodeId, f32, f32)> = mirror
                                .nodes()
                                .iter()
                                .filter(|node| in_column(node))
                                .filter(|node| node.y >= overlapping_y)
                                .map(|node| (node.id, node.x, node.y))
                                .collect();
                            pushed.sort_by(|a, b| a.2.total_cmp(&b.2));
                            for (id, node_x, node_y) in pushed {
                                commands.push(UndoableCommand::MoveNode {
                                    id,
                                    from_x: node_x,
                                    from_y: node_y,
                                    to_x: node_x,
                                    to_y: node_y + push_dy,
                                });
                            }
                        }
                    }

                    state.next_node_id = state.next_node_id.saturating_add(1);
                    state.selected_nodes.clear();
                    state.selected_nodes.insert(new_id);
                    state.primary_selected = Some(new_id);

                    return KernelMessage::Execute(UndoableCommand::Batch(commands));
                }

                // Leaf / branch: centered chain, nudged sideways only when an
                // unrelated card already owns the target slot.
                let mut x = x;
                let y = up_node.y + up_size.y + CHAIN_GAP;
                while mirror
                    .nodes()
                    .iter()
                    .any(|node| (node.x - x).abs() < 40.0 && (node.y - y).abs() < 30.0)
                {
                    x += 140.0;
                }

                state.next_node_id = state.next_node_id.saturating_add(1);
                state.selected_nodes.clear();
                state.selected_nodes.insert(new_id);
                state.primary_selected = Some(new_id);

                let mut commands = vec![
                    UndoableCommand::AddNode {
                        id: new_id,
                        kind: kind.to_string(),
                        x,
                        y,
                    },
                    UndoableCommand::ConnectEdge {
                        from_node: up_id,
                        from_port: PortId(0),
                        to_node: new_id,
                        to_port: PortId(0),
                    },
                ];
                if kind == "CornerPin2D"
                    && let Some(size) = infer_upstream_format(mirror, up_id)
                {
                    let spawn_size = mirror
                        .project_format()
                        .map_or((256, 256), |format| (format.width, format.height));
                    commands.extend(
                        corner_pin_format_knobs(size)
                            .into_iter()
                            .zip(corner_pin_format_knobs(spawn_size))
                            .map(|((name, value), (_, previous))| UndoableCommand::SetKnob {
                                node: new_id,
                                knob: name.into(),
                                value,
                                previous,
                            }),
                    );
                }
                return KernelMessage::Execute(UndoableCommand::Batch(commands));
            }
        }
    }

    // Case 2: unconnected spawn at the cursor / anchor.
    let (mut x, y) = (cursor_world.x, cursor_world.y);
    // A `BackdropNode` is a container, not a card: never nudge it sideways or
    // it can end up off to the side of the nodes it was meant to wrap.
    if !is_backdrop_node(kind) {
        while mirror
            .nodes()
            .iter()
            .any(|node| (node.x - x).abs() < 40.0 && (node.y - y).abs() < 30.0)
        {
            x += 140.0;
        }
    }

    let message = add_node_message(state, kind, Pos2::new(x, y));
    state.next_node_id = state.next_node_id.saturating_add(1);
    state.selected_nodes.clear();
    state.selected_nodes.insert(new_id);
    state.primary_selected = Some(new_id);
    message
}

/// Maps a plain Nuke spawn key to its cartridge kind.
///
/// `Shift+B` is handled separately in the spawn handler (it spawns
/// `BackdropNode`); plain `B` is `Blur`. Plain `Z` spawns `BackdropNode`.
pub fn nuke_key_to_node_kind(key: Key) -> Option<&'static str> {
    match key {
        Key::R => Some("Read"),
        Key::W => Some("Write"),
        Key::C => Some("Constant"),
        Key::G => Some("Grade"),
        Key::B => Some("Blur"),
        Key::T => Some("Transform"),
        Key::M => Some("Merge2"),
        Key::Period => Some("Dot"),
        Key::Z => Some("BackdropNode"),
        _ => None,
    }
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
///
/// The third value is the side of the destination pin, so the live Bezier
/// enters a left/right mask pin horizontally instead of from above.
fn live_wire_points(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    rect: Rect,
    pointer: Pos2,
) -> Option<(Pos2, Pos2, PortSide)> {
    match state.active_wire_drag? {
        WireDrag::FromOutput { from_node, .. } => {
            let position = state.node_position(from_node)?;
            let size = graph
                .node(from_node)
                .map_or(Vec2::new(NODE_W, NODE_H), node_world_size);
            Some((
                output_port(node_screen_rect_at(state, position, size, rect), state.zoom),
                pointer,
                PortSide::Top,
            ))
        }
        WireDrag::FromInput { to_node, to_port } => {
            let position = state.node_position(to_node)?;
            let node = graph.node(to_node)?;
            let count = input_port_count(&node.kind);
            let side = input_port_side(&node.kind, count, to_port.0 as usize);
            Some((
                pointer,
                input_port(
                    node_screen_rect_at(state, position, node_world_size(node), rect),
                    &node.kind,
                    count,
                    to_port,
                ),
                side,
            ))
        }
        WireDrag::Detaching { from_node, .. } => {
            let position = state.node_position(from_node)?;
            let size = graph
                .node(from_node)
                .map_or(Vec2::new(NODE_W, NODE_H), node_world_size);
            Some((
                output_port(node_screen_rect_at(state, position, size, rect), state.zoom),
                pointer,
                PortSide::Top,
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

fn node_screen_rect(state: &NodeCanvasState, id: NodeId, rect: Rect, graph: &GraphMirror) -> Rect {
    state.node_position(id).map_or(Rect::NOTHING, |position| {
        let size = graph
            .node(id)
            .map_or(Vec2::new(NODE_W, NODE_H), node_world_size);
        node_screen_rect_at(state, position, size, rect)
    })
}

fn node_screen_rect_at(state: &NodeCanvasState, position: Pos2, size: Vec2, rect: Rect) -> Rect {
    let min = state.world_to_screen(rect, position);
    Rect::from_min_size(min, size * state.zoom)
}

/// Screen-space endpoints and downstream edge side of `edge`.
///
/// Returns `None` when either endpoint has not been laid out yet.
fn edge_screen_geometry(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    rect: Rect,
    edge: &CanvasEdge,
) -> Option<(Pos2, Pos2, PortSide)> {
    if state.node_position(edge.from_node).is_none() || state.node_position(edge.to_node).is_none()
    {
        return None;
    }
    let from_rect = node_screen_rect(state, edge.from_node, rect, graph);
    let to_rect = node_screen_rect(state, edge.to_node, rect, graph);
    let to_node = graph.node(edge.to_node)?;
    let port_count = input_port_count(&to_node.kind);
    let from = output_port(from_rect, state.zoom);
    let to = input_port(to_rect, &to_node.kind, port_count, edge.to_port);
    let side = input_port_side(&to_node.kind, port_count, edge.to_port.0 as usize);
    Some((from, to, side))
}

/// The four cubic Bezier control points of a wire, in screen space.
fn wire_curve(from: Pos2, to: Pos2, to_side: PortSide, zoom: f32) -> [Pos2; 4] {
    let distance = match to_side {
        PortSide::Top => (to.y - from.y).abs(),
        PortSide::Left | PortSide::Right => (to.x - from.x).abs(),
    }
    .max(40.0 * zoom);
    let control = distance * 0.5;
    let cp1 = from + Vec2::new(0.0, control);
    let cp2 = match to_side {
        PortSide::Top => to - Vec2::new(0.0, control),
        PortSide::Left => to - Vec2::new(control, 0.0),
        PortSide::Right => to + Vec2::new(control, 0.0),
    };
    [from, cp1, cp2, to]
}

/// Point at `t` in `0..=1` along a cubic Bezier.
fn cubic_bezier_point(points: [Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    Pos2::new(
        a * points[0].x + b * points[1].x + c * points[2].x + d * points[3].x,
        a * points[0].y + b * points[1].y + c * points[2].y + d * points[3].y,
    )
}

/// Screen-space radius for "dropped on a wire" splicing.
const SPLICE_RADIUS: f32 = 16.0;

/// Builds the one-batch command that splices a just-dragged, unconnected node
/// into the wire under its center.
///
/// Returns `None` unless the gesture moved exactly one node, that node accepts
/// an input, is not a `BackdropNode` or `Viewer`, and has no wires yet. The
/// batch holds the drag's `MoveNode` plus the three wire edits, so a single
/// `Ctrl+Z` removes the node from the pipe and returns it to its start.
fn dragged_node_splice(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    rect: Rect,
) -> Option<UndoableCommand> {
    if state.selected_nodes.len() != 1 {
        return None;
    }
    let dragged_id = *state.selected_nodes.iter().next()?;
    let node = graph.node(dragged_id)?;
    if is_backdrop_node(&node.kind) || node.kind == "Viewer" || input_port_count(&node.kind) < 1 {
        return None;
    }
    if graph
        .edges()
        .iter()
        .any(|edge| edge.from_node == dragged_id || edge.to_node == dragged_id)
    {
        return None;
    }

    let (from_x, from_y) = state
        .drag_start_positions
        .get(&dragged_id)
        .copied()
        .unwrap_or((node.x, node.y));
    let (to_x, to_y) = state
        .node_position(dragged_id)
        .map_or((node.x, node.y), |position| (position.x, position.y));
    // A click that never moved is not a splice gesture.
    if (to_x - from_x).abs() < 1e-3 && (to_y - from_y).abs() < 1e-3 {
        return None;
    }

    let center = node_screen_rect(state, dragged_id, rect, graph).center();
    let threshold = SPLICE_RADIUS * state.zoom;
    let mut closest: Option<(CanvasEdge, f32)> = None;
    for edge in graph.edges() {
        let Some((from, to, side)) = edge_screen_geometry(state, graph, rect, edge) else {
            continue;
        };
        let points = wire_curve(from, to, side, state.zoom);
        let mut distance = f32::INFINITY;
        for step in 0..=16 {
            let t = step as f32 / 16.0;
            distance = distance.min(cubic_bezier_point(points, t).distance(center));
        }
        let better = closest.is_none_or(|(_, best)| distance < best);
        if distance <= threshold && better {
            closest = Some((*edge, distance));
        }
    }
    let (edge, _) = closest?;

    Some(UndoableCommand::Batch(vec![
        UndoableCommand::MoveNode {
            id: dragged_id,
            from_x,
            from_y,
            to_x,
            to_y,
        },
        UndoableCommand::DisconnectEdge {
            from_node: edge.from_node,
            from_port: edge.from_port,
            to_node: edge.to_node,
            to_port: edge.to_port,
        },
        UndoableCommand::ConnectEdge {
            from_node: edge.from_node,
            from_port: edge.from_port,
            to_node: dragged_id,
            to_port: PortId(0),
        },
        UndoableCommand::ConnectEdge {
            from_node: dragged_id,
            from_port: PortId(0),
            to_node: edge.to_node,
            to_port: edge.to_port,
        },
    ]))
}

/// Which edge of a node card an input pin sits on.
///
/// Nuke's combiner layout puts the `B` pipe on top, the `A` pipe on the left,
/// and the mask on the right. Two-input filters keep `in` on top and expose
/// their mask on the right edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSide {
    /// Top edge (`B` / `in`).
    Top,
    /// Left edge (`A` on a combiner).
    Left,
    /// Right edge (`M` mask on filters and combiners).
    Right,
}

/// Side of `port_index` for a node with `input_count` inputs.
///
/// Port 0 is always the top pipe. Two-input filters put their mask on the
/// right, while three-or-more-input combiners put `A` on the left and every
/// later pipe (the mask) on the right.
pub fn input_port_side(kind: &str, input_count: usize, port_index: usize) -> PortSide {
    if kind == "Switch" {
        // Input 0 is the top `B` pipe; input 1 enters from the left `A` side so
        // the selected input reads as a swap partner, not a mask.
        return if port_index == 1 {
            PortSide::Left
        } else {
            PortSide::Top
        };
    }
    if port_index == 0 || input_count <= 1 {
        return PortSide::Top;
    }
    if input_count == 2 {
        return PortSide::Right;
    }
    // `Merge2` and any other combiner: B top, A left, M right.
    let combiner = kind == "Merge2" || input_count >= 3;
    if combiner && port_index == 1 {
        PortSide::Left
    } else {
        PortSide::Right
    }
}

/// Number of input ports a cartridge exposes to the canvas.
fn input_port_count(kind: &str) -> usize {
    match kind {
        "Constant" | "Read" | "BackdropNode" | "CheckerBoard2" | "Ramp" => 0,
        "Merge2" => 3,
        "Grade" | "Switch" | "Clamp" | "Saturation" | "Glow" | "Defocus" => 2,
        _ => 1,
    }
}

/// `true` when `kind` is the `Dot` routing elbow.
pub fn is_dot_node(kind: &str) -> bool {
    kind == "Dot"
}

/// `true` when `kind` is the `BackdropNode` canvas container.
pub fn is_backdrop_node(kind: &str) -> bool {
    kind == "BackdropNode"
}

/// The canvas-space size of a node card: `22x22` for `Dot`, knob-driven for
/// `BackdropNode`, and `NODE_W x NODE_H` for everything else.
pub fn node_world_size(node: &CanvasNode) -> Vec2 {
    if is_dot_node(&node.kind) {
        return DOT_SIZE;
    }
    if is_backdrop_node(&node.kind) {
        let width = node
            .knobs
            .get("bdwidth")
            .and_then(|v| match v {
                KnobValue::Float(v) => Some(*v as f32),
                KnobValue::Int(v) => Some(*v as f32),
                _ => None,
            })
            .unwrap_or(320.0)
            .max(140.0);
        let height = node
            .knobs
            .get("bdheight")
            .and_then(|v| match v {
                KnobValue::Float(v) => Some(*v as f32),
                KnobValue::Int(v) => Some(*v as f32),
                _ => None,
            })
            .unwrap_or(200.0)
            .max(90.0);
        return Vec2::new(width, height);
    }
    NODE_SIZE
}

/// Computes the uniform minimap transform for mapping world space into
/// `minimap_rect`.
///
/// The bounds are derived **strictly from `mirror.nodes`** (using each node's
/// actual world rect, including `BackdropNode` and `Dot` sizes), expanded by
/// fixed padding (25% of span + 180.0 world units per side, minimum span
/// 600x400). The viewport world rect is never included — this prevents the
/// feedback loop where panning far away shrinks nodes into specks and dragging
/// inside the minimap rescales under the cursor.
///
/// Returns `(world_min, world_max, scale, offset)` where `scale` is the
/// uniform aspect-preserving scale and `offset` centers the scaled bounds
/// inside `minimap_rect`. The minimap mapping is
/// `minimap_pos = minimap_rect.min + offset + (world_pos - world_min) * scale`.
fn minimap_world_bounds(
    graph: &GraphMirror,
    state: &NodeCanvasState,
    minimap_rect: Rect,
) -> Option<(Pos2, Pos2, f32, Vec2)> {
    let mut world_min = Pos2::new(f32::INFINITY, f32::INFINITY);
    let mut world_max = Pos2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    for node in graph.nodes() {
        let Some(pos) = state.node_position(node.id) else {
            continue;
        };
        let size = node_world_size(node);
        let n_min = pos;
        let n_max = pos + size;
        world_min = world_min.min(n_min);
        world_max = world_max.max(n_max);
    }
    if !world_min.x.is_finite() {
        return None;
    }

    // Padding: 25% of span + 180 world units per side, with minimum span.
    let span = world_max - world_min;
    let pad_x = span.x.max(600.0) * 0.25 + 180.0;
    let pad_y = span.y.max(400.0) * 0.25 + 180.0;
    world_min -= egui::vec2(pad_x, pad_y);
    world_max += egui::vec2(pad_x, pad_y);

    let bounds_span = world_max - world_min;
    let bounds_span = bounds_span.max(Vec2::splat(1.0));

    let inner = minimap_rect.size();
    let scale = (inner.x / bounds_span.x).min(inner.y / bounds_span.y);
    let scaled_size = bounds_span * scale;
    let offset = (inner - scaled_size) / 2.0;

    Some((world_min, world_max, scale, offset))
}

fn node_accepts_inputs(graph: &GraphMirror, id: NodeId) -> bool {
    graph
        .node(id)
        .is_some_and(|node| input_port_count(&node.kind) > 0)
}

/// Pin anchor for input `port` on the correct edge of `rect`.
fn input_port(rect: Rect, kind: &str, count: usize, port: PortId) -> Pos2 {
    if count == 0 {
        return rect.center_top();
    }
    match input_port_side(kind, count, port.0 as usize) {
        PortSide::Top => rect.center_top(),
        PortSide::Left => rect.left_center(),
        PortSide::Right => rect.right_center(),
    }
}

/// Tip of the stem drawn outward from an input pin.
fn input_port_stem_tip(pin: Pos2, side: PortSide, zoom: f32) -> Pos2 {
    let lead = PORT_LEAD * zoom;
    match side {
        PortSide::Top => pin - Vec2::new(0.0, lead),
        PortSide::Left => pin - Vec2::new(lead, 0.0),
        PortSide::Right => pin + Vec2::new(lead, 0.0),
    }
}

/// Anchor for an input port label, clear of the port stem.
///
/// The stem rises [`PORT_LEAD`] pixels above the node's top edge, so the label
/// Computes the label anchor sitting `(PORT_LEAD + 3.0) * zoom` above a pin,
/// keeping every glyph clear of the stem tip. Used by `PortSide::Top` labels
/// and directly tested by `port_labels_sit_above_the_stem_tip`.
#[allow(dead_code)]
fn port_label_position(position: Pos2, zoom: f32) -> Pos2 {
    position - Vec2::new(0.0, (PORT_LEAD + 3.0) * zoom)
}

/// Label anchor for an input pin on `side`.
///
/// `PortSide::Top` labels shift left of the vertical stem so incoming
/// vertical wires pass cleanly to the right. `PortSide::Left` / `Right`
/// labels sit below the horizontal stem so curving wires from above clear
/// them.
fn input_port_label_position(pin: Pos2, side: PortSide, zoom: f32) -> Pos2 {
    match side {
        PortSide::Top => Pos2::new(pin.x - 6.0 * zoom, pin.y - (PORT_LEAD + 3.0) * zoom),
        PortSide::Left => pin - Vec2::new((PORT_LEAD + 2.0) * zoom, -5.0 * zoom),
        PortSide::Right => pin + Vec2::new((PORT_LEAD + 2.0) * zoom, 5.0 * zoom),
    }
}

/// Text alignment that keeps a side-aware port label clear of its stem.
fn input_port_label_align(side: PortSide) -> Align2 {
    match side {
        PortSide::Top => Align2::RIGHT_BOTTOM,
        PortSide::Left => Align2::RIGHT_TOP,
        PortSide::Right => Align2::LEFT_TOP,
    }
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
        // Backdrops are hit-tested separately, and only on their header or
        // resize handle, so a card sitting inside one stays clickable.
        .filter(|node| !is_backdrop_node(&node.kind))
        .find(|node| node_screen_rect(state, node.id, rect, graph).contains(pointer))
        .map(|node| node.id)
}

/// Circular body of a `Dot` under the pointer, if any.
///
/// The `Dot` is deliberately compact, so its entire body is a drag handle.
/// Wire drags only start from its stem tips (above `rect.top()` / below
/// `rect.bottom()`), which keeps a body click from being hijacked by the
/// generously sized port hit-test.
fn hit_dot_body(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    pointer: Pos2,
    rect: Rect,
) -> Option<NodeId> {
    graph
        .nodes()
        .iter()
        .rev()
        .filter(|node| is_dot_node(&node.kind))
        .find(|node| {
            let node_rect = node_screen_rect(state, node.id, rect, graph);
            pointer.distance(node_rect.center()) <= node_rect.width() * 0.5
        })
        .map(|node| node.id)
}

/// Screen rect of a backdrop's draggable top header bar.
fn backdrop_header_rect(rect: Rect, zoom: f32) -> Rect {
    let header_h = (24.0 * zoom).clamp(12.0, 24.0);
    Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.min.y + header_h))
}

/// Screen rect of a backdrop's bottom-right resize grip.
fn backdrop_resize_handle_rect(rect: Rect, zoom: f32) -> Rect {
    let grip = 16.0 * zoom;
    Rect::from_min_size(rect.right_bottom() - Vec2::splat(grip), Vec2::splat(grip))
}

/// Topmost `BackdropNode` whose header bar contains `pointer`.
fn hit_backdrop_header(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    pointer: Pos2,
    rect: Rect,
) -> Option<NodeId> {
    graph
        .nodes()
        .iter()
        .rev()
        .filter(|node| is_backdrop_node(&node.kind))
        .find(|node| {
            let node_rect = node_screen_rect(state, node.id, rect, graph);
            backdrop_header_rect(node_rect, state.zoom).contains(pointer)
        })
        .map(|node| node.id)
}

/// Topmost `BackdropNode` whose bottom-right resize grip contains `pointer`.
fn hit_backdrop_resize_handle(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    pointer: Pos2,
    rect: Rect,
) -> Option<NodeId> {
    graph
        .nodes()
        .iter()
        .rev()
        .filter(|node| is_backdrop_node(&node.kind))
        .find(|node| {
            let node_rect = node_screen_rect(state, node.id, rect, graph);
            backdrop_resize_handle_rect(node_rect, state.zoom).contains(pointer)
        })
        .map(|node| node.id)
}

/// Non-backdrop nodes whose world-space center lies inside `backdrop`'s rect.
///
/// Used to move a backdrop's contents with it when its header is dragged.
fn backdrop_enclosed_nodes(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    backdrop: NodeId,
) -> Vec<NodeId> {
    let Some(node) = graph.node(backdrop) else {
        return Vec::new();
    };
    let Some(position) = state.node_position(backdrop) else {
        return Vec::new();
    };
    let bounds = Rect::from_min_size(position, node_world_size(node));
    graph
        .nodes()
        .iter()
        .filter(|candidate| !is_backdrop_node(&candidate.kind))
        .filter(|candidate| {
            state.node_position(candidate.id).is_some_and(|position| {
                let center = position + node_world_size(candidate) * 0.5;
                bounds.contains(center)
            })
        })
        .map(|candidate| candidate.id)
        .collect()
}

/// Latches the exact set of nodes a drag gesture will move.
///
/// Runs once on the mouse-down frame. It records every selected node, plus the
/// contents of every selected `BackdropNode` **as they are right now**. Nothing
/// may add to this set while the button is held, so sweeping a backdrop across
/// other cards can never vacuum them up.
fn latch_drag_start(state: &mut NodeCanvasState, graph: &GraphMirror) {
    state.drag_start_positions.clear();
    let selected: Vec<NodeId> = state.selected_nodes.iter().copied().collect();
    for id in &selected {
        if let Some(node) = graph.node(*id) {
            state.drag_start_positions.insert(*id, (node.x, node.y));
        }
    }
    // A selected backdrop drags the cards it encloses at mouse-down. Those
    // cards join the drag group but never the selection.
    for id in selected {
        let is_backdrop = graph
            .node(id)
            .is_some_and(|node| is_backdrop_node(&node.kind));
        if !is_backdrop {
            continue;
        }
        for enclosed in backdrop_enclosed_nodes(state, graph, id) {
            if let Some(node) = graph.node(enclosed) {
                state
                    .drag_start_positions
                    .entry(enclosed)
                    .or_insert((node.x, node.y));
            }
        }
    }
}

fn hit_input_port(
    state: &NodeCanvasState,
    graph: &GraphMirror,
    pointer: Pos2,
    rect: Rect,
) -> Option<(NodeId, PortId)> {
    let radius = port_hit_radius(state.zoom);
    for node in graph.nodes().iter().rev() {
        // A `BackdropNode` is a canvas container, never a wiring target.
        if is_backdrop_node(&node.kind) {
            continue;
        }
        let node_rect = node_screen_rect(state, node.id, rect, graph);
        let count = input_port_count(&node.kind);
        for port in 0..count {
            let pin = input_port(node_rect, &node.kind, count, PortId(port as u16));
            // A `Dot`'s compact body is a drag handle, so only its stem tip may
            // start a wire drag; every other cartridge keeps the pin anchor.
            let position = if is_dot_node(&node.kind) {
                input_port_stem_tip(pin, input_port_side(&node.kind, count, port), state.zoom)
            } else {
                pin
            };
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
        .filter(|node| !is_backdrop_node(&node.kind))
        .find(|node| {
            let node_rect = node_screen_rect(state, node.id, rect, graph);
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
    if kind == "Dot" {
        return "";
    }
    if kind == "Switch" {
        return match port_index {
            0 => "0",
            1 => "1",
            _ => "",
        };
    }
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

/// Draws a `Dot` routing elbow: a filled circle with a compact input stem above
/// and an output stem below, so wires visibly pass through it.
fn draw_dot(painter: &Painter, rect: Rect, selected: bool, zoom: f32, pal: &ThemePalette) {
    let center = rect.center();
    let radius = rect.width() * 0.5;
    // Soft drop shadow behind the circle.
    painter.circle_filled(
        center + Vec2::new(1.5 * zoom, 2.5 * zoom),
        radius,
        pal.card_shadow,
    );
    // Filled circle body.
    painter.circle_filled(center, radius, pal.pin_b);
    let rim = if selected {
        Stroke::new(2.0 * zoom, pal.selected_rim)
    } else {
        Stroke::new(1.0 * zoom, pal.card_border)
    };
    painter.circle_stroke(center, radius, rim);

    // Top input stem and pin, mirroring the output side so both wire anchors
    // sit clear of the circular body.
    let input = rect.center_top() - Vec2::new(0.0, PORT_LEAD * zoom);
    painter.line_segment(
        [rect.center_top(), input],
        Stroke::new((1.5 * zoom).max(0.6), pal.wire_default),
    );
    painter.circle_filled(input, 2.5 * zoom, pal.wire_default);
    // Bottom output dot, joined to the wire anchor by a short stem.
    let output = output_port(rect, zoom);
    painter.line_segment(
        [rect.center_bottom(), output],
        Stroke::new((1.5 * zoom).max(0.6), pal.wire_default),
    );
    painter.circle_filled(output, 2.5 * zoom, pal.wire_default);
}

/// Draws a `BackdropNode` container: a tinted fill with a solid header bar
/// showing the backdrop's label, plus a subtle resize grip in the bottom-right
/// corner. Drawn behind wires and regular nodes (3-layer z-order).
fn draw_backdrop(
    painter: &Painter,
    rect: Rect,
    node: &CanvasNode,
    zoom: f32,
    pal: &ThemePalette,
    selected: bool,
) {
    let color_name = node
        .knobs
        .get("color")
        .and_then(|v| match v {
            KnobValue::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .unwrap_or("slate");
    let tint = backdrop_tint_color(color_name);

    // Semi-transparent body fill.
    painter.rect_filled(
        rect,
        CornerRadius::same((6.0 * zoom).clamp(1.0, 12.0) as u8),
        Color32::from_rgba_unmultiplied(tint.r(), tint.g(), tint.b(), 48),
    );

    // Solid header bar at the top.
    let header_h = (24.0 * zoom).clamp(12.0, 24.0);
    let header_rect = Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.min.y + header_h));
    painter.rect_filled(
        header_rect,
        CornerRadius {
            nw: (6.0 * zoom).clamp(1.0, 12.0) as u8,
            ne: (6.0 * zoom).clamp(1.0, 12.0) as u8,
            sw: 0,
            se: 0,
        },
        Color32::from_rgba_unmultiplied(tint.r(), tint.g(), tint.b(), 210),
    );

    // Outline border.
    let border_width = if selected { 2.0 } else { 1.0 };
    let border_color = if selected {
        pal.selected_rim
    } else {
        Color32::from_black_alpha(120)
    };
    painter.rect_stroke(
        rect,
        CornerRadius::same((6.0 * zoom).clamp(1.0, 12.0) as u8),
        Stroke::new(border_width, border_color),
        StrokeKind::Inside,
    );

    // Backdrop label in bold white inside the header bar.
    let label = node
        .knobs
        .get("label")
        .and_then(|v| match v {
            KnobValue::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .unwrap_or("Backdrop");
    painter.text(
        header_rect.min + Vec2::new(10.0, header_h / 2.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional((11.0 * zoom).clamp(8.0, 14.0)),
        Color32::WHITE,
    );

    // Subtle diagonal resize grip in the bottom-right corner.
    let grip = 14.0 * zoom;
    let grip_rect = Rect::from_min_size(rect.right_bottom() - Vec2::splat(grip), Vec2::splat(grip));
    let grip_stroke = Stroke::new((1.0 * zoom).max(0.5), Color32::from_black_alpha(140));
    for step in 1..=3 {
        let offset = grip * step as f32 / 4.0;
        painter.line_segment(
            [
                Pos2::new(grip_rect.right() - offset, grip_rect.bottom()),
                Pos2::new(grip_rect.right(), grip_rect.bottom() - offset),
            ],
            grip_stroke,
        );
    }
}

fn draw_grid(painter: &Painter, rect: Rect, pan: Vec2, zoom: f32, pal: &ThemePalette) {
    painter.rect_filled(rect, 0.0, pal.canvas_bg);

    let spacing = (GRID_SPACING * zoom).max(18.0);
    let origin = rect.min + pan;

    if pal.use_dot_grid {
        // Fusion: dot grid at intersections.
        let first_x = ((rect.left() - origin.x) / spacing).floor() as i64;
        let last_x = ((rect.right() - origin.x) / spacing).ceil() as i64;
        let first_y = ((rect.top() - origin.y) / spacing).floor() as i64;
        let last_y = ((rect.bottom() - origin.y) / spacing).ceil() as i64;
        for x in first_x..=last_x {
            for y in first_y..=last_y {
                let pos = Pos2::new(origin.x + x as f32 * spacing, origin.y + y as f32 * spacing);
                if rect.contains(pos) {
                    painter.circle_filled(pos, 1.5, pal.grid_line);
                }
            }
        }
    } else {
        // Nuke / Natron: line grid.
        let first_x = ((rect.left() - origin.x) / spacing).floor() as i64;
        let last_x = ((rect.right() - origin.x) / spacing).ceil() as i64;
        for index in first_x..=last_x {
            let x = origin.x + index as f32 * spacing;
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(1.0, pal.grid_line),
            );
        }

        let first_y = ((rect.top() - origin.y) / spacing).floor() as i64;
        let last_y = ((rect.bottom() - origin.y) / spacing).ceil() as i64;
        for index in first_y..=last_y {
            let y = origin.y + index as f32 * spacing;
            painter.line_segment(
                [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
                Stroke::new(1.0, pal.grid_line),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_node(
    painter: &Painter,
    rect: Rect,
    node: &CanvasNode,
    selected: bool,
    viewer_slot_label: Option<&'static str>,
    zoom: f32,
    pal: &ThemePalette,
    active_port: Option<PortId>,
    perf: Option<(u64, bool)>,
) {
    let crashed = node.crashed.is_some();
    let disabled = node.disabled;
    let cat = node_category(&node.kind);

    // Scaled corner radius, clamped to stay sensible at extreme zoom levels.
    let r = (pal.card_corner_radius as f32 * zoom)
        .round()
        .clamp(1.0, 12.0) as u8;
    let full_rounding = CornerRadius::same(r);
    let top_only_rounding = CornerRadius {
        nw: r,
        ne: r,
        sw: 0,
        se: 0,
    };

    // Body fill: full-body category fill for Classic, otherwise dark body.
    let body_fill = if crashed {
        Color32::from_rgb(58, 22, 24)
    } else if pal.full_body_category_fill && !disabled {
        pal.category_color(cat)
    } else if disabled {
        pal.card_body_bg.linear_multiply(0.7)
    } else {
        pal.card_body_bg
    };

    // 1. Drop shadow behind the card.
    painter.rect_filled(
        rect.translate(Vec2::new(3.0 * zoom, 4.0 * zoom)),
        full_rounding,
        pal.card_shadow,
    );

    // 2. Card base body.
    painter.rect_filled(rect, full_rounding, body_fill);

    // Classic full-body cards: a slim dark footer strip at the bottom 32%
    // behind #id and VIEWER badges so both the category color and badges
    // read with high contrast.
    if pal.full_body_category_fill && !crashed {
        let footer_h = rect.height() * 0.32;
        let footer_rect =
            Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - footer_h), rect.max);
        painter.rect_filled(footer_rect, 0.0, Color32::from_black_alpha(110));
    }

    // 3. Category header band: top-only rounding so the bottom edge sits flat.
    let header_h = (rect.height() * 0.44).clamp(14.0 * zoom, 26.0 * zoom);
    let header_rect = Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.min.y + header_h));

    let header_color = if crashed {
        Color32::from_rgb(230, 70, 70) // bright Airbag red
    } else if disabled {
        pal.category_color(cat).linear_multiply(0.7)
    } else {
        pal.category_color(cat)
    };

    painter.rect_filled(header_rect, top_only_rounding, header_color);

    // 1 px dark separator along the header bottom edge.
    painter.line_segment(
        [
            Pos2::new(rect.left(), header_rect.bottom()),
            Pos2::new(rect.right(), header_rect.bottom()),
        ],
        Stroke::new(1.0, Color32::from_black_alpha(90)),
    );

    // Input ports: role-colored pins by PortSide.
    let count = input_port_count(&node.kind);
    let box_size = (9.0 * zoom).max(5.0);
    for port in 0..count {
        let port_id = PortId(port as u16);
        let side = input_port_side(&node.kind, count, port);
        let position = input_port(rect, &node.kind, count, port_id);
        let stem_tip = input_port_stem_tip(position, side, zoom);

        let pin_color = match side {
            PortSide::Top => pal.pin_b,
            PortSide::Left => pal.pin_a,
            PortSide::Right => pal.pin_m,
        };

        let stub = Stroke::new((1.5 * zoom).max(0.6), pin_color);
        painter.line_segment([stem_tip, position], stub);
        let active = active_port == Some(port_id);
        let port_rect = Rect::from_center_size(position, Vec2::splat(box_size));
        painter.rect_filled(
            port_rect,
            CornerRadius::same(1),
            if active { WIRE_ACTIVE } else { pin_color },
        );
        let label = input_port_label(&node.kind, count, port);
        if !label.is_empty() {
            painter.text(
                input_port_label_position(position, side, zoom),
                input_port_label_align(side),
                label,
                FontId::proportional((9.0 * zoom).clamp(7.0, 11.0)),
                Color32::from_gray(200),
            );
        }
    }

    // Output port at bottom center.
    let output = output_port(rect, zoom);
    let stub = Stroke::new((1.5 * zoom).max(0.6), pal.card_border);
    painter.line_segment([rect.center_bottom(), output], stub);
    painter.circle_filled(output, (4.0 * zoom).max(2.5), pal.card_border);

    // 5. Crisp shadowed typography: 1 px dark drop-shadow behind bright white kind text.
    let header_font = FontId::proportional((13.0 * zoom).clamp(9.0, 17.0));
    let kind_pos = rect.left_top() + Vec2::new(8.0 * zoom, 4.0 * zoom);
    painter.text(
        kind_pos + Vec2::new(1.0, 1.0),
        Align2::LEFT_TOP,
        node.kind.as_str(),
        header_font.clone(),
        Color32::from_black_alpha(160),
    );
    painter.text(
        kind_pos,
        Align2::LEFT_TOP,
        node.kind.as_str(),
        header_font,
        Color32::WHITE,
    );

    let body_font = FontId::proportional((12.0 * zoom).clamp(9.0, 16.0));
    painter.text(
        rect.left_bottom() + Vec2::new(8.0 * zoom, -6.0 * zoom),
        Align2::LEFT_BOTTOM,
        format!("#{}", node.id.0),
        body_font,
        if disabled {
            Color32::from_gray(130)
        } else {
            Color32::from_gray(190)
        },
    );

    // Per-node evaluation heat bar: 2.5 px along the bottom interior edge, plus
    // a compact timing label beside `#id` when zoomed in (>= 0.75). The label
    // stays on the left so it never sits under the `VIEWER 1/2` pill in the
    // bottom-right corner.
    if let Some((eval_us, from_cache)) = perf {
        let eval_ms = eval_us as f64 / 1000.0;
        let heat_color = if from_cache || eval_ms < 5.0 {
            Color32::from_rgb(70, 195, 115) // emerald green: cached or fast
        } else if eval_ms <= 25.0 {
            Color32::from_rgb(240, 180, 60) // amber
        } else {
            Color32::from_rgb(235, 85, 75) // coral red: slow
        };
        let bar_h = 2.5 * zoom;
        let bar_rect = Rect::from_min_max(
            Pos2::new(rect.left() + 2.0 * zoom, rect.bottom() - 2.0 * zoom - bar_h),
            Pos2::new(rect.right() - 2.0 * zoom, rect.bottom() - 2.0 * zoom),
        );
        painter.rect_filled(bar_rect, 0, heat_color);

        if zoom >= 0.75 {
            let text = if from_cache {
                "C".to_string()
            } else {
                format!("{:.1}ms", eval_ms)
            };
            painter.text(
                rect.left_bottom() + Vec2::new(48.0 * zoom, -6.0 * zoom),
                Align2::LEFT_BOTTOM,
                text,
                FontId::proportional((8.0 * zoom).clamp(7.0, 10.0)),
                heat_color,
            );
        }
    }

    if crashed {
        painter.text(
            rect.right_top() + Vec2::new(-7.0 * zoom, 5.0 * zoom),
            Align2::RIGHT_TOP,
            "CRASHED",
            FontId::proportional((10.0 * zoom).clamp(8.0, 13.0)),
            Color32::from_rgb(255, 130, 130),
        );
    }

    // 4. Card border & active selection rim on `rect` with full_rounding.
    let (border_color, border_width) = if crashed {
        (Color32::from_rgb(230, 70, 70), 2.0)
    } else if selected {
        (pal.selected_rim, 2.0)
    } else {
        (pal.card_border, 1.0)
    };
    painter.rect_stroke(
        rect,
        full_rounding,
        Stroke::new(border_width, border_color),
        StrokeKind::Inside,
    );

    // Additional outside selection rim for high visibility.
    if selected {
        painter.rect_stroke(
            rect.expand(2.0),
            full_rounding,
            Stroke::new(2.0, pal.selected_rim),
            StrokeKind::Outside,
        );
    }

    // Nuke's bypass marker: a bold diagonal `X` across the whole card.
    if disabled {
        let stroke = Stroke::new(2.0 * zoom, BYPASS_X);
        painter.line_segment([rect.left_top(), rect.right_bottom()], stroke);
        painter.line_segment([rect.left_bottom(), rect.right_top()], stroke);
    }

    if let Some(label) = viewer_slot_label {
        painter.rect_stroke(
            rect.expand(4.0),
            full_rounding,
            Stroke::new(2.0, pal.viewer_badge_accent),
            StrokeKind::Outside,
        );
        let tag = Rect::from_min_size(
            rect.right_bottom() + Vec2::new(-58.0 * zoom, -16.0 * zoom),
            Vec2::new(54.0 * zoom, 13.0 * zoom),
        );
        painter.rect_filled(tag, CornerRadius::same(2), pal.viewer_badge_accent);
        painter.text(
            tag.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional((9.0 * zoom).clamp(7.0, 11.0)),
            Color32::from_rgb(30, 24, 8),
        );
    }
}

/// Renders the pure-ASCII per-node timing tooltip near the pointer.
///
/// `interactable(false)` keeps the tooltip completely out of egui's input
/// chain, so hovering a node never blocks clicks, drags, or context menus.
fn draw_node_perf_tooltip(ui: &Ui, pointer: Pos2, text: &str) {
    egui::Area::new(egui::Id::unique("tanova_node_perf_tooltip"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .fixed_pos(pointer + Vec2::new(14.0, 14.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(18, 20, 26, 245))
                .stroke(Stroke::new(1.0, Color32::from_rgb(70, 78, 92)))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(7, 4))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(text)
                            .font(FontId::proportional(11.0))
                            .color(Color32::from_gray(220)),
                    );
                });
        });
}

fn draw_wire(
    painter: &Painter,
    from: Pos2,
    to: Pos2,
    to_side: PortSide,
    zoom: f32,
    color: Color32,
    label: Option<&str>,
) {
    let points = wire_curve(from, to, to_side, zoom);
    painter.add(Shape::CubicBezier(CubicBezierShape::from_points_stroke(
        points,
        false,
        Color32::TRANSPARENT,
        Stroke::new((1.8 * zoom).max(0.8), color),
    )));

    let head = 6.0 * zoom.max(0.8);
    let arrow = match to_side {
        PortSide::Top => vec![
            to - Vec2::new(0.0, head * 0.5),
            to + Vec2::new(-head * 0.6, -head * 1.5),
            to + Vec2::new(head * 0.6, -head * 1.5),
        ],
        PortSide::Left => vec![
            to - Vec2::new(head * 0.5, 0.0),
            to + Vec2::new(-head * 1.5, -head * 0.6),
            to + Vec2::new(-head * 1.5, head * 0.6),
        ],
        PortSide::Right => vec![
            to + Vec2::new(head * 0.5, 0.0),
            to + Vec2::new(head * 1.5, -head * 0.6),
            to + Vec2::new(head * 1.5, head * 0.6),
        ],
    };
    painter.add(Shape::convex_polygon(arrow, color, Stroke::NONE));

    if let Some(label) = label {
        let (anchor, align) = match to_side {
            PortSide::Top => (to + Vec2::new(8.0, 2.0), Align2::LEFT_TOP),
            PortSide::Left => (to - Vec2::new(8.0, 0.0), Align2::RIGHT_CENTER),
            PortSide::Right => (to + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER),
        };
        painter.text(
            anchor,
            align,
            label,
            FontId::proportional((11.0 * zoom).clamp(9.0, 14.0)),
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::NodeCategory;

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

        let original_knobs = graph.node(NodeId(2)).unwrap().knobs.clone();
        let command = graph.removal_command(NodeId(2)).expect("snapshot");
        match &command {
            UndoableCommand::RemoveNode { knobs, edges, .. } => {
                assert_eq!(knobs.as_ref(), &original_knobs);
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
        assert_eq!(graph.node(NodeId(2)).unwrap().knobs, original_knobs);
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
    fn input_ports_follow_side_aware_geometry() {
        let rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(120.0, 72.0));

        assert_eq!(input_port(rect, "Blur", 1, PortId(0)), rect.center_top());
        assert_eq!(input_port(rect, "Grade", 2, PortId(0)), rect.center_top());
        assert_eq!(input_port(rect, "Grade", 2, PortId(1)), rect.right_center());
        assert_eq!(input_port(rect, "Merge2", 3, PortId(0)), rect.center_top());
        assert_eq!(input_port(rect, "Merge2", 3, PortId(1)), rect.left_center());
        assert_eq!(
            input_port(rect, "Merge2", 3, PortId(2)),
            rect.right_center()
        );
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
        // The Phase 6 color/alpha cartridges are offered to the artist.
        assert!(nuke.contains(&"ColorCorrect"));
        assert!(nuke.contains(&"Invert"));
        assert!(nuke.contains(&"Premult"));
        assert!(nuke.contains(&"Unpremult"));

        let fusion = spawnable_kinds(ViewerWorkflow::Fusion);
        assert!(!fusion.contains(&"Dummy"));
        assert!(
            !fusion.contains(&"Viewer"),
            "Fusion has no physical Viewer node"
        );
    }

    #[test]
    fn phase_six_color_alpha_cartridges_are_spawnable_and_color_category() {
        let kinds = ["ColorCorrect", "Invert", "Premult", "Unpremult"];
        for kind in kinds {
            assert!(
                spawnable_kinds(ViewerWorkflow::Nuke).contains(&kind),
                "{kind} must be offered in the Nuke spawn list"
            );
            assert!(
                spawnable_kinds(ViewerWorkflow::Fusion).contains(&kind),
                "{kind} must be offered in the Fusion spawn list"
            );
            assert_eq!(
                node_category(kind),
                NodeCategory::Color,
                "{kind} must map to NodeCategory::Color"
            );
        }
    }

    #[test]
    fn milestone_twenty_one_cartridges_are_spawnable_with_expected_ports() {
        let expected = [
            ("Clamp", 2, NodeCategory::Color),
            ("Saturation", 2, NodeCategory::Color),
            ("Crop", 1, NodeCategory::Transform),
            ("Switch", 2, NodeCategory::Merge),
            ("TimeOffset", 1, NodeCategory::Filter),
            ("FrameHold", 1, NodeCategory::Filter),
        ];
        for (kind, ports, category) in expected {
            assert!(
                spawnable_kinds(ViewerWorkflow::Nuke).contains(&kind),
                "{kind} must be offered in the Nuke spawn list"
            );
            assert!(
                spawnable_kinds(ViewerWorkflow::Fusion).contains(&kind),
                "{kind} must be offered in the Fusion spawn list"
            );
            assert_eq!(input_port_count(kind), ports, "{kind} input port count");
            assert_eq!(node_category(kind), category, "{kind} category");
        }
        assert_eq!(
            input_port_label("Switch", 2, 0),
            "0",
            "Switch input 0 is labelled 0"
        );
        assert_eq!(
            input_port_label("Switch", 2, 1),
            "1",
            "Switch input 1 is labelled 1"
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
    fn nuke_keys_spawn_unconnected_sources_and_chain_processors_vertically() {
        assert_eq!(nuke_key_to_node_kind(Key::R), Some("Read"));
        assert_eq!(nuke_key_to_node_kind(Key::W), Some("Write"));
        assert_eq!(nuke_key_to_node_kind(Key::C), Some("Constant"));
        assert_eq!(nuke_key_to_node_kind(Key::G), Some("Grade"));
        assert_eq!(nuke_key_to_node_kind(Key::B), Some("Blur"));
        assert_eq!(nuke_key_to_node_kind(Key::T), Some("Transform"));
        assert_eq!(nuke_key_to_node_kind(Key::M), Some("Merge2"));
        assert_eq!(nuke_key_to_node_kind(Key::Q), None);

        let mut graph = GraphMirror::default();
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Constant".to_string(),
            x: 40.0,
            y: 40.0,
        });
        let mut state = NodeCanvasState::default();

        // No selection: a processor spawns unconnected at the cursor.
        match spawn_node_message(&graph, &mut state, "Grade", Pos2::new(300.0, 200.0)) {
            KernelMessage::Execute(UndoableCommand::AddNode { id, kind, x, y }) => {
                assert_eq!(id, NodeId(2));
                assert_eq!(kind, "Grade");
                assert_eq!((x, y), (300.0, 200.0));
            }
            other => panic!("expected an unconnected AddNode, got {other:?}"),
        }
        assert_eq!(state.primary_selected, Some(NodeId(2)));
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(2),
            kind: "Grade".to_string(),
            x: 300.0,
            y: 200.0,
        });

        // A selected processor chains below it at y + 90 in one atomic batch.
        match spawn_node_message(&graph, &mut state, "Blur", Pos2::new(0.0, 0.0)) {
            KernelMessage::Execute(UndoableCommand::Batch(commands)) => {
                assert_eq!(commands.len(), 2);
                match &commands[0] {
                    UndoableCommand::AddNode { id, kind, x, y } => {
                        assert_eq!(*id, NodeId(3));
                        assert_eq!(kind, "Blur");
                        assert_eq!((*x, *y), (300.0, 290.0));
                    }
                    other => panic!("expected AddNode first, got {other:?}"),
                }
                match &commands[1] {
                    UndoableCommand::ConnectEdge {
                        from_node,
                        from_port,
                        to_node,
                        to_port,
                    } => {
                        assert_eq!(*from_node, NodeId(2));
                        assert_eq!(*from_port, PortId(0));
                        assert_eq!(*to_node, NodeId(3));
                        assert_eq!(*to_port, PortId(0));
                    }
                    other => panic!("expected ConnectEdge second, got {other:?}"),
                }
            }
            other => panic!("expected an atomic AddNode + ConnectEdge batch, got {other:?}"),
        }
        assert_eq!(state.primary_selected, Some(NodeId(3)));
        for command in [
            UndoableCommand::AddNode {
                id: NodeId(3),
                kind: "Blur".to_string(),
                x: 300.0,
                y: 290.0,
            },
            UndoableCommand::ConnectEdge {
                from_node: NodeId(2),
                from_port: PortId(0),
                to_node: NodeId(3),
                to_port: PortId(0),
            },
        ] {
            graph.apply_command(&command);
        }

        // Sources never chain, even with a processor selected.
        match spawn_node_message(&graph, &mut state, "Constant", Pos2::new(500.0, 500.0)) {
            KernelMessage::Execute(UndoableCommand::AddNode { id, kind, x, y }) => {
                assert_eq!(id, NodeId(4));
                assert_eq!(kind, "Constant");
                assert_eq!((x, y), (500.0, 500.0));
            }
            other => panic!("expected an unconnected source AddNode, got {other:?}"),
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
            selected_snapshot: HashSet::new(),
            primary_snapshot: None,
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
    fn pin_geometry_places_b_top_a_left_and_mask_right() {
        assert_eq!(input_port_side("Merge2", 3, 0), PortSide::Top);
        assert_eq!(input_port_side("Merge2", 3, 1), PortSide::Left);
        assert_eq!(input_port_side("Merge2", 3, 2), PortSide::Right);
        assert_eq!(input_port_side("Grade", 2, 0), PortSide::Top);
        assert_eq!(input_port_side("Grade", 2, 1), PortSide::Right);

        let rect = Rect::from_min_size(Pos2::new(100.0, 200.0), Vec2::new(NODE_W, NODE_H));
        assert_eq!(input_port(rect, "Merge2", 3, PortId(0)), rect.center_top());
        assert_eq!(input_port(rect, "Merge2", 3, PortId(1)), rect.left_center());
        assert_eq!(
            input_port(rect, "Merge2", 3, PortId(2)),
            rect.right_center()
        );
        assert_eq!(input_port(rect, "Grade", 2, PortId(0)), rect.center_top());
        assert_eq!(input_port(rect, "Grade", 2, PortId(1)), rect.right_center());
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
                        messages = NodeCanvas::show(
                            ui,
                            &mut canvas,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
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
                        messages = NodeCanvas::show(
                            ui,
                            &mut canvas,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
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
                        let _ = NodeCanvas::show(
                            ui,
                            &mut canvas,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
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
                        let _ = NodeCanvas::show(
                            ui,
                            &mut canvas,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
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
                        let _ = NodeCanvas::show(
                            ui,
                            &mut canvas,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
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
                        let _ = NodeCanvas::show(
                            ui,
                            &mut canvas,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
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
                        let _ = NodeCanvas::show(
                            ui,
                            &mut canvas,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        assert!(!canvas.dragging_nodes, "drag should end on release");
    }

    /// A `Dot`'s compact body must drag the node freely. Only its stem tips may
    /// start a wire drag, so a body press can never be hijacked by the port
    /// hit-test.
    #[test]
    fn dot_body_drag_moves_dot_without_starting_wire_drag() {
        let ctx = egui::Context::default();
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Dot".to_string(),
            x: 200.0,
            y: 200.0,
        }));
        let mut canvas = NodeCanvasState::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));

        let _ = run_canvas_pass(&ctx, screen, vec![], &mut canvas, &graph);
        let rect = canvas
            .last_canvas_rect
            .expect("the canvas must be laid out");
        let center = Pos2::new(200.0, 200.0) + DOT_SIZE * 0.5;
        let press = canvas.world_to_screen(rect, center);
        let dragged = press + Vec2::new(60.0, 20.0);

        // Frame 1: press dead-center on the Dot body.
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
            &graph,
        );
        assert!(
            canvas.dragging_nodes,
            "a Dot body press must latch a node drag"
        );
        assert!(
            canvas.active_wire_drag.is_none(),
            "a Dot body press must not start a wire drag"
        );
        assert!(
            messages.is_empty(),
            "the press frame must not emit, got {messages:?}"
        );

        // Frame 2: drag by (+60, +20).
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerMoved(dragged)],
            &mut canvas,
            &graph,
        );
        assert!(
            canvas.active_wire_drag.is_none(),
            "dragging a Dot must never start a wire drag"
        );
        let mut moves = Vec::new();
        for message in &messages {
            if let KernelMessage::Execute(command) = message {
                collect_moves(command, &mut moves);
            }
        }
        assert!(
            moves.iter().any(|(id, _, _, to_x, to_y)| {
                *id == NodeId(1) && (*to_x - 260.0).abs() < 1e-3 && (*to_y - 220.0).abs() < 1e-3
            }),
            "the Dot must move by (+60, +20), got {moves:?}"
        );
    }

    /// Sweeping an empty backdrop across existing cards must never capture or
    /// move them: the enclosed set is latched once, at mouse-down.
    #[test]
    fn dragging_empty_backdrop_across_nodes_never_captures_or_moves_them() {
        let ctx = egui::Context::default();
        let mut graph = GraphMirror::default();
        graph.apply_event(&KernelEvent::Applied(add_node(1, 0.0, 0.0)));
        graph.apply_event(&KernelEvent::Applied(add_node(2, 0.0, 90.0)));
        graph.apply_event(&KernelEvent::Applied(UndoableCommand::Batch(vec![
            UndoableCommand::AddNode {
                id: NodeId(3),
                kind: "BackdropNode".to_string(),
                x: 300.0,
                y: 0.0,
            },
            UndoableCommand::SetKnob {
                node: NodeId(3),
                knob: "bdwidth".to_string(),
                value: KnobValue::Float(200.0),
                previous: KnobValue::Float(320.0),
            },
            UndoableCommand::SetKnob {
                node: NodeId(3),
                knob: "bdheight".to_string(),
                value: KnobValue::Float(140.0),
                previous: KnobValue::Float(200.0),
            },
        ])));

        let mut canvas = NodeCanvasState::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));
        let _ = run_canvas_pass(&ctx, screen, vec![], &mut canvas, &graph);
        let rect = canvas
            .last_canvas_rect
            .expect("the canvas must be laid out");

        // Grab the empty backdrop's header and sweep it over both cards.
        let press = canvas.world_to_screen(rect, Pos2::new(310.0, 8.0));
        let release = canvas.world_to_screen(rect, Pos2::new(10.0, 8.0));

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
            &graph,
        );
        assert!(
            canvas.dragging_nodes,
            "a backdrop header press must latch a drag"
        );
        assert!(
            messages.is_empty(),
            "the press frame must not emit, got {messages:?}"
        );

        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerMoved(release)],
            &mut canvas,
            &graph,
        );
        let mut moves = Vec::new();
        for message in &messages {
            if let KernelMessage::Execute(command) = message {
                collect_moves(command, &mut moves);
            }
        }
        assert!(
            moves.iter().all(|(id, _, _, _, _)| *id == NodeId(3)),
            "only the backdrop may move, got {moves:?}"
        );
        assert!(
            moves
                .iter()
                .any(|(id, _, _, to_x, _)| *id == NodeId(3) && to_x.abs() < 1.0),
            "the backdrop must follow the pointer to x = 0, got {moves:?}"
        );
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
                        let _ = NodeCanvas::show(
                            ui,
                            &mut canvas,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
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
                                false,
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
                                false,
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
                        messages =
                            NodeCanvas::show(ui, canvas, graph, None, ViewerWorkflow::Nuke, false);
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

    #[test]
    fn shift_x_swaps_merge2_inputs_atomically() {
        let mut graph = GraphMirror::default();
        for (id, kind, y) in [
            (1u64, "Constant", 0.0f32),
            (2, "Grade", 120.0),
            (3, "Merge2", 240.0),
        ] {
            graph.apply_event(&KernelEvent::Applied(UndoableCommand::AddNode {
                id: NodeId(id),
                kind: kind.to_string(),
                x: 0.0,
                y,
            }));
        }
        // A = #1 on PortId(0), B = #2 on PortId(1).
        graph.apply_event(&KernelEvent::Applied(UndoableCommand::ConnectEdge {
            from_node: NodeId(1),
            from_port: PortId(0),
            to_node: NodeId(3),
            to_port: PortId(0),
        }));
        graph.apply_event(&KernelEvent::Applied(UndoableCommand::ConnectEdge {
            from_node: NodeId(2),
            from_port: PortId(0),
            to_node: NodeId(3),
            to_port: PortId(1),
        }));

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));
        let mut canvas = NodeCanvasState::default();
        canvas.selected_nodes.insert(NodeId(3));
        canvas.primary_selected = Some(NodeId(3));

        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::Key {
                key: Key::X,
                physical_key: Some(Key::X),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::SHIFT,
            }],
            &mut canvas,
            &graph,
        );

        let commands = messages
            .iter()
            .find_map(|message| match message {
                KernelMessage::Execute(UndoableCommand::Batch(commands)) => Some(commands),
                _ => None,
            })
            .expect("Shift+X must emit one atomic batch");

        assert_eq!(
            commands.len(),
            4,
            "both pipes must be disconnected and reconnected, got {commands:?}"
        );
        assert!(
            matches!(
                &commands[0],
                UndoableCommand::DisconnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                } if *from_node == NodeId(1) && *to_node == NodeId(3) && *to_port == PortId(0)
            ),
            "#1 must detach from PortId(0), got {:?}",
            commands[0]
        );
        assert!(
            matches!(
                &commands[1],
                UndoableCommand::DisconnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                } if *from_node == NodeId(2) && *to_node == NodeId(3) && *to_port == PortId(1)
            ),
            "#2 must detach from PortId(1), got {:?}",
            commands[1]
        );
        assert!(
            matches!(
                &commands[2],
                UndoableCommand::ConnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                } if *from_node == NodeId(1) && *to_node == NodeId(3) && *to_port == PortId(1)
            ),
            "#1 must reconnect on PortId(1), got {:?}",
            commands[2]
        );
        assert!(
            matches!(
                &commands[3],
                UndoableCommand::ConnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                } if *from_node == NodeId(2) && *to_node == NodeId(3) && *to_port == PortId(0)
            ),
            "#2 must reconnect on PortId(0), got {:?}",
            commands[3]
        );
    }

    #[test]
    fn dropping_unconnected_node_on_wire_splices_pipe_in_single_batch() {
        use tanova_kernel::{Dag, UndoStack};

        let ctx = egui::Context::default();
        let mut graph = GraphMirror::default();
        for (id, kind, x, y) in [
            (1u64, "Constant", 100.0f32, 100.0f32),
            (2, "Grade", 600.0, 100.0),
            (3, "Viewer", 300.0, 300.0),
        ] {
            graph.apply_event(&KernelEvent::Applied(UndoableCommand::AddNode {
                id: NodeId(id),
                kind: kind.to_string(),
                x,
                y,
            }));
        }
        graph.apply_event(&KernelEvent::Applied(UndoableCommand::ConnectEdge {
            from_node: NodeId(1),
            from_port: PortId(0),
            to_node: NodeId(3),
            to_port: PortId(0),
        }));

        let mut dag = Dag::new();
        dag.add_node_at(NodeId(1), "Constant", 100.0, 100.0)
            .unwrap();
        dag.add_node_at(NodeId(2), "Grade", 600.0, 100.0).unwrap();
        dag.add_node_at(NodeId(3), "Viewer", 300.0, 300.0).unwrap();
        dag.connect_edge(tanova_kernel::Edge::new(
            NodeId(1),
            PortId(0),
            NodeId(3),
            PortId(0),
        ))
        .unwrap();

        let mut canvas = NodeCanvasState::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0));
        let mut undo = UndoStack::new();
        let mut emitted: Vec<UndoableCommand> = Vec::new();

        let _ = run_canvas_pass(&ctx, screen, vec![], &mut canvas, &graph);
        let rect = canvas.last_canvas_rect.expect("canvas laid out");

        // Aim at the wire's midpoint between Constant and Viewer.
        let from_rect = node_screen_rect(&canvas, NodeId(1), rect, &graph);
        let to_rect = node_screen_rect(&canvas, NodeId(3), rect, &graph);
        let from = output_port(from_rect, canvas.zoom);
        let viewer_ports = input_port_count("Viewer");
        let to = input_port(to_rect, "Viewer", viewer_ports, PortId(0));
        let side = input_port_side("Viewer", viewer_ports, 0);
        let mid = cubic_bezier_point(wire_curve(from, to, side, canvas.zoom), 0.5);

        let press = node_screen_rect(&canvas, NodeId(2), rect, &graph).center();

        // Frame 1: grab Grade. Nothing is emitted on the press frame.
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
            &graph,
        );
        assert!(canvas.dragging_nodes, "press must latch the drag");
        assert!(messages.is_empty(), "the press frame emits no move");

        // Frame 2: drag Grade onto the wire.
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerMoved(mid)],
            &mut canvas,
            &graph,
        );
        assert!(!messages.is_empty(), "the moving frame must emit a move");
        feed_commands(&messages, &mut graph, &mut dag, &mut undo, &mut emitted);

        // Frame 3: release over the wire: one batch splices Grade in.
        let messages = run_canvas_pass(
            &ctx,
            screen,
            vec![egui::Event::PointerButton {
                pos: mid,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
            &mut canvas,
            &graph,
        );

        let commands = messages
            .iter()
            .find_map(|message| match message {
                KernelMessage::Execute(UndoableCommand::Batch(commands)) => Some(commands),
                _ => None,
            })
            .expect("dropping on a wire must emit one atomic batch");

        assert_eq!(
            commands.len(),
            4,
            "MoveNode + Disconnect + two Connects, got {commands:?}"
        );
        assert!(
            matches!(&commands[0], UndoableCommand::MoveNode { id, .. } if *id == NodeId(2)),
            "the batch must carry the drag's MoveNode first, got {:?}",
            commands[0]
        );
        assert!(
            matches!(
                &commands[1],
                UndoableCommand::DisconnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                } if *from_node == NodeId(1) && *to_node == NodeId(3) && *to_port == PortId(0)
            ),
            "the old Constant -> Viewer wire must be detached, got {:?}",
            commands[1]
        );
        assert!(
            matches!(
                &commands[2],
                UndoableCommand::ConnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                } if *from_node == NodeId(1) && *to_node == NodeId(2) && *to_port == PortId(0)
            ),
            "Constant must feed the dropped Grade, got {:?}",
            commands[2]
        );
        assert!(
            matches!(
                &commands[3],
                UndoableCommand::ConnectEdge {
                    from_node,
                    to_node,
                    to_port,
                    ..
                } if *from_node == NodeId(2) && *to_node == NodeId(3) && *to_port == PortId(0)
            ),
            "the dropped Grade must feed the Viewer, got {:?}",
            commands[3]
        );
    }

    #[test]
    fn minimap_click_pans_canvas_viewport_to_target_node() {
        use tanova_kernel::UndoableCommand;

        let mut graph = GraphMirror::default();
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Constant".to_string(),
            x: 0.0,
            y: 0.0,
        });
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(2),
            kind: "Grade".to_string(),
            x: 1000.0,
            y: 1000.0,
        });

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));
        let mut state = NodeCanvasState {
            show_minimap: true,
            ..Default::default()
        };

        // First pass to lay out nodes.
        let _ = run_canvas_pass(&ctx, screen, Vec::new(), &mut state, &graph);

        let pan_before = state.pan;

        // Click inside the minimap on the far node (Node 2 at 1000, 1000).
        // The minimap rect is in the bottom-right of the canvas rect.
        let canvas_rect = state.last_canvas_rect.unwrap_or(screen);
        let minimap_size = Vec2::new(170.0, 110.0);
        let margin = 8.0;
        let minimap_rect = Rect::from_min_size(
            Pos2::new(
                canvas_rect.max.x - minimap_size.x - margin,
                canvas_rect.max.y - minimap_size.y - margin,
            ),
            minimap_size,
        );
        // Click at the center of the minimap where Node 2 would project.
        let click_pos = minimap_rect.center();

        let events = vec![egui::Event::PointerButton {
            pos: click_pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        }];
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
                        messages = NodeCanvas::show(
                            ui,
                            &mut state,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
                    });
                });
            },
        )
        .drop_without_applying_deltas();

        // Pan should have shifted toward Node 2.
        assert!(
            (state.pan - pan_before).length() > 0.0,
            "minimap click must pan the canvas, pan was {:?} before and {:?} after",
            pan_before,
            state.pan
        );
        // No node-move commands should be emitted.
        let has_move = messages
            .iter()
            .any(|m| matches!(m, KernelMessage::Execute(UndoableCommand::MoveNode { .. })));
        assert!(!has_move, "minimap click must not emit node move commands");
    }

    #[test]
    fn minimap_click_centers_target_node_even_when_panned_far_away() {
        use tanova_kernel::UndoableCommand;

        let mut graph = GraphMirror::default();
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Constant".to_string(),
            x: 0.0,
            y: 0.0,
        });
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(2),
            kind: "Grade".to_string(),
            x: 1000.0,
            y: 1000.0,
        });

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));
        let mut state = NodeCanvasState {
            show_minimap: true,
            pan: Vec2::new(-5000.0, 4000.0),
            zoom: 0.6,
            ..Default::default()
        };

        // First pass to lay out nodes.
        let _ = run_canvas_pass(&ctx, screen, Vec::new(), &mut state, &graph);

        let canvas_rect = state.last_canvas_rect.unwrap_or(screen);
        let minimap_size = Vec2::new(170.0, 110.0);
        let margin = 8.0;
        let minimap_rect = Rect::from_min_size(
            Pos2::new(
                canvas_rect.max.x - minimap_size.x - margin,
                canvas_rect.max.y - minimap_size.y - margin,
            ),
            minimap_size,
        );

        // Click at Node 2's center minimap screen position using the same
        // transform as the painting pass.
        let Some((world_min, _world_max, scale, offset)) =
            minimap_world_bounds(&graph, &state, minimap_rect)
        else {
            panic!("minimap bounds should be computable");
        };
        // Node 2's center in world space.
        let node2_size = node_world_size(graph.node(NodeId(2)).unwrap());
        let node2_center = Pos2::new(1000.0, 1000.0) + node2_size * 0.5;
        let click_pos = minimap_rect.min + offset + (node2_center - world_min) * Vec2::splat(scale);

        let events = vec![egui::Event::PointerButton {
            pos: click_pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        }];
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
                        messages = NodeCanvas::show(
                            ui,
                            &mut state,
                            &graph,
                            None,
                            ViewerWorkflow::Nuke,
                            false,
                        );
                    });
                });
            },
        )
        .drop_without_applying_deltas();

        // Node 2's center should now be within 2.0 px of canvas center.
        let screen_pos = state.world_to_screen(canvas_rect, node2_center);
        let canvas_center = canvas_rect.center();
        assert!(
            (screen_pos - canvas_center).length() < 2.0,
            "node 2 center {:?} should be within 2.0px of canvas center {:?}, got distance {:?}",
            screen_pos,
            canvas_center,
            (screen_pos - canvas_center).length()
        );

        // No node-move commands should be emitted.
        let has_move = messages
            .iter()
            .any(|m| matches!(m, KernelMessage::Execute(UndoableCommand::MoveNode { .. })));
        assert!(!has_move, "minimap click must not emit node move commands");
    }

    #[test]
    fn minimap_drag_stays_stable_even_past_minimap_border() {
        use tanova_kernel::UndoableCommand;

        let mut graph = GraphMirror::default();
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Constant".to_string(),
            x: 0.0,
            y: 0.0,
        });
        graph.apply_command(&UndoableCommand::AddNode {
            id: NodeId(2),
            kind: "Grade".to_string(),
            x: 1000.0,
            y: 1000.0,
        });

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));
        let mut state = NodeCanvasState {
            show_minimap: true,
            ..Default::default()
        };

        // First pass to lay out nodes.
        let _ = run_canvas_pass(&ctx, screen, Vec::new(), &mut state, &graph);

        let canvas_rect = state.last_canvas_rect.unwrap_or(screen);
        let minimap_size = Vec2::new(170.0, 110.0);
        let margin = 8.0;
        let minimap_rect = Rect::from_min_size(
            Pos2::new(
                canvas_rect.max.x - minimap_size.x - margin,
                canvas_rect.max.y - minimap_size.y - margin,
            ),
            minimap_size,
        );

        // Drag across 3 points inside and past the minimap border.
        let drag_points = [
            minimap_rect.center(),
            Pos2::new(minimap_rect.min.x - 10.0, minimap_rect.center().y), // past left edge
            Pos2::new(minimap_rect.max.x + 10.0, minimap_rect.center().y), // past right edge
        ];

        let mut pan_history = Vec::new();
        for (i, &point) in drag_points.iter().enumerate() {
            let events = vec![egui::Event::PointerButton {
                pos: point,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }];
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
                            messages = NodeCanvas::show(
                                ui,
                                &mut state,
                                &graph,
                                None,
                                ViewerWorkflow::Nuke,
                                false,
                            );
                        });
                    });
                },
            )
            .drop_without_applying_deltas();

            pan_history.push(state.pan);

            // No marquee box-selection should start.
            assert!(
                state.box_selection.is_none(),
                "drag point {i} must not start box selection"
            );
            // No nodes should move.
            let has_move = messages
                .iter()
                .any(|m| matches!(m, KernelMessage::Execute(UndoableCommand::MoveNode { .. })));
            assert!(!has_move, "drag point {i} must not emit node move commands");
        }

        // Pan must stay finite throughout.
        for (i, &pan) in pan_history.iter().enumerate() {
            assert!(
                pan.is_finite(),
                "pan at drag point {i} must be finite, got {:?}",
                pan
            );
        }
    }

    #[test]
    fn phase_seven_cartridges_spawnable_with_expected_ports_and_2d_knobs() {
        use crate::theme::NodeCategory;
        use tanova_kernel::UndoableCommand;

        tanova_nodes::ensure_linked();
        let mut graph = GraphMirror::default();

        // Spawn each Phase 7 cartridge via AddNode through GraphMirror.
        for (kind, expected_ports, category, xy_rows) in [
            ("Ramp", 0, NodeCategory::Color, 2),
            ("Mirror", 1, NodeCategory::Transform, 0),
            ("CornerPin2D", 1, NodeCategory::Transform, 4),
            ("Defocus", 2, NodeCategory::Filter, 0),
            ("Glow", 2, NodeCategory::Filter, 0),
        ] {
            assert!(CONTEXT_MENU_KINDS.contains(&kind));
            assert!(spawnable_kinds(ViewerWorkflow::Nuke).contains(&kind));
            let id = graph.next_node_id();
            graph.apply_command(&UndoableCommand::AddNode {
                id,
                kind: kind.to_string(),
                x: 0.0,
                y: 0.0,
            });
            let node = graph.node(id).expect("{kind} must be in mirror");
            assert_eq!(
                input_port_count(&node.kind),
                expected_ports,
                "{kind} must expose {expected_ports} input port(s)"
            );
            assert_eq!(node_category(&node.kind), category, "{kind}");
            if expected_ports == 2 {
                assert_eq!(input_port_label(kind, 2, 0), "in");
                assert_eq!(input_port_label(kind, 2, 1), "M");
                assert_eq!(input_port_side(kind, 2, 0), PortSide::Top);
                assert_eq!(input_port_side(kind, 2, 1), PortSide::Right);
            }

            // Inspect real PropertiesPanel paint output rather than duplicating
            // its dispatch condition in the test.
            let ctx = egui::Context::default();
            let mut panel = crate::properties::PropertiesPanel::default();
            let output = ctx.run_pass(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 1600.0))),
                    ..Default::default()
                },
                |ctx| {
                    ctx.root_ui(|ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            assert!(panel.show(ui, &graph, Some(id), None).is_empty());
                        });
                    });
                },
            );
            fn collect_text(shape: &egui::Shape, text: &mut Vec<String>) {
                match shape {
                    egui::Shape::Text(s) => text.push(s.galley.text().to_owned()),
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            collect_text(shape, text);
                        }
                    }
                    _ => {}
                }
            }
            let mut labels = Vec::new();
            for shape in &output.shapes {
                collect_text(&shape.shape, &mut labels);
            }
            assert_eq!(
                labels.iter().filter(|text| *text == "X").count(),
                xy_rows,
                "{kind}: {labels:?}"
            );
            assert_eq!(
                labels.iter().filter(|text| *text == "Y").count(),
                xy_rows,
                "{kind}: {labels:?}"
            );
            if kind == "CornerPin2D" {
                assert!(
                    !labels
                        .iter()
                        .any(|text| ["R", "G", "B", "A"].contains(&text.as_str()))
                );
            }
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn format_stamps_match_kernel_and_survive_echoes() {
        tanova_nodes::ensure_linked();
        for (width, height) in [(256, 256), (1280, 720), (1920, 1080), (2048, 1556)] {
            let format = ProjectFormat { width, height };
            let mut dag = tanova_kernel::Dag::new();
            dag.set_project_format(Some(format));
            let mut graph = GraphMirror::default();
            graph.set_project_format(format);
            for kind in [
                "Constant",
                "CheckerBoard2",
                "Reformat",
                "Ramp",
                "Crop",
                "CornerPin2D",
            ] {
                let id = graph.next_node_id();
                let add = UndoableCommand::AddNode {
                    id,
                    kind: kind.into(),
                    x: 0.0,
                    y: 0.0,
                };
                dag.apply_command(add.clone()).unwrap();
                graph.apply_command(&add);
                assert_eq!(graph.node(id).unwrap().knobs, dag.node(id).unwrap().knobs);
                let w = width as f32;
                let h = height as f32;
                if matches!(kind, "Constant" | "CheckerBoard2" | "Reformat" | "Ramp") {
                    let knobs = &graph.node(id).unwrap().knobs;
                    assert_eq!(
                        knobs.get("format_width"),
                        Some(&KnobValue::Int(i64::from(width)))
                    );
                    assert_eq!(
                        knobs.get("format_height"),
                        Some(&KnobValue::Int(i64::from(height)))
                    );
                }
                let expected = match kind {
                    "Ramp" => vec![("p1", [w, h, 0.0, 0.0])],
                    "Crop" => vec![("box_max", [w, h, 0.0, 0.0])],
                    "CornerPin2D" => vec![
                        ("to2", [w, 0.0, 0.0, 0.0]),
                        ("to3", [w, h, 0.0, 0.0]),
                        ("to4", [0.0, h, 0.0, 0.0]),
                    ],
                    _ => continue,
                };
                for (name, value) in &expected {
                    assert_eq!(
                        graph.node(id).unwrap().knobs.get(name),
                        Some(&KnobValue::Color(*value))
                    );
                }
                let edit = UndoableCommand::SetKnob {
                    node: id,
                    knob: expected[0].0.into(),
                    value: KnobValue::Color([17.0, 23.0, 0.0, 0.0]),
                    previous: KnobValue::Color(expected[0].1),
                };
                dag.apply_command(edit.clone()).unwrap();
                graph.apply_command(&edit);
                graph.apply_command(&add);
                assert_eq!(graph.node(id).unwrap().knobs, dag.node(id).unwrap().knobs);
            }
        }
    }

    /// A `PerfSnapshot` only covers the branches it evaluated (a cached
    /// upstream node returns early), so folding must merge — never wipe — the
    /// timings already on the mirror. Deleting a node prunes only its entry.
    #[test]
    fn perf_snapshot_merges_across_renders_and_remove_prunes_the_entry() {
        use tanova_kernel::NodePerfSample;

        let mut graph = GraphMirror::default();
        for id in [1u64, 2, 3] {
            graph.apply_command(&UndoableCommand::AddNode {
                id: NodeId(id),
                kind: "Constant".to_string(),
                x: 0.0,
                y: id as f32 * 100.0,
            });
        }

        // First startup render evaluates the whole chain.
        graph.apply_event(&KernelEvent::PerfSnapshot(vec![
            NodePerfSample {
                node: NodeId(1),
                eval_us: 200,
                from_cache: false,
            },
            NodePerfSample {
                node: NodeId(2),
                eval_us: 44_000,
                from_cache: false,
            },
            NodePerfSample {
                node: NodeId(3),
                eval_us: 100,
                from_cache: false,
            },
        ]));
        assert_eq!(graph.node_perf.len(), 3);

        // A later render finds an upstream packet cached and only samples the
        // top node; the earlier bars must survive.
        graph.apply_event(&KernelEvent::PerfSnapshot(vec![NodePerfSample {
            node: NodeId(3),
            eval_us: 50,
            from_cache: true,
        }]));
        assert_eq!(
            graph.node_perf.get(&NodeId(1)),
            Some(&(200, false)),
            "Constant #1 timing must survive a partial snapshot"
        );
        assert_eq!(
            graph.node_perf.get(&NodeId(2)),
            Some(&(44_000, false)),
            "Grade #2 timing must survive a partial snapshot"
        );
        assert_eq!(graph.node_perf.get(&NodeId(3)), Some(&(50, true)));

        // Removing a node prunes only its own timing.
        graph.apply_command(&UndoableCommand::RemoveNode {
            id: NodeId(2),
            kind: "Constant".to_string(),
            x: 0.0,
            y: 200.0,
            disabled: false,
            knobs: Box::default(),
            edges: Vec::new(),
        });
        assert!(!graph.node_perf.contains_key(&NodeId(2)));
        assert!(graph.node_perf.contains_key(&NodeId(1)));
        assert!(graph.node_perf.contains_key(&NodeId(3)));
    }
}
