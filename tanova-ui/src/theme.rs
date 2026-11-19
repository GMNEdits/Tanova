//! Studio theme engine: Classic, Slate, and Midnight palettes for node cards,
//! canvas backgrounds, wires, and input pins.

use egui::Color32;

/// Available studio visual themes (brand-free names).
///
/// `Classic` is familiar to Foundry Nuke artists, `Slate` to Natron artists,
/// and `Midnight` to Blackmagic Fusion artists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StudioTheme {
    #[default]
    Classic,
    Slate,
    Midnight,
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
        "Constant" | "Grade" | "ColorCorrect" | "Invert" | "Premult" | "Unpremult" | "Clamp"
        | "Saturation" | "CheckerBoard2" => NodeCategory::Color,
        "Merge2" | "Switch" | "Shuffle" => NodeCategory::Merge,
        "Blur" | "TimeOffset" | "FrameHold" => NodeCategory::Filter,
        "Transform" | "Crop" | "Reformat" => NodeCategory::Transform,
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
    /// Fill for interactive widgets that sit on the panel background
    /// (slider rails, checkbox bodies, slider handles). Must contrast clearly
    /// with [`ThemePalette::panel_bg`], or the rails vanish into the panel.
    pub widget_bg: Color32,
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
    /// When `true`, the node card body is filled with the category color
    /// (Classic full-body cards). When `false`, a dark body with a top header
    /// band is used (Slate / Midnight two-tone cards).
    pub full_body_category_fill: bool,
    /// Theme-coordinated color for the active viewer node ring and the
    /// `VIEWER 1` / `VIEWER 2` pill badge.
    pub viewer_badge_accent: Color32,
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
        StudioTheme::Classic => ThemePalette {
            // Classic: Nuke muscle-memory look, full-body category cards.
            panel_bg: Color32::from_rgb(43, 43, 43),
            canvas_bg: Color32::from_rgb(58, 58, 58),
            grid_line: Color32::from_rgb(70, 70, 70),
            use_dot_grid: false,
            card_body_bg: Color32::from_rgb(34, 36, 40),
            card_border: Color32::from_rgb(20, 20, 20),
            card_shadow: Color32::from_black_alpha(130),
            selected_rim: Color32::from_rgb(247, 147, 30),
            card_corner_radius: 2,
            widget_bg: Color32::from_rgb(74, 76, 82),
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
            full_body_category_fill: true,
            viewer_badge_accent: Color32::from_rgb(229, 169, 59),
        },
        StudioTheme::Slate => ThemePalette {
            // Slate: Natron-inspired high-contrast dark, two-tone header cards.
            panel_bg: Color32::from_rgb(24, 25, 28),
            canvas_bg: Color32::from_rgb(33, 35, 40),
            grid_line: Color32::from_rgb(45, 48, 55),
            use_dot_grid: false,
            card_body_bg: Color32::from_rgb(24, 26, 31),
            card_border: Color32::from_rgb(16, 16, 18),
            card_shadow: Color32::from_black_alpha(120),
            selected_rim: Color32::from_rgb(230, 126, 34),
            card_corner_radius: 3,
            widget_bg: Color32::from_rgb(55, 60, 70),
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
            full_body_category_fill: false,
            viewer_badge_accent: Color32::from_rgb(140, 194, 59),
        },
        StudioTheme::Midnight => ThemePalette {
            // Midnight: Fusion-inspired deep gunmetal dot grid, rounded capsules.
            panel_bg: Color32::from_rgb(19, 19, 24),
            canvas_bg: Color32::from_rgb(13, 13, 18),
            grid_line: Color32::from_rgb(42, 42, 56),
            use_dot_grid: true,
            card_body_bg: Color32::from_rgb(29, 29, 36),
            card_border: Color32::from_rgb(10, 10, 14),
            card_shadow: Color32::from_black_alpha(100),
            selected_rim: Color32::from_rgb(58, 130, 246),
            card_corner_radius: 7,
            widget_bg: Color32::from_rgb(50, 54, 68),
            wire_default: Color32::from_rgb(148, 163, 184),
            pin_b: Color32::from_rgb(250, 204, 21),
            pin_a: Color32::from_rgb(74, 222, 128),
            pin_m: Color32::from_rgb(192, 132, 252),
            color_io: Color32::from_rgb(35, 133, 122),
            color_color: Color32::from_rgb(94, 84, 142),
            color_merge: Color32::from_rgb(184, 107, 32),
            color_filter: Color32::from_rgb(181, 105, 50),
            color_transform: Color32::from_rgb(139, 63, 184),
            color_viewer: Color32::from_rgb(56, 189, 248),
            color_other: Color32::from_rgb(74, 78, 105),
            full_body_category_fill: false,
            viewer_badge_accent: Color32::from_rgb(56, 189, 248),
        },
    }
}

