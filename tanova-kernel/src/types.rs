//! Blackboard types shared by the kernel and every cartridge.
//!
//! Data flows through the DAG in two directions:
//!
//! * [`Packet`] flows **bottom-up** (node output to downstream inputs).
//! * [`EvalContext`] flows **top-down** (viewer request to upstream nodes).
//!
//! Both are type maps: any `Send + Sync` value can be stored under its own
//! type, which keeps the kernel blind to VFX-specific data.

use std::any::{Any, TypeId};
use std::collections::HashMap;

use crate::PortId;
use crate::governor::MemoryGovernor;
use crate::knobs::KnobTable;

/// Object-safe extension of `std::any::Any` that can clone itself, used as the
/// value trait for [`TypeMap`].
///
/// This is the pure-`std` replacement for `anymap::any::CloneAny`: values are
/// stored as `Box<dyn CloneAny>` and downcast on demand, so no `unsafe` code is
/// required.
trait CloneAny: Any + Send + Sync {
    fn clone_box(&self) -> Box<dyn CloneAny>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

impl<T: Any + Send + Sync + Clone> CloneAny for T {
    fn clone_box(&self) -> Box<dyn CloneAny> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Clones a stored value through its concrete `CloneAny` implementation.
///
/// The receiver is taken as `&dyn CloneAny` on purpose: `Box<dyn CloneAny>`
/// itself satisfies the blanket `CloneAny` impl, so calling `clone_box` on the
/// box would recurse into this blanket impl forever.
fn clone_boxed(value: &dyn CloneAny) -> Box<dyn CloneAny> {
    value.clone_box()
}

/// Unboxes a stored value back to its concrete type.
fn take_boxed<T: Any + Send + Sync + Clone>(value: Box<dyn CloneAny>) -> Option<T> {
    CloneAny::into_any(value)
        .downcast::<T>()
        .ok()
        .map(|boxed| *boxed)
}

impl Clone for TypeMap {
    fn clone(&self) -> Self {
        Self {
            values: self
                .values
                .iter()
                .map(|(&type_id, value)| (type_id, clone_boxed(&**value)))
                .collect(),
        }
    }
}

/// The backing type map: one value per concrete type, safe to move between
/// the UI and the kernel thread.
///
/// Values must be `Clone` so packets stay cheaply copyable for the RAM cache;
/// heavy payloads should be stored behind an `Arc` so a clone does not copy
/// pixels.
#[derive(Default)]
pub struct TypeMap {
    values: HashMap<TypeId, Box<dyn CloneAny>>,
}

impl std::fmt::Debug for TypeMap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TypeMap")
            .field("len", &self.values.len())
            .finish()
    }
}

impl TypeMap {
    /// Creates an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty map with room for `capacity` values.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            values: HashMap::with_capacity(capacity),
        }
    }

    /// Inserts a value, returning the previous value of the same type, if any.
    pub fn insert<T: Any + Send + Sync + Clone>(&mut self, value: T) -> Option<T> {
        let previous = self.values.insert(TypeId::of::<T>(), Box::new(value))?;
        take_boxed::<T>(previous)
    }

    /// Borrows the value of type `T`.
    pub fn get<T: Any + Send + Sync + Clone>(&self) -> Option<&T> {
        let value: &dyn CloneAny = &**self.values.get(&TypeId::of::<T>())?;
        value.as_any().downcast_ref::<T>()
    }

    /// Mutably borrows the value of type `T`.
    pub fn get_mut<T: Any + Send + Sync + Clone>(&mut self) -> Option<&mut T> {
        let value: &mut dyn CloneAny = &mut **self.values.get_mut(&TypeId::of::<T>())?;
        value.as_any_mut().downcast_mut::<T>()
    }

    /// Removes and returns the value of type `T`.
    pub fn remove<T: Any + Send + Sync + Clone>(&mut self) -> Option<T> {
        let value = self.values.remove(&TypeId::of::<T>())?;
        take_boxed::<T>(value)
    }

    /// Returns `true` if the map holds a value of type `T`.
    pub fn contains<T: Any + Send + Sync + Clone>(&self) -> bool {
        self.values.contains_key(&TypeId::of::<T>())
    }

    /// Number of values in the map.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` if the map holds no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Drops every value from the map.
    pub fn clear(&mut self) {
        self.values.clear();
    }
}

/// Render state carried top-down: the current timeline frame.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Time(pub f64);

/// Render state carried top-down: the viewer whose output was requested.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct View(pub String);

