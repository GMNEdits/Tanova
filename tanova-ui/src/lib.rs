//! Stateless Tanova UI layer.
//!
//! Widgets never mutate application state directly; they emit
//! `UndoableCommand`s for the kernel message bus. The node graph is drawn on
//! a custom `egui::Painter` with top-to-bottom B-pipe topology.

pub mod canvas;
pub mod properties;
pub mod viewer;

pub use canvas::{CanvasEdge, CanvasNode, GraphMirror, NodeCanvas, NodeCanvasState, WireDrag};
pub use properties::PropertiesPanel;
pub use viewer::ViewerPanel;
