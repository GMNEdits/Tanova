//! Viewer paradigms the application can present.
//!
//! Tanova supports both the classic Nuke workflow (a physical `Viewer` node in
//! the graph) and the Fusion workflow (a transient viewer target highlighted in
//! the canvas). The mode only changes how the UI routes the viewer; the kernel
//! and the render pipeline are identical.

/// Which viewer workflow the UI is presenting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewerWorkflow {
    /// Nuke: `1` routes through a physical `Viewer` node that the kernel wires
    /// and that appears in the canvas.
    #[default]
    Nuke,
    /// Fusion: `1` binds the transient viewer directly to the selected node and
    /// the canvas draws a `VIEWER 1` badge on it.
    Fusion,
}

impl ViewerWorkflow {
    /// Short label used by the top-bar toggle.
    pub fn label(self) -> &'static str {
        match self {
            Self::Nuke => "Nuke Node",
            Self::Fusion => "Fusion Target",
        }
    }

    /// Whether the canvas may spawn a physical `Viewer` node.
    pub fn exposes_physical_viewer(self) -> bool {
        matches!(self, Self::Nuke)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_the_nuke_workflow() {
        assert_eq!(ViewerWorkflow::default(), ViewerWorkflow::Nuke);
        assert!(ViewerWorkflow::Nuke.exposes_physical_viewer());
        assert!(!ViewerWorkflow::Fusion.exposes_physical_viewer());
    }
}
