//! DAG topology, operation hashing, RAM cache, and lazy-pull evaluation.
//!
//! Nothing is rendered until a viewer asks for a node. Pulling that node
//! recursively pulls its inputs, hashes the whole upstream chain, and consults
//! the cache before any cartridge runs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::bus::{NodeId, PortId, UndoableCommand};
use crate::command::{EdgeSnapshot, NodeSnapshot};
use crate::governor::{MemoryGovernor, RenderEpoch, evaluate_with_airbag};
use crate::knobs::{KnobTable, KnobValue, canonical_f64};
use crate::registry::find_cartridge;
use crate::types::{EvalContext, NodeError, NodeTrait, Packet};

/// A directed connection from an upstream output to a downstream input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Edge {
    /// Upstream node producing the data.
    pub from_node: NodeId,
    /// Output port on the upstream node.
    pub from_port: PortId,
    /// Downstream node consuming the data.
    pub to_node: NodeId,
    /// Input port on the downstream node.
    pub to_port: PortId,
}

impl Edge {
    /// Builds an edge.
    pub fn new(from_node: NodeId, from_port: PortId, to_node: NodeId, to_port: PortId) -> Self {
        Self {
            from_node,
            from_port,
            to_node,
            to_port,
        }
    }
}

/// Fallback cartridge for node kinds the registry does not know.
///
/// Proprietary Foundry nodes (and any other unregistered class) must not break
/// a script: they pass their `B` input (`PortId(0)`) straight through so the
/// rest of the graph still renders.
#[derive(Debug, Clone, Copy, Default)]
pub struct PassthroughNode;

impl NodeTrait for PassthroughNode {
    fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
        Ok(inputs.first().cloned().unwrap_or_default())
    }

    fn serialize_to_tcl(&self) -> String {
        "Passthrough {}".to_string()
    }
}

/// One node in the DAG, plus the live state the kernel tracks for it.
pub struct DagNode {
    /// Stable identity of this node.
    pub id: NodeId,
    /// Registered cartridge name (e.g. `Blur`).
    pub kind: String,
    /// User-facing parameters.
    pub knobs: KnobTable,
    /// The cartridge itself; `None` until a registry resolves `kind`.
    pub implementation: Option<Arc<dyn NodeTrait>>,
    /// Last panic message, if evaluation crashed this node.
    pub crashed: Option<String>,
    /// Canvas-space X position of the node.
    pub x: f32,
    /// Canvas-space Y position of the node.
    pub y: f32,
}

impl DagNode {
    /// Creates a node without an attached cartridge.
    pub fn new(id: NodeId, kind: impl Into<String>) -> Self {
        Self {
            id,
            kind: kind.into(),
            knobs: KnobTable::new(),
            implementation: None,
            crashed: None,
            x: 0.0,
            y: 0.0,
        }
    }

    /// Attaches a cartridge implementation.
    pub fn with_implementation(mut self, implementation: Arc<dyn NodeTrait>) -> Self {
        self.implementation = Some(implementation);
        self
    }
}

impl fmt::Debug for DagNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DagNode")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("knobs", &self.knobs)
            .field("has_implementation", &self.implementation.is_some())
            .field("crashed", &self.crashed)
            .field("x", &self.x)
            .field("y", &self.y)
            .finish()
    }
}

/// Content hash of a node's fully resolved operation.
///
/// Two pulls with the same op hash are guaranteed to produce the same packet,
/// which is what makes cache hits safe.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpHash([u8; 32]);

impl OpHash {
    /// Raw hash bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hexadecimal representation, handy for logs.
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;

        let mut text = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(text, "{byte:02x}");
        }
        text
    }
}

impl fmt::Debug for OpHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OpHash").field(&self.to_hex()).finish()
    }
}

impl fmt::Display for OpHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl From<blake3::Hash> for OpHash {
    fn from(hash: blake3::Hash) -> Self {
        Self(*hash.as_bytes())
    }
}

/// Bounded FIFO cache of evaluated packets, keyed by [`OpHash`].
#[derive(Debug)]
pub struct RamCache {
    entries: HashMap<OpHash, Arc<Packet>>,
    order: VecDeque<OpHash>,
    capacity: usize,
}

impl RamCache {
    /// Entries kept by [`RamCache::default`].
    pub const DEFAULT_CAPACITY: usize = 256;

    /// Creates a cache that evicts the oldest entry past `capacity` items.
    ///
    /// A capacity of zero disables caching.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    /// Looks up a cached packet.
    pub fn get(&self, hash: &OpHash) -> Option<Arc<Packet>> {
        self.entries.get(hash).cloned()
    }

    /// Stores a packet, evicting the oldest entry when over capacity.
    pub fn insert(&mut self, hash: OpHash, packet: Arc<Packet>) -> Option<Arc<Packet>> {
        if self.capacity == 0 {
            return None;
        }

        let previous = self.entries.insert(hash, packet);
        if previous.is_none() {
            self.order.push_back(hash);
            while self.entries.len() > self.capacity {
                self.evict_oldest();
            }
        }
        previous
    }

    /// Drops one entry by hash.
    pub fn remove(&mut self, hash: &OpHash) -> Option<Arc<Packet>> {
        let removed = self.entries.remove(hash);
        if removed.is_some() {
            self.order.retain(|queued| queued != hash);
        }
        removed
    }

    /// Returns `true` if the hash is cached.
    pub fn contains(&self, hash: &OpHash) -> bool {
        self.entries.contains_key(hash)
    }

    /// Number of cached packets.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured capacity in entries.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Drops every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn evict_oldest(&mut self) -> Option<Arc<Packet>> {
        while let Some(hash) = self.order.pop_front() {
            if let Some(packet) = self.entries.remove(&hash) {
                return Some(packet);
            }
        }
        None
    }
}

impl Default for RamCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

/// The kernel's graph: nodes, edges, and the packet cache.
#[derive(Debug)]
pub struct Dag {
    nodes: IndexMap<NodeId, DagNode>,
    edges: Vec<Edge>,
    cache: RamCache,
    /// Most recent op hash per node, used for targeted cache invalidation.
    node_hashes: HashMap<NodeId, OpHash>,
}

impl Dag {
    /// Creates an empty graph with the default cache capacity.
    pub fn new() -> Self {
        Self::with_cache_capacity(RamCache::DEFAULT_CAPACITY)
    }

    /// Creates an empty graph with a specific cache capacity.
    pub fn with_cache_capacity(capacity: usize) -> Self {
        Self {
            nodes: IndexMap::new(),
            edges: Vec::new(),
            cache: RamCache::new(capacity),
            node_hashes: HashMap::new(),
        }
    }

    /// Borrows a node.
    pub fn node(&self, id: NodeId) -> Option<&DagNode> {
        self.nodes.get(&id)
    }

    /// Mutably borrows a node.
    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut DagNode> {
        self.nodes.get_mut(&id)
    }

    /// Iterates nodes in insertion order.
    pub fn nodes(&self) -> impl Iterator<Item = &DagNode> + '_ {
        self.nodes.values()
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the graph currently holds `id`.
    pub fn contains_node(&self, id: NodeId) -> bool {
        self.nodes.contains_key(&id)
    }

    /// Next unused node id, used when the kernel has to create a node itself.
    pub fn next_node_id(&self) -> NodeId {
        NodeId(
            self.nodes
                .keys()
                .map(|id| id.0)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        )
    }

    /// Iterates edges in connection order.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> + '_ {
        self.edges.iter()
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Borrows the packet cache.
    pub fn cache(&self) -> &RamCache {
        &self.cache
    }

    /// Mutably borrows the packet cache.
    pub fn cache_mut(&mut self) -> &mut RamCache {
        &mut self.cache
    }