/// Marks a sequence export, where sources must return final data rather than
/// an interactive loading placeholder.
#[derive(Debug, Clone, Copy)]
pub struct RenderForExport;

/// The active project format, carried top-down so every source cartridge
/// (e.g. `Constant`) produces rasters at the project resolution instead of
/// its own default format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectFormat {
    pub width: u32,
    pub height: u32,
}

impl ProjectFormat {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Logical-to-proxy resolution multiplier (`1.0` = full 1:1, `0.5` = 1:2,
/// `0.25` = 1:4). Stored in [`EvalContext`] so every cartridge reads the same
/// requested scale and hashes it into its `OpHash`, keeping full-res and
/// proxy-res cache entries distinct.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProxyScale(pub f32);

impl ProxyScale {
    /// Clamps finite scales to `0.1..=1.0`; non-finite scales fall back to
    /// `1.0`. Callers always pass user input through here before storing.
    pub fn sanitized(scale: f32) -> f32 {
        if scale.is_finite() {
            scale.clamp(0.1, 1.0)
        } else {
            1.0
        }
    }

    /// Rounds `dim * scale` to a whole pixel, floored to at least `1`.
    pub fn scale_dim(dim: u32, scale: f32) -> u32 {
        ((dim as f32 * scale).round() as u32).max(1)
    }
}

impl Default for ProxyScale {
    fn default() -> Self {
        Self(1.0)
    }
}

/// Interpolation mode for a keyframe segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterpMode {
    /// Nuke's default smooth/cubic segment (`'s'`, `'S'`, `'c'`, `'C'`).
    #[default]
    Smooth,
    /// Straight linear interpolation (`'l'`, `'L'`).
    Linear,
    /// Step/hold the value until the next keyframe (`'k'`, `'K'`).
    Constant,
}

/// A single keyframe sample on an [`AnimationCurve`].
#[derive(Debug, Clone, PartialEq)]
pub struct Keyframe {
    /// Frame at which this key sits.
    pub frame: f64,
    /// Value at this frame.
    pub value: f64,
    /// Interpolation used from this key to the next one.
    pub interp: InterpMode,
}

/// A frame-sorted 1D animation curve for a knob channel.
///
/// The kernel stores one curve per animated knob channel and evaluates them at
/// pull time, so a cartridge always receives concrete knob values in its
/// [`EvalContext`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnimationCurve {
    /// Keyframes, always sorted ascending by `frame`.
    pub keys: Vec<Keyframe>,
}

const KEYFRAME_EPSILON: f64 = 1e-6;
const SPAN_EPSILON: f64 = 1e-9;

impl AnimationCurve {
    /// Creates an empty curve.
    pub fn new() -> Self {
        Self { keys: Vec::new() }
    }

    /// Creates a 1-key constant curve at frame `1.0`, used when a multi-channel
    /// knob mixes animated `{curve ...}` channels with static numeric channels.
    pub fn from_constant(value: f64) -> Self {
        let mut curve = Self::new();
        curve.keys.push(Keyframe {
            frame: 1.0,
            value,
            interp: InterpMode::Constant,
        });
        curve
    }

    /// Returns `true` if the curve holds no keyframes.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Returns the number of keyframes.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Inserts or overwrites a keyframe, keeping `keys` sorted ascending by
    /// frame. Non-finite frames or values are ignored.
    pub fn set_key(&mut self, frame: f64, value: f64, interp: InterpMode) {
        if !frame.is_finite() || !value.is_finite() {
            return;
        }
        if let Some(existing) = self
            .keys
            .iter_mut()
            .find(|key| (key.frame - frame).abs() <= KEYFRAME_EPSILON)
        {
            existing.value = value;
            existing.interp = interp;
            return;
        }
        self.keys.push(Keyframe {
            frame,
            value,
            interp,
        });
        self.keys.sort_by(|a, b| a.frame.total_cmp(&b.frame));
    }

    /// Removes the keyframe nearest to `frame`, if one sits within epsilon.
    /// Returns `true` when a keyframe was removed.
    pub fn remove_key(&mut self, frame: f64) -> bool {
        let before = self.keys.len();
        self.keys
            .retain(|key| (key.frame - frame).abs() > KEYFRAME_EPSILON);
        self.keys.len() != before
    }

    /// Returns `true` if a keyframe sits within epsilon of `frame`.
    pub fn has_key_at(&self, frame: f64) -> bool {
        self.keys
            .iter()
            .any(|key| (key.frame - frame).abs() <= KEYFRAME_EPSILON)
    }

