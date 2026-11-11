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

    /// Adds a node, auto-wiring its cartridge from the registry when the kind
    /// is registered.
    ///
    /// Registered cartridges contribute their default knobs and factory
    /// implementation. Unknown kinds are still added, ready for the UI to
    /// attach an implementation manually.
    pub fn add_node(&mut self, id: NodeId, kind: impl Into<String>) -> Result<(), NodeError> {
        let kind = kind.into();
        let mut node = DagNode::new(id, kind.clone());
        if let Some(descriptor) = find_cartridge(&kind) {
            node.knobs = (descriptor.default_knobs)();
            node.implementation = Some((descriptor.factory)());
        }
        self.insert_node(node)
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
        self.is_reachable(to, from)
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
    pub fn apply_command(&mut self, command: &UndoableCommand) -> Result<(), NodeError> {
        match command {
            UndoableCommand::AddNode { id, kind } => self.add_node(*id, kind.clone()),
            UndoableCommand::RemoveNode { id, .. } => self
                .remove_node(*id)
                .map(|_| ())
                .ok_or(NodeError::UnknownNode(id.0)),
            UndoableCommand::ConnectEdge {
                from_node,
                from_port,
                to_node,
                to_port,
            } => self.connect_edge(Edge::new(*from_node, *from_port, *to_node, *to_port)),
            UndoableCommand::DisconnectEdge {
                from_node,
                from_port,
                to_node,
                to_port,
            } => {
                self.disconnect_edge(&Edge::new(*from_node, *from_port, *to_node, *to_port));
                Ok(())
            }
            UndoableCommand::SetKnob {
                node, knob, value, ..
            } => self.set_knob(*node, knob.clone(), value.clone()),
        }
    }

    /// Reverses a bus command.
    pub fn undo_command(&mut self, command: &UndoableCommand) -> Result<(), NodeError> {
        self.apply_command(&command.inverse())
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

        let mut inputs = Vec::with_capacity(edges.len());
        for edge in &edges {
            let packet = self.pull(edge.from_node, ctx, governor, epoch, target_epoch, visiting)?;
            inputs.push((*packet).clone());
        }

        let evaluated = evaluate_with_airbag(implementation.as_ref(), ctx, &inputs);

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

    fn is_reachable(&self, start: NodeId, target: NodeId) -> bool {
        if start == target {
            return true;
        }

        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        seen.insert(start);
        queue.push_back(start);

        while let Some(current) = queue.pop_front() {
            for edge in self.edges.iter().filter(|edge| edge.from_node == current) {
                if edge.to_node == target {
                    return true;
                }
                if seen.insert(edge.to_node) {
                    queue.push_back(edge.to_node);
                }
            }
        }

        false
    }
}

impl Default for Dag {
    fn default() -> Self {
        Self::new()
    }
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
}