/// Resolves a `BackdropNode`'s `"color"` knob preset to a tint color.
pub fn backdrop_tint_color(color: &str) -> Color32 {
    match color {
        "slate" => Color32::from_rgb(90, 100, 110),
        "blue" => Color32::from_rgb(60, 90, 160),
        "teal" => Color32::from_rgb(40, 120, 120),
        "amber" => Color32::from_rgb(180, 140, 60),
        "purple" => Color32::from_rgb(140, 80, 160),
        "crimson" => Color32::from_rgb(180, 60, 70),
        "green" => Color32::from_rgb(80, 140, 80),
        _ => Color32::from_rgb(90, 100, 110),
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
        // Interactive fills must read as recessed tracks against the panel.
        // Setting this to `panel_bg` (the old behavior) erased every slider
        // rail, leaving only a floating thumb on `mix`, `gain`, and `gamma`.
        visuals.widgets.inactive.bg_fill = pal.widget_bg;
        visuals.widgets.inactive.weak_bg_fill = pal.widget_bg;
        // Hover/press step brighter so the thumb and buttons still respond.
        visuals.widgets.hovered.bg_fill = pal.widget_bg.gamma_multiply(1.25);
        visuals.widgets.hovered.weak_bg_fill = pal.widget_bg.gamma_multiply(1.25);
        visuals.widgets.active.bg_fill = pal.widget_bg.gamma_multiply(1.5);
        visuals.widgets.active.weak_bg_fill = pal.widget_bg.gamma_multiply(1.5);
        // Light the filled portion of a slider rail behind the thumb.
        visuals.selection.bg_fill = pal.selected_rim;
        visuals.slider_trailing_fill = true;
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
        assert_eq!(node_category("ColorCorrect"), NodeCategory::Color);
        assert_eq!(node_category("Invert"), NodeCategory::Color);
        assert_eq!(node_category("Premult"), NodeCategory::Color);
        assert_eq!(node_category("Unpremult"), NodeCategory::Color);
        assert_eq!(node_category("Merge2"), NodeCategory::Merge);
        assert_eq!(node_category("Blur"), NodeCategory::Filter);
        assert_eq!(node_category("Transform"), NodeCategory::Transform);
        assert_eq!(node_category("Viewer"), NodeCategory::Viewer);
        assert_eq!(node_category("UnknownGizmo"), NodeCategory::Other);

        for theme in [
            StudioTheme::Classic,
            StudioTheme::Slate,
            StudioTheme::Midnight,
        ] {
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

        assert!(palette(StudioTheme::Midnight).use_dot_grid);
        assert!(!palette(StudioTheme::Classic).use_dot_grid);
        assert!(!palette(StudioTheme::Slate).use_dot_grid);

        // Distinct silhouettes.
        assert!(palette(StudioTheme::Classic).full_body_category_fill);
        assert!(!palette(StudioTheme::Slate).full_body_category_fill);
        assert!(!palette(StudioTheme::Midnight).full_body_category_fill);

        // Palettes must differ between themes.
        assert_ne!(
            palette(StudioTheme::Classic).canvas_bg,
            palette(StudioTheme::Slate).canvas_bg
        );
        assert_ne!(
            palette(StudioTheme::Slate).canvas_bg,
            palette(StudioTheme::Midnight).canvas_bg
        );
        assert_ne!(
            palette(StudioTheme::Classic).viewer_badge_accent,
            palette(StudioTheme::Midnight).viewer_badge_accent
        );
        assert_ne!(
            palette(StudioTheme::Slate).viewer_badge_accent,
            palette(StudioTheme::Midnight).viewer_badge_accent
        );
    }

    #[test]
    fn classic_slate_and_midnight_palettes_have_distinct_card_and_grid_styles() {
        // Classic: full-body category cards.
        assert!(palette(StudioTheme::Classic).full_body_category_fill);
        assert!(!palette(StudioTheme::Classic).use_dot_grid);

        // Slate: two-tone header cards, no dot grid.
        assert!(!palette(StudioTheme::Slate).full_body_category_fill);
        assert!(!palette(StudioTheme::Slate).use_dot_grid);

        // Midnight: two-tone header cards, dot grid.
        assert!(!palette(StudioTheme::Midnight).full_body_category_fill);
        assert!(palette(StudioTheme::Midnight).use_dot_grid);

        // All three themes have distinct canvas_bg and viewer_badge_accent.
        let classic_bg = palette(StudioTheme::Classic).canvas_bg;
        let slate_bg = palette(StudioTheme::Slate).canvas_bg;
        let midnight_bg = palette(StudioTheme::Midnight).canvas_bg;
        assert_ne!(classic_bg, slate_bg);
        assert_ne!(slate_bg, midnight_bg);
        assert_ne!(classic_bg, midnight_bg);

        let classic_accent = palette(StudioTheme::Classic).viewer_badge_accent;
        let slate_accent = palette(StudioTheme::Slate).viewer_badge_accent;
        let midnight_accent = palette(StudioTheme::Midnight).viewer_badge_accent;
        assert_ne!(classic_accent, slate_accent);
        assert_ne!(slate_accent, midnight_accent);
        assert_ne!(classic_accent, midnight_accent);
    }

    #[test]
    fn every_theme_paints_a_visible_slider_rail_and_trailing_fill() {
        fn luminance(color: Color32) -> f32 {
            0.299 * color.r() as f32 + 0.587 * color.g() as f32 + 0.114 * color.b() as f32
        }

        for theme in [
            StudioTheme::Classic,
            StudioTheme::Slate,
            StudioTheme::Midnight,
        ] {
            let ctx = egui::Context::default();
            apply_theme_visuals(&ctx, theme);
            let visuals = ctx.style_of(ctx.theme()).visuals.clone();
            let pal = palette(theme);

            // The rail uses `inactive.bg_fill`; panels used to overwrite it
            // with `panel_bg`, which made every slider track invisible.
            assert_eq!(
                visuals.widgets.inactive.bg_fill, pal.widget_bg,
                "{theme:?}: inactive bg_fill must be the widget track color"
            );
            assert_ne!(
                visuals.widgets.inactive.bg_fill, pal.panel_bg,
                "{theme:?}: slider rail must not match the panel background"
            );
            assert!(
                (luminance(pal.widget_bg) - luminance(pal.panel_bg)).abs() >= 12.0,
                "{theme:?}: slider rail needs visible luminance contrast"
            );
            assert!(
                visuals.slider_trailing_fill,
                "{theme:?}: the filled part of the slider rail must be painted"
            );
        }
    }
}