    /// Removes cached packets for `node_id` and every node downstream of it
    /// (reachable via outgoing edges). Upstream nodes (e.g. a `Constant` feeding
    /// a `Transform`) keep their cache entries intact, so a knob drag on a
    /// downstream node doesn't force upstream re-allocation.
    pub fn invalidate_downstream(&mut self, node_id: NodeId) {
        let mut downstream: Vec<NodeId> = Vec::new();
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut queue = vec![node_id];
        while let Some(current) = queue.pop() {
            if !visited.insert(current) {
                continue;
            }
            for edge in self.edges.iter() {
                if edge.from_node == current
                    && edge.to_node != current
                    && !visited.contains(&edge.to_node)
                {
                    downstream.push(edge.to_node);
                    queue.push(edge.to_node);
                }
            }
        }
        for node in downstream {
            if let Some(hash) = self.node_hashes.get(&node) {
                self.cache.remove(hash);
            }
        }
    }

    /// Serializes the whole graph back into Nuke `.nk` stack syntax.
    ///
    /// Nodes are emitted in topological order so every `push $N...` refers to
    /// an upstream node that is already on the stack. Disconnected ports are
    /// written as `push 0`, which the parser reads back as "no input".
    pub fn export_to_tcl(&self) -> String {
        use std::fmt::Write as _;

        let order = self.topological_order();
        let mut variables: HashMap<NodeId, usize> = HashMap::with_capacity(order.len());
        for (index, id) in order.iter().enumerate() {
            variables.insert(*id, index + 1);
        }

        let mut out = String::from("version 15.0 v5\n");
        for id in &order {
            let Some(node) = self.nodes.get(id) else {
                continue;
            };

            let mut incoming: Vec<Edge> = self
                .edges
                .iter()
                .filter(|edge| edge.to_node == *id)
                .copied()
                .collect();
            incoming.sort_by_key(|edge| edge.to_port.0);
            let total_inputs = incoming
                .last()
                .map_or(0, |edge| edge.to_port.0 as usize + 1);

            // Highest port first: the parser pops the top of the stack for
            // port 0, matching Nuke's reverse push order.
            for port in (0..total_inputs).rev() {
                match incoming.iter().find(|edge| edge.to_port.0 as usize == port) {
                    Some(edge) => {
                        let variable = variables.get(&edge.from_node).copied().unwrap_or(0);
                        let _ = writeln!(out, "push $N{variable}");
                    }
                    None => out.push_str("push 0\n"),
                }
            }

            let _ = writeln!(out, "{} {{", node.kind);
            let _ = writeln!(out, " inputs {total_inputs}");
            for (name, value) in node.knobs.iter() {
                let _ = writeln!(out, " {name} {}", format_knob_value(value));
            }
            out.push_str("}\n");

            if let Some(variable) = variables.get(id) {
                let _ = writeln!(out, "set N{variable} [stack 0]");
            }
        }
        out
    }

    /// Orders nodes so every node appears after all of its inputs.
    fn topological_order(&self) -> Vec<NodeId> {
        let mut indegree: HashMap<NodeId, usize> = self.nodes.keys().map(|id| (*id, 0)).collect();
        for edge in &self.edges {
            if let Some(degree) = indegree.get_mut(&edge.to_node) {
                *degree += 1;
            }
        }

        let mut queue: VecDeque<NodeId> = self
            .nodes
            .keys()
            .filter(|id| indegree.get(id).copied().unwrap_or(0) == 0)
            .copied()
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());