    /// Returns deduplicated, rounded keyframe times in ascending order, used
    /// for timeline keyframe tick rendering.
    pub fn keyframe_times(&self) -> Vec<i64> {
        let mut times: Vec<i64> = self
            .keys
            .iter()
            .map(|key| key.frame.round() as i64)
            .collect();
        times.sort_unstable();
        times.dedup();
        times
    }

    /// Evaluates the curve at `frame`, clamping to the end keyframes outside
    /// the curve's time range.
    pub fn evaluate(&self, frame: f64) -> f64 {
        if self.keys.is_empty() {
            return 0.0;
        }
        if self.keys.len() == 1 || frame <= self.keys[0].frame {
            return self.keys[0].value;
        }
        let last = self.keys.last().expect("non-empty curve");
        if frame >= last.frame {
            return last.value;
        }
        let Some((k0, k1)) = self.keys.windows(2).find_map(|window| {
            let (k0, k1) = (&window[0], &window[1]);
            if k0.frame <= frame && frame <= k1.frame {
                Some((k0, k1))
            } else {
                None
            }
        }) else {
            return 0.0;
        };
        let span = k1.frame - k0.frame;
        if span.abs() <= SPAN_EPSILON {
            return k0.value;
        }
        let t = ((frame - k0.frame) / span).clamp(0.0, 1.0);
        match k0.interp {
            InterpMode::Constant => k0.value,
            InterpMode::Linear => k0.value + (k1.value - k0.value) * t,
            InterpMode::Smooth => {
                let s = t * t * (3.0 - 2.0 * t);
                k0.value + (k1.value - k0.value) * s
            }
        }
    }

    /// Serializes the curve into Nuke TCL syntax, e.g. `"{curve x1 0 x10 100}"`.
    /// Emits `l`/`k` mode tokens for non-smooth interpolation and formats
    /// integer-valued floats without trailing decimal noise.
    pub fn to_nuke_tcl(&self) -> String {
        if self.keys.is_empty() {
            return String::new();
        }
        let mut out = String::from("{curve");
        for key in &self.keys {
            match key.interp {
                InterpMode::Linear => out.push_str(" l"),
                InterpMode::Constant => out.push_str(" k"),
                InterpMode::Smooth => {}
            }
            out.push_str(&format!(
                " x{} {}",
                format_curve_scalar(key.frame),
                format_curve_scalar(key.value)
            ));
        }
        out.push('}');
        out
    }
}

/// Formats a scalar curve value cleanly: integer-valued floats lose their
/// trailing `.0`, other values keep a bounded fractional part.
fn format_curve_scalar(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    if value.fract() == 0.0 && value.abs() < 1e12 {
        return format!("{:.0}", value);
    }
    let text = format!("{:.6}", value);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Generic data travelling bottom-up from a node to its consumers.
#[derive(Debug, Clone)]
pub struct Packet {
    data: TypeMap,
}

impl Default for Packet {
    fn default() -> Self {
        Self::new()
    }
}

impl Packet {
    /// Creates an empty packet.
    pub fn new() -> Self {
        Self {
            data: TypeMap::new(),
        }
    }

    /// Creates an empty packet with room for `capacity` values.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: TypeMap::with_capacity(capacity),
        }
    }

    /// Inserts a value, returning the previous value of the same type, if any.
    pub fn insert<T: Any + Send + Sync + Clone>(&mut self, value: T) -> Option<T> {
        self.data.insert(value)
    }

    /// Borrows the value of type `T`.
    pub fn get<T: Any + Send + Sync + Clone>(&self) -> Option<&T> {
        self.data.get()
    }

    /// Mutably borrows the value of type `T`.
    pub fn get_mut<T: Any + Send + Sync + Clone>(&mut self) -> Option<&mut T> {
        self.data.get_mut()
    }

    /// Removes and returns the value of type `T`.
    pub fn remove<T: Any + Send + Sync + Clone>(&mut self) -> Option<T> {
        self.data.remove()
    }

    /// Returns `true` if the packet holds a value of type `T`.
    pub fn contains<T: Any + Send + Sync + Clone>(&self) -> bool {
        self.data.contains::<T>()
    }

    /// Number of values in the packet.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns `true` if the packet holds no values.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Drops every value from the packet.
    pub fn clear(&mut self) {
        self.data.clear();
    }
}

