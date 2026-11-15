//! Studio theme engine: Nuke, Natron, and Fusion palettes for node cards,
//! canvas backgrounds, wires, and input pins.

use egui::Color32;

/// Available studio visual themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StudioTheme {
    #[default]
    Nuke,
    Natron,
    Fusion,
}

/// Functional category of a node cartridge, used for card header band color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeCategory {
    Io,        // Read, Write
    Color,     // Constant, Grade
    Merge,     // Merge2
    Filter,    // Blur
    Transform, // Transform
    Viewer,    // Viewer
    Other,     // Unknown / passthrough classes
}

/// Maps a cartridge kind to its node category.
pub fn node_category(kind: &str) -> NodeCategory {
    match kind {
        "Read" | "Write" => NodeCategory::Io,
        "Constant" | "Grade" => NodeCategory::Color,
        "Merge2" => NodeCategory::Merge,
        "Blur" => NodeCategory::Filter,
        "Transform" => NodeCategory::Transform,
        "Viewer" => NodeCategory::Viewer,
        _ => NodeCategory::Other,
    }
}

/// Complete color and geometry palette for one studio theme.
#[derive(Debug, Clone, Copy)]
pub struct ThemePalette {
    pub panel_bg: Color32,
    pub canvas_bg: Color32,
    pub grid_line: Color32,
    pub use_dot_grid: bool,
    pub card_body_bg: Color32,
    pub card_border: Color32,
    pub card_shadow: Color32,
    pub selected_rim: Color32,
    pub card_corner_radius: u8,
    pub wire_default: Color32,
    pub pin_b: Color32,
    pub pin_a: Color32,
    pub pin_m: Color32,
    pub color_io: Color32,
    pub color_color: Color32,
    pub color_merge: Color32,
    pub color_filter: Color32,
    pub color_transform: Color32,
    pub color_viewer: Color32,
    pub color_other: Color32,
}

impl ThemePalette {
    /// Category header band color for a node category.
    pub fn category_color(&self, cat: NodeCategory) -> Color32 {
        match cat {
            NodeCategory::Io => self.color_io,
            NodeCategory::Color => self.color_color,
            NodeCategory::Merge => self.color_merge,
            NodeCategory::Filter => self.color_filter,
            NodeCategory::Transform => self.color_transform,
            NodeCategory::Viewer => self.color_viewer,
            NodeCategory::Other => self.color_other,
        }
    }
}

/// Returns the complete palette for a studio theme.
pub fn palette(theme: StudioTheme) -> ThemePalette {
    match theme {
        StudioTheme::Nuke => ThemePalette {
            // Foundry Nuke muscle-memory palette.
            panel_bg: Color32::from_rgb(38, 38, 38),
            canvas_bg: Color32::from_rgb(51, 51, 51),
            grid_line: Color32::from_rgb(61, 61, 61),
            use_dot_grid: false,
            card_body_bg: Color32::from_rgb(34, 36, 40),
            card_border: Color32::from_rgb(20, 20, 20),
            card_shadow: Color32::from_black_alpha(130),
            selected_rim: Color32::from_rgb(247, 147, 30),
            card_corner_radius: 2,
            wire_default: Color32::from_rgb(200, 200, 200),
            pin_b: Color32::from_rgb(230, 230, 230),
            pin_a: Color32::from_rgb(92, 214, 141),
            pin_m: Color32::from_rgb(217, 114, 255),
            color_io: Color32::from_rgb(184, 146, 40),
            color_color: Color32::from_rgb(75, 114, 159),
            color_merge: Color32::from_rgb(56, 78, 130),
            color_filter: Color32::from_rgb(181, 102, 51),
            color_transform: Color32::from_rgb(122, 80, 123),
            color_viewer: Color32::from_rgb(158, 142, 36),
            color_other: Color32::from_rgb(85, 90, 100),
        },
        StudioTheme::Natron => ThemePalette {
            // Natron high-contrast dark palette.
            panel_bg: Color32::from_rgb(31, 31, 34),
            canvas_bg: Color32::from_rgb(40, 41, 45),
            grid_line: Color32::from_rgb(51, 53, 58),
            use_dot_grid: false,
            card_body_bg: Color32::from_rgb(27, 28, 32),
            card_border: Color32::from_rgb(16, 16, 18),
            card_shadow: Color32::from_black_alpha(120),
            selected_rim: Color32::from_rgb(230, 126, 34),
            card_corner_radius: 3,
            wire_default: Color32::from_rgb(208, 211, 216),
            pin_b: Color32::from_rgb(240, 240, 240),
            pin_a: Color32::from_rgb(72, 201, 176),
            pin_m: Color32::from_rgb(195, 155, 211),
            color_io: Color32::from_rgb(166, 138, 36),
            color_color: Color32::from_rgb(64, 107, 144),
            color_merge: Color32::from_rgb(46, 68, 117),
            color_filter: Color32::from_rgb(173, 89, 42),
            color_transform: Color32::from_rgb(112, 69, 115),
            color_viewer: Color32::from_rgb(125, 150, 40),
            color_other: Color32::from_rgb(79, 84, 94),
        },
        StudioTheme::Fusion => ThemePalette {
            // Blackmagic Fusion gunmetal palette.
            panel_bg: Color32::from_rgb(22, 22, 26),
            canvas_bg: Color32::from_rgb(16, 16, 20),
            grid_line: Color32::from_rgb(42, 42, 54),
            use_dot_grid: true,
            card_body_bg: Color32::from_rgb(29, 29, 36),
            card_border: Color32::from_rgb(10, 10, 14),
            card_shadow: Color32::from_black_alpha(100),
            selected_rim: Color32::from_rgb(58, 130, 246),
            card_corner_radius: 6,
            wire_default: Color32::from_rgb(148, 163, 184),
            pin_b: Color32::from_rgb(250, 204, 21),
            pin_a: Color32::from_rgb(74, 222, 128),
            pin_m: Color32::from_rgb(192, 132, 252),
            color_io: Color32::from_rgb(35, 133, 122),
            color_color: Color32::from_rgb(94, 84, 142),
            color_merge: Color32::from_rgb(184, 107, 32),
            color_filter: Color32::from_rgb(181, 105, 50),
            color_transform: Color32::from_rgb(139, 63, 184),
            color_viewer: Color32::from_rgb(34, 139, 230),
            color_other: Color32::from_rgb(74, 78, 105),
        },
    }
}