        while let Some(id) = queue.pop_front() {
            order.push(id);
            for edge in self.edges.iter().filter(|edge| edge.from_node == id) {
                if let Some(degree) = indegree.get_mut(&edge.to_node) {
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        queue.push_back(edge.to_node);
                    }
                }
            }
        }

        // A cycle should be impossible; keep the remaining nodes visible rather
        // than silently dropping them.
        if order.len() < self.nodes.len() {
            for id in self.nodes.keys() {
                if !order.contains(id) {
                    order.push(*id);
                }
            }
        }
        order
    }

    /// Adds a node, auto-wiring its cartridge from the registry when the kind
    /// is registered.
    ///
    /// Registered cartridges contribute their default knobs and factory
    /// implementation. Unknown kinds receive a [`PassthroughNode`] so scripts
    /// containing proprietary nodes still evaluate end-to-end.
    pub fn add_node(&mut self, id: NodeId, kind: impl Into<String>) -> Result<(), NodeError> {
        let kind = kind.into();
        let mut node = DagNode::new(id, kind.clone());
        match find_cartridge(&kind) {
            Some(descriptor) => {
                node.knobs = (descriptor.default_knobs)();
                node.implementation = Some((descriptor.factory)());
            }
            None => {
                node.implementation = Some(Arc::new(PassthroughNode));
            }
        }
        self.insert_node(node)
    }

    /// Adds a node and records its canvas position.
    pub fn add_node_at(
        &mut self,
        id: NodeId,
        kind: impl Into<String>,
        x: f32,
        y: f32,
    ) -> Result<(), NodeError> {
        self.add_node(id, kind)?;
        self.set_position(id, x, y)
    }

    /// Moves a node in canvas space.
    pub fn set_position(&mut self, id: NodeId, x: f32, y: f32) -> Result<(), NodeError> {
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or(NodeError::UnknownNode(id.0))?;
        node.x = x;
        node.y = y;
        Ok(())
    }

    /// Adds a node with its cartridge attached.
    pub fn add_node_with_implementation(
        &mut self,
        id: NodeId,
        kind: impl Into<String>,
        implementation: Arc<dyn NodeTrait>,
    ) -> Result<(), NodeError> {
        self.insert_node(DagNode::new(id, kind).with_implementation(implementation))
    }

    /// Inserts a fully built node, rejecting duplicate ids.
    pub fn insert_node(&mut self, node: DagNode) -> Result<(), NodeError> {
        if self.nodes.contains_key(&node.id) {
            return Err(NodeError::NodeAlreadyExists(node.id.0));
        }
        self.nodes.insert(node.id, node);
        Ok(())
    }

    /// Removes a node and every edge touching it.
    pub fn remove_node(&mut self, id: NodeId) -> Option<DagNode> {
        let node = self.nodes.shift_remove(&id)?;
        self.edges
            .retain(|edge| edge.from_node != id && edge.to_node != id);
        Some(node)
    }

    /// Captures everything needed to recreate `id` exactly.
    ///
    /// The snapshot includes the full knob table and every incident edge, so
    /// undoing a removal never loses user edits or wiring.
    pub fn snapshot_node(&self, id: NodeId) -> Option<NodeSnapshot> {
        let node = self.nodes.get(&id)?;
        let edges = self
            .edges
            .iter()
            .filter(|edge| edge.from_node == id || edge.to_node == id)
            .map(|edge| {
                EdgeSnapshot::new(edge.from_node, edge.from_port, edge.to_node, edge.to_port)
            })
            .collect();

        Some(NodeSnapshot {
            id,
            kind: node.kind.clone(),
            x: node.x,
            y: node.y,
            knobs: node.knobs.clone(),
            edges,
        })
    }

    /// Builds the full-state [`UndoableCommand::RemoveNode`] for `id`.
    pub fn removal_command(&self, id: NodeId) -> Option<UndoableCommand> {
        let snapshot = self.snapshot_node(id)?;
        Some(UndoableCommand::RemoveNode {
            id: snapshot.id,
            kind: snapshot.kind,
            x: snapshot.x,
            y: snapshot.y,
            knobs: Box::new(snapshot.knobs),
            edges: snapshot.edges,
        })
    }

    /// Replaces a UI-built removal with the live snapshot from this graph.
    ///
    /// Other commands pass through untouched. If the node is already gone the
    /// command is returned as-is so the normal error path reports it.
    pub fn enrich(&self, command: UndoableCommand) -> UndoableCommand {
        if let UndoableCommand::RemoveNode { id, .. } = &command {
            if let Some(enriched) = self.removal_command(*id) {
                return enriched;
            }
        }
        command
    }

    /// Recreates a removed node with its exact knobs, position, and wiring.
    ///
    /// Edges are restored best-effort: when several connected nodes are undone
    /// one at a time, a peer may not exist yet. The edge is present in both
    /// endpoints' snapshots, so the later restore reconnects it.
    pub fn restore_node(
        &mut self,
        id: NodeId,
        kind: &str,
        x: f32,
        y: f32,
        knobs: &KnobTable,
        edges: &[EdgeSnapshot],
    ) -> Result<(), NodeError> {
        if self.nodes.contains_key(&id) {
            return Err(NodeError::NodeAlreadyExists(id.0));
        }

        let mut node = DagNode::new(id, kind);
        node.knobs = knobs.clone();
        node.x = x;
        node.y = y;
        node.implementation = Some(match find_cartridge(kind) {
            Some(descriptor) => (descriptor.factory)(),
            None => Arc::new(PassthroughNode) as Arc<dyn NodeTrait>,
        });
        self.insert_node(node)?;

        for edge in edges {
            let _ = self.connect_edge(Edge::new(
                edge.from_node,
                edge.from_port,
                edge.to_node,
                edge.to_port,
            ));
        }

        Ok(())
    }

    /// Attaches or replaces a node's cartridge.
    pub fn set_implementation(
        &mut self,
        id: NodeId,
        implementation: Arc<dyn NodeTrait>,
    ) -> Result<(), NodeError> {
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or(NodeError::UnknownNode(id.0))?;
        node.implementation = Some(implementation);
        Ok(())
    }

    /// Connects `from`'s output to input `port` of `to`.
    ///
    /// Before connecting, a depth-first search walks **upstream** from
    /// `from`. If it reaches `to`, then `to` already feeds `from` and the new
    /// edge would close a loop, so the connection is rejected.
    pub fn connect(&mut self, from: NodeId, to: NodeId, port: PortId) -> Result<(), NodeError> {
        if !self.nodes.contains_key(&from) {
            return Err(NodeError::UnknownNode(from.0));
        }
        if !self.nodes.contains_key(&to) {
            return Err(NodeError::UnknownNode(to.0));
        }
        if self.upstream_contains(from, to) {
            return Err(NodeError::Other("Cyclic connection rejected".to_string()));
        }

        self.connect_edge(Edge::new(from, PortId(0), to, port))
    }

    /// Connects two nodes, rejecting loops and dangling endpoints.
    pub fn connect_edge(&mut self, edge: Edge) -> Result<(), NodeError> {
        if !self.nodes.contains_key(&edge.from_node) {
            return Err(NodeError::UnknownNode(edge.from_node.0));
        }
        if !self.nodes.contains_key(&edge.to_node) {
            return Err(NodeError::UnknownNode(edge.to_node.0));
        }
        if self.edges.contains(&edge) {
            return Ok(());
        }
        if self.would_create_cycle(edge.from_node, edge.to_node) {
            return Err(NodeError::CycleDetected {
                from: edge.from_node.0,
                to: edge.to_node.0,
            });
        }

        self.edges.push(edge);
        Ok(())
    }

    /// Removes one connection, returning whether it existed.
    pub fn disconnect_edge(&mut self, edge: &Edge) -> bool {
        let before = self.edges.len();
        self.edges.retain(|candidate| candidate != edge);
        self.edges.len() != before
    }

    /// Returns `true` if data already flows from `from` to `to`, directly or
    /// through other nodes.
    pub fn would_create_cycle(&self, from: NodeId, to: NodeId) -> bool {
        self.upstream_contains(from, to)
    }

    /// Depth-first search **upwards** from `start`, following each node's
    /// inputs. Returns `true` when `target` is reached (or `start` is
    /// `target`, which would be a self-loop).
    fn upstream_contains(&self, start: NodeId, target: NodeId) -> bool {
        if start == target {
            return true;
        }

        let mut seen = HashSet::new();
        let mut stack = vec![start];

        while let Some(current) = stack.pop() {
            if !seen.insert(current) {
                continue;
            }
            for edge in self.edges.iter().filter(|edge| edge.to_node == current) {
                if edge.from_node == target {
                    return true;
                }
                stack.push(edge.from_node);
            }
        }

        false
    }

    /// Sets one knob on a node.
    pub fn set_knob(
        &mut self,
        id: NodeId,
        name: impl Into<String>,
        value: KnobValue,
    ) -> Result<(), NodeError> {
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or(NodeError::UnknownNode(id.0))?;
        node.knobs.insert(name, value);
        Ok(())
    }

    /// Applies a bus command to the graph.
    ///
    /// Returns the (possibly enriched) command that was applied, so callers
    /// can store it in the undo stack. For [`UndoableCommand::Batch`],
    /// sub-commands are enriched and applied one at a time; if any sub-command
    /// fails, all previously applied sub-commands are rolled back.
    pub fn apply_command(
        &mut self,
        command: UndoableCommand,
    ) -> Result<UndoableCommand, NodeError> {
        match command {
            UndoableCommand::AddNode { id, kind, x, y } => {
                self.add_node_at(id, kind.clone(), x, y)?;
                Ok(UndoableCommand::AddNode { id, kind, x, y })
            }
            UndoableCommand::RemoveNode { id, .. } => {
                self.remove_node(id)
                    .map(|_| ())
                    .ok_or(NodeError::UnknownNode(id.0))?;
                // Reconstruct the command with the same fields for return.
                Ok(command)
            }
            UndoableCommand::RestoreNode {
                id,
                kind,
                x,
                y,
                knobs,
                edges,
            } => {
                self.restore_node(id, &kind, x, y, &knobs, &edges)?;
                Ok(UndoableCommand::RestoreNode {
                    id,
                    kind,
                    x,
                    y,
                    knobs,
                    edges,
                })
            }
            UndoableCommand::ConnectEdge {
                from_node,
                from_port,
                to_node,
                to_port,
            } => {
                self.connect_edge(Edge::new(from_node, from_port, to_node, to_port))?;
                Ok(command)
            }
            UndoableCommand::DisconnectEdge {
                from_node,
                from_port,
                to_node,
                to_port,
            } => {
                self.disconnect_edge(&Edge::new(from_node, from_port, to_node, to_port));
                Ok(command)
            }
            UndoableCommand::SetKnob { .. } => {
                let cmd = command.clone();
                if let UndoableCommand::SetKnob {
                    node,
                    knob,
                    value,
                    previous: _,
                } = cmd
                {
                    self.set_knob(node, knob.clone(), value.clone())?;
                }
                Ok(command)
            }
            UndoableCommand::MoveNode {
                id,
                from_x: _,
                from_y: _,
                to_x,
                to_y,
            } => {
                self.set_position(id, to_x, to_y)?;
                Ok(command)
            }
            UndoableCommand::Batch(cmds) => {
                let mut applied: Vec<UndoableCommand> = Vec::with_capacity(cmds.len());
                for sub_cmd in cmds {
                    let enriched = self.enrich(sub_cmd);
                    match self.apply_command(enriched) {
                        Ok(applied_cmd) => applied.push(applied_cmd),
                        Err(e) => {
                            // Roll back everything applied so far, in reverse order.
                            for rolled in applied.into_iter().rev() {
                                let _ = self.apply_command(rolled.inverse());
                            }
                            return Err(e);
                        }
                    }
                }
                Ok(UndoableCommand::Batch(applied))
            }
        }
    }

    /// Reverses a bus command.
    pub fn undo_command(&mut self, command: &UndoableCommand) -> Result<(), NodeError> {
        let _ = self.apply_command(command.inverse())?;
        Ok(())
    }

    /// Computes the content hash of `target` and everything it pulls from.
    pub fn compute_op_hash(&self, target: NodeId, ctx: &EvalContext) -> Result<OpHash, NodeError> {
        self.hash_node(target, ctx, &mut HashMap::new(), &mut HashSet::new())
    }

    /// Pulls `target` for one frame, using the cache and preemption signals.
    ///
    /// The cache is consulted before any cartridge runs. Input packets are
    /// pulled in input-port order; a stale epoch aborts with
    /// [`NodeError::Preempted`] at every step.
    pub fn evaluate_pull(
        &mut self,
        target: NodeId,
        ctx: &EvalContext,
        governor: &MemoryGovernor,
        epoch: &RenderEpoch,
        target_epoch: u64,
    ) -> Result<Arc<Packet>, NodeError> {
        self.pull(
            target,
            ctx,
            governor,
            epoch,
            target_epoch,
            &mut HashSet::new(),
        )
    }

    fn pull(
        &mut self,
        id: NodeId,
        ctx: &EvalContext,
        governor: &MemoryGovernor,
        epoch: &RenderEpoch,
        target_epoch: u64,
        visiting: &mut HashSet<NodeId>,
    ) -> Result<Arc<Packet>, NodeError> {
        if !epoch.is_current(target_epoch) {
            return Err(NodeError::Preempted);
        }
        if !self.nodes.contains_key(&id) {
            return Err(NodeError::UnknownNode(id.0));
        }
        if !visiting.insert(id) {
            return Err(NodeError::CycleDetected {
                from: id.0,
                to: id.0,
            });
        }

        let hash = self.compute_op_hash(id, ctx)?;
        self.node_hashes.insert(id, hash);
        if let Some(cached) = self.cache.get(&hash) {
            visiting.remove(&id);
            return Ok(cached);
        }

        let implementation = self
            .nodes
            .get(&id)
            .and_then(|node| node.implementation.clone())
            .ok_or(NodeError::NoImplementation(id.0))?;

        // Pixel allocations will reserve permits from the governor when the
        // render milestone lands; the handle is part of the pull contract.
        let _ = governor;

        let mut edges: Vec<Edge> = self
            .edges
            .iter()
            .filter(|edge| edge.to_node == id)
            .copied()
            .collect();
        edges.sort_by_key(|edge| (edge.to_port, edge.from_node, edge.from_port));

        // Inputs are indexed by port, not by edge order: Nuke's B/A/mask ports
        // must keep their slots even when an intermediate port is unconnected.
        let max_port = edges.iter().map(|edge| edge.to_port.0).max();
        let mut inputs = vec![Packet::new(); max_port.map_or(0, |port| port as usize + 1)];
        for edge in &edges {
            let packet = self.pull(edge.from_node, ctx, governor, epoch, target_epoch, visiting)?;
            inputs[edge.to_port.0 as usize] = (*packet).clone();
        }

        let evaluated = {
            // Knobs live on the DagNode; inject them into a per-node context so
            // cartridges can read live values without changing `evaluate`'s
            // signature. The governor rides along so tile allocations reserve
            // from the same cap the kernel enforces.
            let mut node_ctx = ctx.clone();
            if let Some(node) = self.nodes.get(&id) {
                node_ctx.insert(node.knobs.clone());
            }
            node_ctx.insert(governor.clone());
            evaluate_with_airbag(implementation.as_ref(), &node_ctx, &inputs)
        };

        if !epoch.is_current(target_epoch) {
            visiting.remove(&id);
            return Err(NodeError::Preempted);
        }

        let packet = match evaluated {
            Ok(packet) => {
                if let Some(node) = self.nodes.get_mut(&id) {
                    node.crashed = None;
                }
                Arc::new(packet)
            }
            Err(error) => {
                if let NodeError::Panic(message) = &error {
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.crashed = Some(message.clone());
                    }
                }
                visiting.remove(&id);
                return Err(error);
            }
        };

        self.cache.insert(hash, Arc::clone(&packet));
        visiting.remove(&id);
        Ok(packet)
    }

    fn hash_node(
        &self,
        id: NodeId,
        ctx: &EvalContext,
        memo: &mut HashMap<NodeId, OpHash>,
        visiting: &mut HashSet<NodeId>,
    ) -> Result<OpHash, NodeError> {
        if let Some(hash) = memo.get(&id) {
            return Ok(*hash);
        }

        let node = self.nodes.get(&id).ok_or(NodeError::UnknownNode(id.0))?;
        if !visiting.insert(id) {
            return Err(NodeError::CycleDetected {
                from: id.0,
                to: id.0,
            });
        }

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"tanova-op-v1");
        hasher.update(&(node.kind.len() as u64).to_le_bytes());
        hasher.update(node.kind.as_bytes());
        node.knobs.feed_hash(&mut hasher);
        feed_eval_context(&mut hasher, ctx);

        let mut inputs: Vec<Edge> = self
            .edges
            .iter()
            .filter(|edge| edge.to_node == id)
            .copied()
            .collect();
        inputs.sort_by_key(|edge| (edge.to_port, edge.from_node, edge.from_port));

        hasher.update(&(inputs.len() as u64).to_le_bytes());
        for edge in inputs {
            hasher.update(&edge.to_port.0.to_le_bytes());
            hasher.update(&edge.from_port.0.to_le_bytes());
            let upstream = self.hash_node(edge.from_node, ctx, memo, visiting)?;
            hasher.update(upstream.as_bytes());
        }

        visiting.remove(&id);
        let hash = OpHash::from(hasher.finalize());
        memo.insert(id, hash);
        Ok(hash)
    }
}