/// Render context travelling top-down from the viewer into the DAG.
#[derive(Debug, Clone)]
pub struct EvalContext {
    data: TypeMap,
}

impl Default for EvalContext {
    fn default() -> Self {
        Self::new()
    }
}

impl EvalContext {
    /// Creates an empty context.
    pub fn new() -> Self {
        Self {
            data: TypeMap::new(),
        }
    }

    /// Inserts a value, returning the previous value of the same type, if any.
    pub fn insert<T: Any + Send + Sync + Clone>(&mut self, value: T) -> Option<T> {
        self.data.insert(value)
    }

    /// Borrows the value of type `T`.
    pub fn get<T: Any + Send + Sync + Clone>(&self) -> Option<&T> {
        self.data.get()
    }

    /// Mutably borrows the value of type `T`.
    pub fn get_mut<T: Any + Send + Sync + Clone>(&mut self) -> Option<&mut T> {
        self.data.get_mut()
    }

    /// Removes and returns the value of type `T`.
    pub fn remove<T: Any + Send + Sync + Clone>(&mut self) -> Option<T> {
        self.data.remove()
    }

    /// Returns `true` if the context holds a value of type `T`.
    pub fn contains<T: Any + Send + Sync + Clone>(&self) -> bool {
        self.data.contains::<T>()
    }

    /// The requested frame time, if one was provided.
    pub fn time(&self) -> Option<f64> {
        self.get::<Time>().map(|time| time.0)
    }

    /// The requested viewer, if one was provided.
    pub fn view(&self) -> Option<&str> {
        self.get::<View>().map(|view| view.0.as_str())
    }

    /// The live knobs of the node currently being evaluated, if injected.
    pub fn knobs(&self) -> Option<&KnobTable> {
        self.get::<KnobTable>()
    }

    /// The kernel's memory governor, if injected.
    ///
    /// Cartridges allocating tile buffers reserve from this governor so the
    /// kernel can enforce its cap before the allocator runs.
    pub fn governor(&self) -> Option<&MemoryGovernor> {
        self.get::<MemoryGovernor>()
    }

    /// The active project format, if one was provided.
    pub fn project_format(&self) -> Option<ProjectFormat> {
        self.get::<ProjectFormat>().copied()
    }

    /// The requested proxy scale, defaulting to `1.0` (full resolution) when
    /// absent so every existing pull defaults to `1:1`.
    pub fn proxy_scale(&self) -> f32 {
        let scale = ProxyScale::sanitized(self.get::<ProxyScale>().map_or(1.0, |s| s.0));
        // Treat values in the legacy hash's full-resolution tolerance as 1:1
        // in evaluation too, so different pixels cannot share that hash.
        if (scale - 1.0).abs() <= 1e-6 {
            1.0
        } else {
            scale
        }
    }
}

/// Errors a cartridge can report without tearing down the kernel.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NodeError {
    /// An input the node cannot operate without was not supplied.
    #[error("missing required input `{0}`")]
    MissingInput(&'static str),

    /// A knob value was rejected (wrong range, unparsable expression, ...).
    #[error("invalid knob `{knob}`: {reason}")]
    InvalidKnob {
        /// Name of the offending knob.
        knob: String,
        /// Why the value was rejected.
        reason: String,
    },

    /// A memory reservation would push the kernel past its cap.
    #[error("memory limit exceeded: requested {requested} bytes, {available} available")]
    MemoryExceeded {
        /// Bytes the caller asked for.
        requested: usize,
        /// Bytes still available when the request was made.
        available: usize,
    },

    /// The requested node is not in the graph.
    #[error("unknown node {0}")]
    UnknownNode(u64),

    /// A node with this id is already in the graph.
    #[error("node {0} already exists")]
    NodeAlreadyExists(u64),

    /// Connecting the two nodes would close a loop in the DAG.
    #[error("connecting node {from} to node {to} would create a cycle")]
    CycleDetected {
        /// Upstream node of the rejected edge.
        from: u64,
        /// Downstream node of the rejected edge.
        to: u64,
    },

    /// The node has no cartridge attached, so it cannot be evaluated.
    #[error("node {0} has no cartridge implementation")]
    NoImplementation(u64),

    /// A newer render was requested; this evaluation is stale.
    #[error("render preempted by a newer request")]
    Preempted,

    /// The node's own code panicked; captured by the airbag in the kernel.
    #[error("node panicked during evaluation: {0}")]
    Panic(String),

    /// The node's state could not be written back to `.nk` syntax.
    #[error("serialization failed: {0}")]
    Serialization(String),

    /// Any other cartridge-specific failure.
    #[error("{0}")]
    Other(String),

    /// A non-graph operation (e.g. disk write) failed.
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
}

