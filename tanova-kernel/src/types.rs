//! Blackboard types shared by the kernel and every cartridge.
//!
//! Data flows through the DAG in two directions:
//!
//! * [`Packet`] flows **bottom-up** (node output to downstream inputs).
//! * [`EvalContext`] flows **top-down** (viewer request to upstream nodes).
//!
//! Both are type maps: any `Send + Sync` value can be stored under its own
//! type, which keeps the kernel blind to VFX-specific data.

use std::any::Any;

/// The backing type map: one value per concrete type, safe to move between
/// the UI and the kernel thread.
///
/// anymap keys on its own `Any` trait (a `std`-`Any` extension), which is what
/// makes the `Send + Sync` bounds expressible.
pub type TypeMap = anymap::Map<dyn anymap::any::Any + Send + Sync>;

/// Render state carried top-down: the frame time in seconds.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Time(pub f64);

/// Render state carried top-down: the viewer whose output was requested.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct View(pub String);

/// Generic data travelling bottom-up from a node to its consumers.
#[derive(Debug)]
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
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) -> Option<T> {
        self.data.insert(value)
    }

    /// Borrows the value of type `T`.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.data.get()
    }

    /// Mutably borrows the value of type `T`.
    pub fn get_mut<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        self.data.get_mut()
    }

    /// Removes and returns the value of type `T`.
    pub fn remove<T: Any + Send + Sync>(&mut self) -> Option<T> {
        self.data.remove()
    }

    /// Returns `true` if the packet holds a value of type `T`.
    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
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
#[derive(Debug)]
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
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) -> Option<T> {
        self.data.insert(value)
    }

    /// Borrows the value of type `T`.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.data.get()
    }

    /// Mutably borrows the value of type `T`.
    pub fn get_mut<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        self.data.get_mut()
    }

    /// Removes and returns the value of type `T`.
    pub fn remove<T: Any + Send + Sync>(&mut self) -> Option<T> {
        self.data.remove()
    }

    /// Returns `true` if the context holds a value of type `T`.
    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
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

    /// The node's own code panicked; captured by the airbag in the kernel.
    #[error("node panicked during evaluation: {0}")]
    Panic(String),

    /// The node's state could not be written back to `.nk` syntax.
    #[error("serialization failed: {0}")]
    Serialization(String),

    /// Any other cartridge-specific failure.
    #[error("{0}")]
    Other(String),
}

/// The cartridge contract implemented by every node.
///
/// The kernel never inspects a node's internals; it only calls these two
/// methods. Implementations are registered at compile time via `inventory`.
pub trait NodeTrait: Send + Sync {
    /// Pulls data from `inputs`, using `ctx` for render state, and returns the
    /// resulting packet. Must not mutate the node.
    fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError>;

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
}