/// Applies theme-derived `egui` visuals to the context.
///
/// Only calls `ctx.set_visuals(...)` when `panel_fill` actually changes, so
/// we never thrash egui's layout/style cache on every 60 FPS frame.
pub fn apply_theme_visuals(ctx: &egui::Context, theme: StudioTheme) {
    let pal = palette(theme);
    let egui_theme = ctx.theme();
    if ctx.style_of(egui_theme).visuals.panel_fill != pal.panel_bg {
        let mut visuals = ctx.style_of(egui_theme).visuals.clone();
        visuals.panel_fill = pal.panel_bg;
        visuals.window_fill = pal.panel_bg;
        visuals.widgets.inactive.bg_fill = pal.panel_bg;
        ctx.set_visuals(visuals);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_categories_map_all_built_in_cartridges_and_palettes_differ() {
        assert_eq!(node_category("Read"), NodeCategory::Io);
        assert_eq!(node_category("Write"), NodeCategory::Io);
        assert_eq!(node_category("Constant"), NodeCategory::Color);
        assert_eq!(node_category("Grade"), NodeCategory::Color);
        assert_eq!(node_category("Merge2"), NodeCategory::Merge);
        assert_eq!(node_category("Blur"), NodeCategory::Filter);
        assert_eq!(node_category("Transform"), NodeCategory::Transform);
        assert_eq!(node_category("Viewer"), NodeCategory::Viewer);
        assert_eq!(node_category("UnknownGizmo"), NodeCategory::Other);

        for theme in [StudioTheme::Nuke, StudioTheme::Natron, StudioTheme::Fusion] {
            let pal = palette(theme);
            let io = pal.category_color(NodeCategory::Io);
            let color = pal.category_color(NodeCategory::Color);
            let merge = pal.category_color(NodeCategory::Merge);
            let filter = pal.category_color(NodeCategory::Filter);
            let transform = pal.category_color(NodeCategory::Transform);

            assert_ne!(io, color, "{theme:?}: Io and Color must differ");
            assert_ne!(io, merge, "{theme:?}: Io and Merge must differ");
            assert_ne!(io, filter, "{theme:?}: Io and Filter must differ");
            assert_ne!(io, transform, "{theme:?}: Io and Transform must differ");
            assert_ne!(color, merge, "{theme:?}: Color and Merge must differ");
            assert_ne!(color, filter, "{theme:?}: Color and Filter must differ");
            assert_ne!(
                color, transform,
                "{theme:?}: Color and Transform must differ"
            );
            assert_ne!(merge, filter, "{theme:?}: Merge and Filter must differ");
            assert_ne!(
                merge, transform,
                "{theme:?}: Merge and Transform must differ"
            );
            assert_ne!(
                filter, transform,
                "{theme:?}: Filter and Transform must differ"
            );
        }

        assert!(palette(StudioTheme::Fusion).use_dot_grid);
        assert!(!palette(StudioTheme::Nuke).use_dot_grid);
        assert!(!palette(StudioTheme::Natron).use_dot_grid);

        // Palettes must differ between themes.
        assert_ne!(
            palette(StudioTheme::Nuke).panel_bg,
            palette(StudioTheme::Natron).panel_bg
        );
        assert_ne!(
            palette(StudioTheme::Natron).panel_bg,
            palette(StudioTheme::Fusion).panel_bg
        );
        assert_ne!(
            palette(StudioTheme::Nuke).selected_rim,
            palette(StudioTheme::Fusion).selected_rim
        );
    }
}