/// The cartridge contract implemented by every node.
///
/// The kernel never inspects a node's internals; it only calls these two
/// methods. Implementations are registered at compile time via `inventory`.
pub trait NodeTrait: Send + Sync {
    /// Pulls data from `inputs`, using `ctx` for render state, and returns the
    /// resulting packet. Must not mutate the node.
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError>;

    /// Declares which connected input ports to pull and the `EvalContext` for
    /// each port.
    ///
    /// Defaults to pulling all connected ports with `ctx` unchanged. Cartridges
    /// that only read some inputs (a `Switch`) override this to skip the
    /// inactive branch, and time-warping cartridges (a `TimeOffset`) rewrite the
    /// context for the port they request.
    fn upstream_inputs(
        &self,
        ctx: &EvalContext,
        connected_ports: &[PortId],
    ) -> Vec<(PortId, EvalContext)> {
        connected_ports
            .iter()
            .map(|&port| (port, ctx.clone()))
            .collect()
    }

    /// Writes the node's state back into `.nk` / TCL syntax.
    fn serialize_to_tcl(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_round_trips_values_by_type() {
        let mut packet = Packet::new();
        assert!(packet.is_empty());

        packet.insert(42u32);
        packet.insert("pixels".to_string());

        assert_eq!(packet.get::<u32>(), Some(&42));
        assert_eq!(packet.get::<String>().map(String::as_str), Some("pixels"));
        assert_eq!(packet.get::<f32>(), None);
        assert_eq!(packet.len(), 2);

        assert_eq!(packet.remove::<u32>(), Some(42));
        assert!(!packet.contains::<u32>());
    }

    #[test]
    fn eval_context_exposes_render_state() {
        let mut ctx = EvalContext::new();
        ctx.insert(Time(1.5));
        ctx.insert(View("main".to_string()));

        assert_eq!(ctx.time(), Some(1.5));
        assert_eq!(ctx.view(), Some("main"));
    }

    #[test]
    fn proxy_scale_defaults_to_full_and_sanitizes_context_values() {
        let mut ctx = EvalContext::new();
        assert_eq!(ctx.proxy_scale(), 1.0);
        for (value, expected) in [
            (f32::NAN, 1.0),
            (f32::INFINITY, 1.0),
            (1.5, 1.0),
            (0.0, 0.1),
            (0.25, 0.25),
        ] {
            ctx.insert(ProxyScale(value));
            assert_eq!(ctx.proxy_scale(), expected);
        }
        assert_eq!(ProxyScale::scale_dim(65, 0.5), 33);
        assert_eq!(ProxyScale::scale_dim(1, 0.25), 1);
    }

    #[test]
    fn type_map_blackboards_are_thread_safe_cloneable_and_removable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Packet>();
        assert_send_sync::<EvalContext>();
        assert_send_sync::<TypeMap>();

        let mut packet = Packet::new();
        packet.insert(7u32);
        packet.insert("shared".to_string());

        let cloned = packet.clone();
        assert_eq!(cloned.get::<u32>(), Some(&7));
        assert_eq!(cloned.get::<String>().map(String::as_str), Some("shared"));

        // Mutating the clone leaves the original untouched.
        let mut cloned = cloned;
        cloned.insert(9u32);
        assert_eq!(cloned.get::<u32>(), Some(&9));
        assert_eq!(packet.get::<u32>(), Some(&7));

        // Removal still works after a clone, on both copies independently.
        assert_eq!(cloned.remove::<u32>(), Some(9));
        assert!(!cloned.contains::<u32>());
        assert_eq!(packet.remove::<u32>(), Some(7));
        assert!(!packet.contains::<u32>());
        assert_eq!(packet.get::<String>().map(String::as_str), Some("shared"));

        let mut ctx = EvalContext::new();
        ctx.insert(Time(3.0));
        let mut ctx_clone = ctx.clone();
        ctx_clone.insert(Time(4.0));
        assert_eq!(ctx.time(), Some(3.0));
        assert_eq!(ctx_clone.time(), Some(4.0));
        assert_eq!(ctx_clone.remove::<Time>().map(|time| time.0), Some(4.0));
        assert!(ctx_clone.time().is_none());
        assert_eq!(ctx.time(), Some(3.0));
    }
}