impl Default for Dag {
    fn default() -> Self {
        Self::new()
    }
}

/// Formats one knob value for `.nk` serialization.
fn format_knob_value(value: &KnobValue) -> String {
    match value {
        KnobValue::Float(value) => {
            if value.fract() == 0.0 {
                format!("{value:.1}")
            } else {
                format!("{value}")
            }
        }
        KnobValue::Int(value) => value.to_string(),
        KnobValue::Bool(value) => {
            if *value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        KnobValue::Text(text) => format!("\"{}\"", escape_tcl_text(text)),
        KnobValue::Color(channels) => format!(
            "{{{} {} {} {}}}",
            channels[0], channels[1], channels[2], channels[3]
        ),
    }
}

/// Escapes text for a double-quoted TCL string.
fn escape_tcl_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Feeds only the render state the hash contract covers.
fn feed_eval_context(hasher: &mut blake3::Hasher, ctx: &EvalContext) {
    hasher.update(b"ctx");

    match ctx.time() {
        Some(time) => {
            hasher.update(&[1]);
            hasher.update(&canonical_f64(time).to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }

    match ctx.view() {
        Some(view) => {
            hasher.update(&[1]);
            hasher.update(&(view.len() as u64).to_le_bytes());
            hasher.update(view.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::UndoStack;
    use crate::types::Time;

    struct CountingNode {
        counter: Arc<AtomicUsize>,
        tag: u64,
    }

    impl NodeTrait for CountingNode {
        fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            let mut packet = Packet::new();
            packet.insert(self.tag);
            packet.insert(inputs.len());
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            format!("CountingNode {{ tag {tag} }}", tag = self.tag)
        }
    }

    struct PanickingNode;

    impl NodeTrait for PanickingNode {
        fn evaluate(&self, _ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            panic!("dag airbag test");
        }

        fn serialize_to_tcl(&self) -> String {
            "PanickingNode {}".to_string()
        }
    }

    struct BumpingNode {
        epoch: RenderEpoch,
    }

    impl NodeTrait for BumpingNode {
        fn evaluate(&self, _ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            self.epoch.bump();
            Ok(Packet::new())
        }

        fn serialize_to_tcl(&self) -> String {
            "BumpingNode {}".to_string()
        }
    }

    struct GovernorProbe;

    impl NodeTrait for GovernorProbe {
        fn evaluate(&self, ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            let governor = ctx
                .governor()
                .ok_or_else(|| NodeError::Other("governor was not injected".to_string()))?;
            let mut packet = Packet::new();
            packet.insert(governor.max_bytes());
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            "GovernorProbe {}".to_string()
        }
    }

    struct TagNode(u32);

    impl NodeTrait for TagNode {
        fn evaluate(&self, _ctx: &EvalContext, _inputs: &[Packet]) -> Result<Packet, NodeError> {
            let mut packet = Packet::new();
            packet.insert(self.0);
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            format!("TagNode {{ tag {} }}", self.0)
        }
    }

    struct PortProbe;

    impl NodeTrait for PortProbe {
        fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
            let report: Vec<(bool, Option<u32>)> = inputs
                .iter()
                .map(|packet| (packet.is_empty(), packet.get::<u32>().copied()))
                .collect();
            let mut packet = Packet::new();
            packet.insert(report);
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            "PortProbe {}".to_string()
        }
    }

    fn counting_node(counter: &Arc<AtomicUsize>, tag: u64) -> Arc<dyn NodeTrait> {
        Arc::new(CountingNode {
            counter: Arc::clone(counter),
            tag,
        })
    }

    fn chain(implementation: bool) -> (Dag, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut dag = Dag::new();

        if implementation {
            dag.add_node_with_implementation(NodeId(1), "Source", counting_node(&counter, 1))
                .unwrap();
            dag.add_node_with_implementation(NodeId(2), "Sink", counting_node(&counter, 2))
                .unwrap();
        } else {
            dag.add_node(NodeId(1), "Source").unwrap();
            dag.add_node(NodeId(2), "Sink").unwrap();
        }

        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();
        (dag, counter)
    }

    #[test]
    fn rejects_cycles() {
        let (mut dag, _) = chain(false);

        let error = dag
            .connect_edge(Edge::new(NodeId(2), PortId(0), NodeId(1), PortId(0)))
            .expect_err("A -> B -> A must be rejected");

        assert!(
            matches!(error, NodeError::CycleDetected { from: 2, to: 1 }),
            "unexpected error: {error:?}"
        );

        let self_loop = dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(1), PortId(0)));
        assert!(matches!(
            self_loop,
            Err(NodeError::CycleDetected { from: 1, to: 1 })
        ));
        assert_eq!(dag.edge_count(), 1);
    }

    #[test]
    fn connect_rejects_cycles_with_the_documented_error() {
        let (mut dag, _) = chain(false);

        let error = dag
            .connect(NodeId(2), NodeId(1), PortId(0))
            .expect_err("B -> A must be rejected");
        assert_eq!(
            error,
            NodeError::Other("Cyclic connection rejected".to_string())
        );

        let self_loop = dag
            .connect(NodeId(1), NodeId(1), PortId(0))
            .expect_err("A -> A must be rejected");
        assert_eq!(
            self_loop,
            NodeError::Other("Cyclic connection rejected".to_string())
        );
        assert_eq!(dag.edge_count(), 1);
    }

    #[test]
    fn connect_accepts_an_acyclic_edge() {
        let mut dag = Dag::new();
        dag.add_node(NodeId(1), "Source").unwrap();
        dag.add_node(NodeId(2), "Sink").unwrap();

        dag.connect(NodeId(1), NodeId(2), PortId(1))
            .expect("forward edge should connect");

        let edge = dag.edges().next().copied().expect("edge recorded");
        assert_eq!(edge.from_node, NodeId(1));
        assert_eq!(edge.from_port, PortId(0));
        assert_eq!(edge.to_node, NodeId(2));
        assert_eq!(edge.to_port, PortId(1));
    }

    #[test]
    fn add_node_command_records_canvas_coordinates() {
        let command = UndoableCommand::AddNode {
            id: NodeId(1),
            kind: "Constant".to_string(),
            x: 12.5,
            y: -4.0,
        };

        let mut dag = Dag::new();
        dag.apply_command(command.clone())
            .expect("AddNode should apply");
        let node = dag.node(NodeId(1)).expect("node exists");
        assert_eq!((node.x, node.y), (12.5, -4.0));

        // Undo removes the node, and redo restores the same coordinates.
        dag.apply_command(command.inverse())
            .expect("undo should apply");
        assert!(dag.node(NodeId(1)).is_none());

        dag.apply_command(command.inverse().inverse())
            .expect("redo should apply");
        let node = dag.node(NodeId(1)).expect("node re-created");
        assert_eq!((node.x, node.y), (12.5, -4.0));
    }

    #[test]
    fn op_hash_is_deterministic_and_reacts_to_changes() {
        let mut ctx = EvalContext::new();
        ctx.insert(Time(1.0));
        ctx.insert(crate::types::View("main".to_string()));

        let (dag, _) = chain(false);
        let baseline = dag.compute_op_hash(NodeId(2), &ctx).unwrap();
        assert_eq!(baseline, dag.compute_op_hash(NodeId(2), &ctx).unwrap());
        assert_eq!(baseline.to_hex().len(), 64);

        // An identical graph built in a different node order hashes the same.
        let mut twin = Dag::new();
        twin.add_node(NodeId(2), "Sink").unwrap();
        twin.add_node(NodeId(1), "Source").unwrap();
        twin.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();
        assert_eq!(baseline, twin.compute_op_hash(NodeId(2), &ctx).unwrap());

        let mut changed_upstream = chain(false).0;
        changed_upstream
            .set_knob(NodeId(1), "gain", KnobValue::Float(2.0))
            .unwrap();
        let upstream_hash = changed_upstream.compute_op_hash(NodeId(2), &ctx).unwrap();
        assert_ne!(baseline, upstream_hash);

        let mut changed_downstream = chain(false).0;
        changed_downstream
            .set_knob(NodeId(2), "mix", KnobValue::Float(0.25))
            .unwrap();
        let downstream_hash = changed_downstream.compute_op_hash(NodeId(2), &ctx).unwrap();
        assert_ne!(baseline, downstream_hash);
        assert_ne!(upstream_hash, downstream_hash);

        let mut later_ctx = ctx.clone();
        later_ctx.insert(Time(2.0));
        assert_ne!(
            baseline,
            dag.compute_op_hash(NodeId(2), &later_ctx).unwrap()
        );

        let mut other_view = ctx.clone();
        other_view.insert(crate::types::View("other".to_string()));
        assert_ne!(
            baseline,
            dag.compute_op_hash(NodeId(2), &other_view).unwrap()
        );
    }

    #[test]
    fn lazy_pull_caches_until_knobs_change() {
        let (mut dag, counter) = chain(true);
        let ctx = EvalContext::new();
        let governor = MemoryGovernor::new(usize::MAX);
        let epoch = RenderEpoch::new();

        let first = dag
            .evaluate_pull(NodeId(2), &ctx, &governor, &epoch, epoch.current())
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2, "source + sink");
        assert_eq!(first.get::<usize>(), Some(&1));

        let second = dag
            .evaluate_pull(NodeId(2), &ctx, &governor, &epoch, epoch.current())
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2, "cache hit");
        assert!(Arc::ptr_eq(&first, &second));

        // Changing an upstream knob invalidates the source and the sink.
        dag.set_knob(NodeId(1), "gain", KnobValue::Float(4.0))
            .unwrap();
        dag.evaluate_pull(NodeId(2), &ctx, &governor, &epoch, epoch.current())
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 4);

        // Changing only the sink re-evaluates the sink; the source stays cached.
        dag.set_knob(NodeId(2), "mix", KnobValue::Float(0.5))
            .unwrap();
        dag.evaluate_pull(NodeId(2), &ctx, &governor, &epoch, epoch.current())
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn pull_aborts_when_epoch_is_bumped_mid_evaluation() {
        let epoch = RenderEpoch::new();
        let mut dag = Dag::new();
        dag.add_node_with_implementation(
            NodeId(1),
            "Bumping",
            Arc::new(BumpingNode {
                epoch: epoch.clone(),
            }),
        )
        .unwrap();

        let error = dag
            .evaluate_pull(
                NodeId(1),
                &EvalContext::new(),
                &MemoryGovernor::new(usize::MAX),
                &epoch,
                epoch.current(),
            )
            .expect_err("bumped epoch must preempt the pull");

        assert!(matches!(error, NodeError::Preempted));
        assert!(dag.cache().is_empty());
    }

    #[test]
    fn ram_cache_evicts_the_oldest_entry() {
        let mut cache = RamCache::new(2);
        let hashes = [OpHash([1; 32]), OpHash([2; 32]), OpHash([3; 32])];

        cache.insert(hashes[0], Arc::new(Packet::new()));
        cache.insert(hashes[1], Arc::new(Packet::new()));
        assert_eq!(cache.len(), 2);

        cache.insert(hashes[2], Arc::new(Packet::new()));
        assert_eq!(cache.len(), 2);
        assert!(!cache.contains(&hashes[0]), "oldest entry should be gone");
        assert!(cache.contains(&hashes[1]));
        assert!(cache.contains(&hashes[2]));

        cache.remove(&hashes[2]);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn panics_are_recorded_on_the_node() {
        let mut dag = Dag::new();
        dag.add_node_with_implementation(NodeId(1), "Panicking", Arc::new(PanickingNode))
            .unwrap();

        let error = dag
            .evaluate_pull(
                NodeId(1),
                &EvalContext::new(),
                &MemoryGovernor::new(usize::MAX),
                &RenderEpoch::new(),
                0,
            )
            .expect_err("panic must surface as an error");

        match error {
            NodeError::Panic(message) => assert!(message.contains("dag airbag test")),
            other => panic!("expected NodeError::Panic, got {other:?}"),
        }
        assert_eq!(
            dag.node(NodeId(1)).unwrap().crashed.as_deref(),
            Some("dag airbag test")
        );
        assert!(dag.cache().is_empty());
    }

    #[test]
    fn governor_is_injected_into_the_eval_context() {
        let mut dag = Dag::new();
        dag.add_node_with_implementation(NodeId(1), "GovernorProbe", Arc::new(GovernorProbe))
            .unwrap();

        let governor = MemoryGovernor::new(4096);
        let packet = dag
            .evaluate_pull(
                NodeId(1),
                &EvalContext::new(),
                &governor,
                &RenderEpoch::new(),
                0,
            )
            .expect("governor probe should evaluate");

        assert_eq!(
            packet.get::<usize>(),
            Some(&4096),
            "the cartridge must see the kernel's governor through EvalContext"
        );
    }

    #[test]
    fn sparse_ports_keep_nuke_input_indices() {
        let mut dag = Dag::new();
        dag.add_node_with_implementation(NodeId(1), "TagB", Arc::new(TagNode(11)))
            .unwrap();
        dag.add_node_with_implementation(NodeId(3), "TagA", Arc::new(TagNode(33)))
            .unwrap();
        dag.add_node_with_implementation(NodeId(2), "PortProbe", Arc::new(PortProbe))
            .unwrap();

        // B (0) and mask (2) are connected; A (1) is intentionally empty.
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();
        dag.connect_edge(Edge::new(NodeId(3), PortId(0), NodeId(2), PortId(2)))
            .unwrap();

        let packet = dag
            .evaluate_pull(
                NodeId(2),
                &EvalContext::new(),
                &MemoryGovernor::new(usize::MAX),
                &RenderEpoch::new(),
                0,
            )
            .expect("probe should evaluate");

        let report = packet
            .get::<Vec<(bool, Option<u32>)>>()
            .expect("probe report");

        assert_eq!(report.len(), 3, "inputs are indexed by port, not edge");
        assert_eq!(report[0], (false, Some(11)));
        assert_eq!(report[1], (true, None), "port 1 must be an empty slot");
        assert_eq!(report[2], (false, Some(33)));
    }

    #[test]
    fn unknown_kinds_get_a_passthrough_implementation() {
        let mut dag = Dag::new();
        dag.add_node_with_implementation(NodeId(1), "Source", Arc::new(TagNode(7)))
            .unwrap();
        dag.add_node(NodeId(2), "Foundry_CameraShake").unwrap();
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();

        assert!(
            dag.node(NodeId(2)).unwrap().implementation.is_some(),
            "unknown kinds must not be left without an implementation"
        );

        let packet = dag
            .evaluate_pull(
                NodeId(2),
                &EvalContext::new(),
                &MemoryGovernor::new(usize::MAX),
                &RenderEpoch::new(),
                0,
            )
            .expect("passthrough should evaluate");
        assert_eq!(
            packet.get::<u32>(),
            Some(&7),
            "port 0 must be forwarded untouched"
        );
    }

    #[test]
    fn export_uses_nuke_stack_syntax_and_live_knobs() {
        let mut dag = Dag::new();
        dag.add_node(NodeId(1), "Source").unwrap();
        dag.add_node(NodeId(2), "Sink").unwrap();
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();
        dag.set_knob(NodeId(1), "gain", KnobValue::Float(2.0))
            .unwrap();
        dag.set_knob(NodeId(2), "secret_knob", KnobValue::Int(42))
            .unwrap();

        let text = dag.export_to_tcl();
        assert!(text.starts_with("version "), "missing header: {text}");
        assert!(text.contains("push $N1"), "missing upstream push: {text}");
        assert!(
            text.contains("set N1 [stack 0]"),
            "missing var capture: {text}"
        );
        assert!(
            text.contains("set N2 [stack 0]"),
            "missing var capture: {text}"
        );
        assert!(
            text.contains("gain 2.0"),
            "float knobs must survive: {text}"
        );
        assert!(
            text.contains("secret_knob 42"),
            "unknown-node knobs must survive: {text}"
        );
        assert!(text.contains("inputs 1"), "input count missing: {text}");
    }

    #[test]
    fn removal_snapshot_restores_knobs_position_and_both_edges() {
        let mut dag = Dag::new();
        dag.add_node_at(NodeId(1), "Constant", 10.0, 20.0).unwrap();
        dag.add_node_at(NodeId(2), "Grade", 10.0, 144.0).unwrap();
        dag.add_node_at(NodeId(3), "Blur", 10.0, 268.0).unwrap();
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();
        dag.connect_edge(Edge::new(NodeId(2), PortId(0), NodeId(3), PortId(0)))
            .unwrap();

        // A non-default knob value is the part a naive undo would lose.
        let edited = KnobValue::Color([0.5, 1.25, 2.0, 0.75]);
        dag.set_knob(NodeId(2), "multiply", edited.clone()).unwrap();

        let command = dag.removal_command(NodeId(2)).expect("live snapshot");
        match &command {
            UndoableCommand::RemoveNode {
                x,
                y,
                knobs,
                edges,
                kind,
                ..
            } => {
                assert_eq!(kind, "Grade");
                assert_eq!((*x, *y), (10.0, 144.0));
                assert_eq!(knobs.get("multiply"), Some(&edited));
                assert_eq!(edges.len(), 2, "both incident edges are captured");
            }
            other => panic!("expected RemoveNode, got {other:?}"),
        }

        dag.apply_command(command.clone()).unwrap();
        assert!(dag.node(NodeId(2)).is_none());
        assert_eq!(dag.edge_count(), 0, "no dangling edge may survive");
        assert!(
            dag.edges()
                .all(|edge| edge.from_node != NodeId(2) && edge.to_node != NodeId(2)),
            "removal must purge every incident edge"
        );

        dag.undo_command(&command).unwrap();
        let restored = dag.node(NodeId(2)).expect("Grade is back");
        assert_eq!((restored.x, restored.y), (10.0, 144.0));
        assert_eq!(restored.kind, "Grade");
        assert_eq!(restored.knobs.get("multiply"), Some(&edited));

        let edges: Vec<Edge> = dag.edges().copied().collect();
        assert_eq!(edges.len(), 2, "both wires must be restored: {edges:?}");
        assert!(edges.contains(&Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0))));
        assert!(edges.contains(&Edge::new(NodeId(2), PortId(0), NodeId(3), PortId(0))));

        // Redo must remove it again, still without dangling edges.
        dag.apply_command(command).unwrap();
        assert!(dag.node(NodeId(2)).is_none());
        assert_eq!(dag.edge_count(), 0);
    }

    #[test]
    fn restoring_a_node_that_still_exists_is_an_error() {
        let mut dag = Dag::new();
        dag.add_node_at(NodeId(1), "Grade", 0.0, 0.0).unwrap();
        let command = dag.removal_command(NodeId(1)).unwrap();

        assert!(matches!(
            dag.apply_command(command.inverse()),
            Err(NodeError::NodeAlreadyExists(1))
        ));
    }

    /// A cartridge that echoes a u32 input with an offset, simulating a simple
    /// transform for end-to-end evaluate_pull testing.
    struct OffsetNode(i32);

    impl NodeTrait for OffsetNode {
        fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
            let input = inputs
                .first()
                .and_then(|p| p.get::<u32>())
                .copied()
                .unwrap_or(0);
            let mut packet = Packet::new();
            packet.insert(input.wrapping_add(self.0 as u32));
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            format!("OffsetNode {{ offset {} }}", self.0)
        }
    }

    /// A cartridge that combines two u32 inputs by taking the first non-zero,
    /// simulating a simple merge for end-to-end evaluate_pull testing.
    struct MergePickNode;

    impl NodeTrait for MergePickNode {
        fn evaluate(&self, _ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError> {
            let value = inputs
                .iter()
                .find_map(|p| p.get::<u32>().copied())
                .filter(|&v| v != 0)
                .unwrap_or(0);
            let mut packet = Packet::new();
            packet.insert(value);
            Ok(packet)
        }

        fn serialize_to_tcl(&self) -> String {
            "MergePickNode {}".to_string()
        }
    }

    fn offset_factory(offset: i32) -> Arc<dyn NodeTrait> {
        Arc::new(OffsetNode(offset))
    }

    #[test]
    fn evaluate_pull_end_to_end_through_dag() {
        let mut dag = Dag::new();

        // Source A: constant value 1 (red) — no inputs, offset 1 → output 1
        dag.add_node_with_implementation(NodeId(1), "SourceA", offset_factory(1))
            .unwrap();

        // Source B: constant value 2 (blue) — no inputs, offset 2 → output 2
        dag.add_node_with_implementation(NodeId(2), "SourceB", offset_factory(2))
            .unwrap();

        // Transform on source A: offset by 100 → 1 + 100 = 101
        dag.add_node_with_implementation(NodeId(3), "Transform", offset_factory(100))
            .unwrap();

        // Merge: A over B (pick first non-zero) → picks 101
        dag.add_node_with_implementation(NodeId(4), "Merge2", Arc::new(MergePickNode))
            .unwrap();

        // Viewer: pulls from merge, offset 0 → passes through 101
        dag.add_node_with_implementation(NodeId(5), "Viewer", offset_factory(0))
            .unwrap();

        // Wire: SourceA(1) -> Transform(3) -> Merge2(4, port 0 = A)
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(3), PortId(0)))
            .unwrap();
        dag.connect_edge(Edge::new(NodeId(3), PortId(0), NodeId(4), PortId(0)))
            .unwrap();
        // Wire: SourceB(2) -> Merge2(4, port 1 = B)
        dag.connect_edge(Edge::new(NodeId(2), PortId(0), NodeId(4), PortId(1)))
            .unwrap();
        // Wire: Merge2(4) -> Viewer(5)
        dag.connect_edge(Edge::new(NodeId(4), PortId(0), NodeId(5), PortId(0)))
            .unwrap();

        let ctx = EvalContext::new();
        let governor = MemoryGovernor::new(usize::MAX);
        let epoch = RenderEpoch::new();
        let target_epoch = epoch.bump();

        let packet = dag
            .evaluate_pull(NodeId(5), &ctx, &governor, &epoch, target_epoch)
            .expect("evaluate_pull should succeed");
        let value = packet.get::<u32>().expect("viewer packet should carry u32");
        // SourceA(1) -> Transform(+100) = 101 -> Merge2 picks 101 (first non-zero)
        assert_eq!(
            *value, 101,
            "end-to-end evaluate_pull should produce transformed value"
        );
    }

    #[test]
    fn invalidate_downstream_keeps_upstream_cached() {
        let mut dag = Dag::new();

        // Chain: Constant(1) -> Transform(+10) -> Merge2(pick first)
        dag.add_node(NodeId(1), "Constant").unwrap();
        dag.node_mut(NodeId(1)).unwrap().implementation = Some(offset_factory(1));

        dag.add_node(NodeId(2), "Transform").unwrap();
        dag.node_mut(NodeId(2)).unwrap().implementation = Some(offset_factory(10));

        dag.add_node(NodeId(3), "Merge2").unwrap();
        dag.node_mut(NodeId(3)).unwrap().implementation = Some(Arc::new(MergePickNode));

        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();
        dag.connect_edge(Edge::new(NodeId(2), PortId(0), NodeId(3), PortId(0)))
            .unwrap();

        let ctx = EvalContext::new();
        let governor = MemoryGovernor::new(usize::MAX);
        let epoch = RenderEpoch::new();
        let target_epoch = epoch.bump();

        // Evaluate to populate cache: Constant=1, Transform=11, Merge2=11
        let _ = dag.evaluate_pull(NodeId(3), &ctx, &governor, &epoch, target_epoch);
        let cache_size_before = dag.cache().len();
        assert!(
            cache_size_before > 0,
            "cache should have entries after evaluation"
        );

        // Invalidate downstream of Transform (node 2): should remove Transform and Merge2
        dag.invalidate_downstream(NodeId(2));
        let cache_size_after = dag.cache().len();
        assert!(
            cache_size_after < cache_size_before,
            "downstream invalidation should reduce cache size"
        );

        // Re-evaluate: Constant should still be cached (its hash didn't change)
        let packet = dag
            .evaluate_pull(NodeId(3), &ctx, &governor, &epoch, target_epoch)
            .expect("re-evaluate should succeed");
        let value = packet.get::<u32>().expect("packet should carry u32");
        assert_eq!(
            *value, 11,
            "result should still be correct after downstream invalidation"
        );
    }

    #[test]
    fn set_knob_coalesces_on_undo_stack() {
        let mut stack = UndoStack::new();

        stack.push(UndoableCommand::SetKnob {
            node: NodeId(1),
            knob: "translate".to_string(),
            value: KnobValue::Color([1.0, 2.0, 0.0, 1.0]),
            previous: KnobValue::Color([0.0, 0.0, 0.0, 1.0]),
        });
        stack.push(UndoableCommand::SetKnob {
            node: NodeId(1),
            knob: "translate".to_string(),
            value: KnobValue::Color([5.0, 6.0, 0.0, 1.0]),
            previous: KnobValue::Color([1.0, 2.0, 0.0, 1.0]),
        });
        stack.push(UndoableCommand::SetKnob {
            node: NodeId(1),
            knob: "translate".to_string(),
            value: KnobValue::Color([9.0, 10.0, 0.0, 1.0]),
            previous: KnobValue::Color([5.0, 6.0, 0.0, 1.0]),
        });

        // Three rapid SetKnobs should coalesce into one undo step
        assert!(stack.can_undo(), "should have one undo entry");
        assert!(!stack.can_redo(), "redo should be empty after push");

        let undone = stack.undo().expect("one coalesced undo step");
        match undone {
            UndoableCommand::SetKnob {
                value, previous, ..
            } => {
                assert_eq!(
                    value,
                    KnobValue::Color([9.0, 10.0, 0.0, 1.0]),
                    "coalesced value should be the last"
                );
                assert_eq!(
                    previous,
                    KnobValue::Color([0.0, 0.0, 0.0, 1.0]),
                    "coalesced previous should be the first"
                );
            }
            other => panic!("expected SetKnob, got {other:?}"),
        }
        assert!(!stack.can_undo(), "drag must undo in one step");
    }

    #[test]
    fn batch_rollback_on_cycle_detection() {
        use crate::dag::Edge;

        let mut dag = Dag::new();
        dag.add_node(NodeId(1), "Constant").unwrap();
        dag.add_node(NodeId(2), "Grade").unwrap();
        dag.add_node(NodeId(3), "Blur").unwrap();
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();
        dag.connect_edge(Edge::new(NodeId(2), PortId(0), NodeId(3), PortId(0)))
            .unwrap();

        // Attempt to create a cycle: disconnect #1->#2, connect #3->#2.
        // #3->#2 would make #3 feed #2, but #2 already feeds #3, so this is a cycle.
        let batch = UndoableCommand::from_vec(vec![
            UndoableCommand::DisconnectEdge {
                from_node: NodeId(1),
                from_port: PortId(0),
                to_node: NodeId(2),
                to_port: PortId(0),
            },
            UndoableCommand::ConnectEdge {
                from_node: NodeId(3),
                from_port: PortId(0),
                to_node: NodeId(2),
                to_port: PortId(0),
            },
        ])
        .expect("batch should be created");

        let result = dag.apply_command(batch);
        assert!(
            matches!(result, Err(NodeError::CycleDetected { from: 3, to: 2 })),
            "expected cycle detection error, got {result:?}"
        );

        // The original wire #1->#2 must still be connected.
        let edges: Vec<Edge> = dag.edges().copied().collect();
        assert_eq!(edges.len(), 2, "both original edges must remain");
        assert!(edges.contains(&Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0))));
        assert!(edges.contains(&Edge::new(NodeId(2), PortId(0), NodeId(3), PortId(0))));
    }

    #[test]
    fn multi_node_delete_single_step_undo() {
        use crate::dag::Edge;

        let mut dag = Dag::new();
        dag.add_node(NodeId(1), "Constant").unwrap();
        dag.add_node(NodeId(2), "Grade").unwrap();
        dag.connect_edge(Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
            .unwrap();

        // Delete both nodes in a single Batch.
        let batch = UndoableCommand::from_vec(vec![
            dag.removal_command(NodeId(2)).unwrap(),
            dag.removal_command(NodeId(1)).unwrap(),
        ])
        .expect("batch should be created");

        assert!(dag.apply_command(batch.clone()).is_ok());
        assert!(dag.node(NodeId(1)).is_none());
        assert!(dag.node(NodeId(2)).is_none());
        assert_eq!(dag.edge_count(), 0);

        // Undo once restores both nodes and the edge.
        dag.apply_command(batch.inverse())
            .expect("undo should work");
        assert!(dag.node(NodeId(1)).is_some());
        assert!(dag.node(NodeId(2)).is_some());
        assert_eq!(dag.edge_count(), 1);
        assert!(
            dag.edges()
                .any(|e| *e == Edge::new(NodeId(1), PortId(0), NodeId(2), PortId(0)))
        );

        // Redo once removes both again.
        dag.apply_command(batch.clone()).expect("redo should work");
        assert!(dag.node(NodeId(1)).is_none());
        assert!(dag.node(NodeId(2)).is_none());
        assert_eq!(dag.edge_count(), 0);

        // Undo once more restores again.
        dag.apply_command(batch.inverse())
            .expect("undo should work");
        assert!(dag.node(NodeId(1)).is_some());
        assert!(dag.node(NodeId(2)).is_some());
        assert_eq!(dag.edge_count(), 1);
    }

    #[test]
    fn group_drag_single_step_undo() {
        use crate::UndoStack;

        let mut dag = Dag::new();
        dag.add_node_at(NodeId(1), "Constant", 10.0, 20.0).unwrap();
        dag.add_node_at(NodeId(2), "Grade", 100.0, 200.0).unwrap();

        let mut undo = UndoStack::new();

        // Push 4 consecutive Batch group-move commands for [1, 2].
        for i in 0..4 {
            let dx = (i + 1) as f32 * 10.0;
            let dy = (i + 1) as f32 * 5.0;
            let batch = UndoableCommand::from_vec(vec![
                UndoableCommand::MoveNode {
                    id: NodeId(1),
                    from_x: 10.0,
                    from_y: 20.0,
                    to_x: 10.0 + dx,
                    to_y: 20.0 + dy,
                },
                UndoableCommand::MoveNode {
                    id: NodeId(2),
                    from_x: 100.0,
                    from_y: 200.0,
                    to_x: 100.0 + dx,
                    to_y: 200.0 + dy,
                },
            ])
            .expect("batch should be created");
            assert!(dag.apply_command(batch.clone()).is_ok());
            undo.push(batch);
        }

        // All batches should coalesce into one undo step.
        assert!(undo.can_undo());
        assert!(!undo.can_redo());

        // Undo once should move both nodes back to initial positions.
        let undone = undo.undo().expect("undo should work");
        assert!(matches!(undone, UndoableCommand::Batch(_)));
        dag.undo_command(&undone).expect("apply undo should work");
        assert_eq!(dag.node(NodeId(1)).unwrap().x, 10.0);
        assert_eq!(dag.node(NodeId(1)).unwrap().y, 20.0);
        assert_eq!(dag.node(NodeId(2)).unwrap().x, 100.0);
        assert_eq!(dag.node(NodeId(2)).unwrap().y, 200.0);
        assert!(!undo.can_undo(), "the drag must undo in one step");
    }

    #[test]
    fn time_bounded_coalescing_break() {
        let mut stack = UndoStack::new();
        stack.break_coalescing();

        stack.push(UndoableCommand::SetKnob {
            node: NodeId(1),
            knob: "translate".to_string(),
            value: KnobValue::Color([1.0, 2.0, 0.0, 1.0]),
            previous: KnobValue::Color([0.0, 0.0, 0.0, 1.0]),
        });
        stack.break_coalescing();
        stack.push(UndoableCommand::SetKnob {
            node: NodeId(1),
            knob: "translate".to_string(),
            value: KnobValue::Color([3.0, 4.0, 0.0, 1.0]),
            previous: KnobValue::Color([1.0, 2.0, 0.0, 1.0]),
        });

        // Two SetKnobs separated by break_coalescing() should be two distinct steps.
        assert!(stack.can_undo());
        let first = stack.undo().expect("first undo");
        match first {
            UndoableCommand::SetKnob {
                value, previous, ..
            } => {
                assert_eq!(
                    value,
                    KnobValue::Color([3.0, 4.0, 0.0, 1.0]),
                    "first undo should return the most recent value"
                );
                assert_eq!(previous, KnobValue::Color([1.0, 2.0, 0.0, 1.0]),);
            }
            other => panic!("expected SetKnob, got {other:?}"),
        }
        assert!(stack.can_undo(), "second undo step should remain");
    }
}
