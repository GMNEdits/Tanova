//! Auto-generated properties bin for the selected node.
//!
//! The panel is stateless: it reads the UI-side [`GraphMirror`] and returns
//! `KernelMessage`s. Every edit carries the previous value so the kernel's undo
//! stack stays mechanical.

use egui::{Color32, ComboBox, DragValue, TextEdit, Ui};
use tanova_kernel::{KernelMessage, KnobValue, NodeId, UndoableCommand};

use crate::canvas::GraphMirror;

const VIEWER: Color32 = Color32::from_rgb(255, 190, 40);

/// One knob edit session, latched in `egui` memory for as long as the artist is
/// typing into or dragging one knob.
///
/// A numeric field commits a value on every keystroke (`'0'` in `0.5` is a real
/// `0.0` for a moment), so the session remembers the value the artist actually
/// started from. Every `SetKnob` emitted during the session reports that value
/// as `previous`, which keeps the whole edit a single undo step.
#[derive(Clone, Debug)]
struct ActiveKnobEdit {
    node: NodeId,
    knob: String,
    initial_value: KnobValue,
}

/// `egui` memory key for the live [`ActiveKnobEdit`].
fn active_knob_edit_id() -> egui::Id {
    egui::Id::unique("tanova_active_knob_edit")
}

#[cfg(test)]
thread_local! {
    // Widget rects captured by the last `PropertiesPanel::show` call. Tests
    // use these to aim pointer events at a named knob without hard-coding the
    // panel's layout.
    static KNOB_WIDGET_RECTS: std::cell::RefCell<
        std::collections::HashMap<(NodeId, String), egui::Rect>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

const OPERATIONS: &[&str] = &[
    "over", "under", "plus", "from", "minus", "multiply", "stencil", "mask",
];
const BBOX_MODES: &[&str] = &["union", "B", "A", "intersection"];

/// Draws controls for every knob on the selected node.
pub struct PropertiesPanel;

impl PropertiesPanel {
    /// Renders the panel and returns the commands the user requested.
    pub fn show(
        ui: &mut Ui,
        graph: &GraphMirror,
        selected: Option<NodeId>,
        viewer: Option<NodeId>,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();

        #[cfg(test)]
        KNOB_WIDGET_RECTS.with(|rects| rects.borrow_mut().clear());

        let Some(node_id) = selected else {
            ui.label("No node selected");
            ui.separator();
            ui.weak("Select a node on the canvas, then press 1 to view it.");
            return messages;
        };
        let Some(node) = graph.node(node_id) else {
            ui.label(format!("Node #{} is not in the graph", node_id.0));
            return messages;
        };

        ui.horizontal(|ui| {
            ui.heading(format!("{} #{}", node.kind, node_id.0));
            ui.separator();
            if ui.button("View (1)").clicked() {
                messages.push(KernelMessage::SetViewerNode(Some(node_id)));
            }
            if viewer == Some(node_id) {
                ui.colored_label(VIEWER, "VIEWER 1");
            }
        });
        ui.separator();

        // A session belongs to the node that was being edited. Switching
        // selection ends it, so a later edit never inherits a stale origin.
        ui.ctx().data_mut(|data| {
            if let Some(active) = data.get_temp::<ActiveKnobEdit>(active_knob_edit_id()) {
                if active.node != node_id {
                    data.remove::<ActiveKnobEdit>(active_knob_edit_id());
                }
            }
        });

        egui::ScrollArea::vertical().show(ui, |ui| {
            // Scope widget IDs by node id so switching selection between two
            // nodes of the same kind never leaks egui widget focus or drag
            // state.
            ui.push_id(node_id, |ui| {
                for (name, value) in node.knobs.iter() {
                    // Value the artist saw before this frame. It becomes the
                    // session origin if this frame starts a new edit.
                    let pre_edit_value = value.clone();
                    let mut next = pre_edit_value.clone();
                    let mut changed = false;
                    let mut row_active = false;
                    let mut widget_rect: Option<egui::Rect> = None;

                    let row = {
                        // Tracks the knob's interactive area and whether it
                        // still owns keyboard focus or an active pointer drag.
                        let mut note = |response: &egui::Response| {
                            widget_rect = Some(
                                widget_rect.map_or(response.rect, |rect| rect.union(response.rect)),
                            );
                            row_active |= response.has_focus() || response.dragged();
                        };

                        ui.horizontal(|ui| {
                            ui.label(name);
                            match value {
                                KnobValue::Float(value) => {
                                    let mut float = *value as f32;
                                    let response = if name == "mix" {
                                        ui.add(
                                            DragValue::new(&mut float).speed(0.01).range(0.0..=1.0),
                                        )
                                    } else {
                                        ui.add(DragValue::new(&mut float).speed(0.01))
                                    };
                                    note(&response);
                                    if response.changed() {
                                        next = KnobValue::Float(float as f64);
                                        changed = true;
                                    }
                                }
                                KnobValue::Int(value) => {
                                    let mut integer = *value;
                                    let response = ui.add(DragValue::new(&mut integer).speed(1.0));
                                    note(&response);
                                    if response.changed() {
                                        next = KnobValue::Int(integer);
                                        changed = true;
                                    }
                                }
                                KnobValue::Bool(value) => {
                                    let mut boolean = *value;
                                    let response = ui.checkbox(&mut boolean, "");
                                    note(&response);
                                    if response.changed() {
                                        next = KnobValue::Bool(boolean);
                                        changed = true;
                                    }
                                }
                                KnobValue::Text(text) => {
                                    let mut edited = text.clone();
                                    let mut combo_changed = false;
                                    let presets = match name {
                                        "operation" => Some(OPERATIONS),
                                        "bbox" => Some(BBOX_MODES),
                                        _ => None,
                                    };
                                    if let Some(presets) = presets {
                                        let combo = ComboBox::from_id_salt((node_id.0, name))
                                            .selected_text(edited.clone())
                                            .show_ui(ui, |ui| {
                                                for preset in presets {
                                                    if ui
                                                        .selectable_value(
                                                            &mut edited,
                                                            (*preset).to_string(),
                                                            *preset,
                                                        )
                                                        .changed()
                                                    {
                                                        combo_changed = true;
                                                    }
                                                }
                                            });
                                        note(&combo.response);
                                    }
                                    let edit = ui.add(
                                        TextEdit::singleline(&mut edited).desired_width(150.0),
                                    );
                                    note(&edit);
                                    if combo_changed || edit.changed() {
                                        next = KnobValue::Text(edited);
                                        changed = true;
                                    }
                                }
                                KnobValue::Color(channels)
                                    if name == "translate" || name == "center" =>
                                {
                                    // 2D spatial knobs keep `Color([x, y, z, w])`
                                    // storage for cartridge compatibility but edit as
                                    // coordinates, never as RGBA.
                                    let mut color = *channels;
                                    ui.label("X");
                                    let x = ui.add(DragValue::new(&mut color[0]).speed(1.0));
                                    note(&x);
                                    if x.changed() {
                                        changed = true;
                                    }
                                    ui.label("Y");
                                    let y = ui.add(DragValue::new(&mut color[1]).speed(1.0));
                                    note(&y);
                                    if y.changed() {
                                        changed = true;
                                    }
                                    if changed {
                                        next = KnobValue::Color(color);
                                    }
                                }
                                KnobValue::Color(channels) => {
                                    let mut color = *channels;
                                    let mut srgba = Color32::from_rgba_unmultiplied(
                                        to_u8(color[0]),
                                        to_u8(color[1]),
                                        to_u8(color[2]),
                                        to_u8(color[3]),
                                    );
                                    let swatch = ui.color_edit_button_srgba(&mut srgba);
                                    note(&swatch);
                                    if swatch.changed() {
                                        color = [
                                            srgba.r() as f32 / 255.0,
                                            srgba.g() as f32 / 255.0,
                                            srgba.b() as f32 / 255.0,
                                            srgba.a() as f32 / 255.0,
                                        ];
                                        changed = true;
                                    }
                                    for (label, channel) in
                                        ["R", "G", "B", "A"].iter().zip(color.iter_mut())
                                    {
                                        ui.label(*label);
                                        let response = ui.add(DragValue::new(channel).speed(0.01));
                                        note(&response);
                                        if response.changed() {
                                            changed = true;
                                        }
                                    }
                                    if changed {
                                        next = KnobValue::Color(color);
                                    }
                                }
                            }
                        })
                    };

                    #[cfg(test)]
                    KNOB_WIDGET_RECTS.with(|rects| {
                        rects.borrow_mut().insert(
                            (node_id, name.to_string()),
                            widget_rect.unwrap_or(row.response.rect),
                        );
                    });
                    #[cfg(not(test))]
                    let _ = (row, widget_rect);

                    if changed && next != pre_edit_value {
                        // Continue the live session if there is one, otherwise
                        // start one from the value the artist saw before this
                        // keystroke/drag.
                        let initial_value = match ui
                            .ctx()
                            .data(|data| data.get_temp::<ActiveKnobEdit>(active_knob_edit_id()))
                        {
                            Some(active) if active.node == node_id && active.knob == name => {
                                active.initial_value
                            }
                            _ => {
                                let initial = pre_edit_value.clone();
                                ui.ctx().data_mut(|data| {
                                    data.insert_temp(
                                        active_knob_edit_id(),
                                        ActiveKnobEdit {
                                            node: node_id,
                                            knob: name.to_string(),
                                            initial_value: initial.clone(),
                                        },
                                    );
                                });
                                initial
                            }
                        };
                        messages.push(KernelMessage::Execute(UndoableCommand::SetKnob {
                            node: node_id,
                            knob: name.to_string(),
                            value: next,
                            previous: initial_value,
                        }));
                    }

                    // A row that no longer holds keyboard focus or a pointer
                    // drag has finished its edit. Clearing after the emit above
                    // keeps the `Enter` frame reporting the true origin.
                    if !row_active {
                        ui.ctx().data_mut(|data| {
                            if let Some(active) =
                                data.get_temp::<ActiveKnobEdit>(active_knob_edit_id())
                            {
                                if active.node == node_id && active.knob == name {
                                    data.remove::<ActiveKnobEdit>(active_knob_edit_id());
                                }
                            }
                        });
                    }
                }
            });
        });

        messages
    }
}

fn to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, Vec2};
    use tanova_kernel::{Dag, KernelEvent, KnobTable, UndoStack};

    const NODE: NodeId = NodeId(2);

    fn screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 1200.0))
    }

    /// Runs one `PropertiesPanel::show` pass and returns what it emitted.
    fn run_panel(
        ctx: &egui::Context,
        mirror: &GraphMirror,
        events: Vec<Event>,
    ) -> Vec<KernelMessage> {
        let mut messages = Vec::new();
        ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(screen()),
                events,
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        messages = PropertiesPanel::show(ui, mirror, Some(NODE), None);
                    });
                });
            },
        )
        .drop_without_applying_deltas();
        messages
    }

    fn knob_rect(name: &str) -> egui::Rect {
        KNOB_WIDGET_RECTS.with(|rects| {
            rects
                .borrow()
                .get(&(NODE, name.to_string()))
                .copied()
                .unwrap_or_else(|| panic!("no widget rect captured for {name}"))
        })
    }

    /// Mirrors the kernel: apply each emitted `SetKnob` to the DAG, record the
    /// enriched command on the undo stack, and follow the change in the mirror.
    fn feed(
        messages: Vec<KernelMessage>,
        mirror: &mut GraphMirror,
        dag: &mut Dag,
        undo: &mut UndoStack,
    ) {
        for message in messages {
            if let KernelMessage::Execute(command) = message {
                let enriched = dag
                    .apply_command(command)
                    .expect("the dag should accept a knob edit");
                undo.push(enriched.clone());
                mirror.apply_command(&enriched);
            }
        }
    }

    fn undo_once(undo: &mut UndoStack, dag: &mut Dag, mirror: &mut GraphMirror) {
        let command = undo.undo().expect("an undo step must exist");
        dag.undo_command(&command)
            .expect("the dag applies the inverse");
        mirror.apply_event(&KernelEvent::Undone(command));
    }

    fn redo_once(undo: &mut UndoStack, dag: &mut Dag, mirror: &mut GraphMirror) {
        let command = undo.redo().expect("a redo step must exist");
        dag.apply_command(command.clone())
            .expect("the dag re-applies the command");
        mirror.apply_event(&KernelEvent::Redone(command));
    }

    fn mix(mirror: &GraphMirror) -> f64 {
        match mirror
            .node(NODE)
            .and_then(|node| node.knobs.get("mix"))
            .cloned()
        {
            Some(KnobValue::Float(value)) => value,
            other => panic!("mix should be a float knob, got {other:?}"),
        }
    }

    /// `mix` is stored as `f64` but edited through an `f32` widget, so compare
    /// with a tolerance rather than bit-for-bit.
    fn assert_mix(mirror: &GraphMirror, expected: f64) {
        let actual = mix(mirror);
        assert!(
            (actual - expected).abs() < 1e-6,
            "mix should be {expected}, got {actual}"
        );
    }

    fn click(center: Pos2) -> Vec<Vec<Event>> {
        vec![
            vec![
                Event::PointerMoved(center),
                Event::PointerButton {
                    pos: center,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
            vec![Event::PointerButton {
                pos: center,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
            // The first edit-mode frame selects all the text.
            vec![],
        ]
    }

    /// A numeric field commits on every keystroke, so `0.5` briefly becomes
    /// `0.0`. Pausing between keystrokes must not split the edit: one undo has
    /// to land on the previous committed number, never on the intermediate `0`.
    #[test]
    fn multi_keystroke_knob_typing_across_pauses_undoes_per_committed_number() {
        let ctx = egui::Context::default();
        // Build the node explicitly instead of linking the cartridge registry:
        // a linked registry changes the default knobs other tests observe.
        let mut knobs = KnobTable::new();
        knobs.insert("mix", KnobValue::Float(1.0));

        let mut mirror = GraphMirror::default();
        mirror.apply_event(&KernelEvent::Applied(UndoableCommand::RestoreNode {
            id: NODE,
            kind: "Grade".to_string(),
            x: 0.0,
            y: 0.0,
            disabled: false,
            knobs: Box::new(knobs.clone()),
            edges: Vec::new(),
        }));
        let mut dag = Dag::new();
        dag.restore_node(NODE, "Grade", 0.0, 0.0, false, &knobs, &[])
            .expect("dag node #2");
        let mut undo = UndoStack::new();

        assert_mix(&mirror, 1.0);

        // Lay the panel out once so the mix widget's rect is known.
        let _ = run_panel(&ctx, &mirror, vec![]);
        let center = knob_rect("mix").center();

        // --- Edit 1: 1.0 -> 0.5 -------------------------------------------
        for events in click(center) {
            let _ = run_panel(&ctx, &mirror, events);
        }

        let messages = run_panel(&ctx, &mirror, vec![Event::Text("0".to_string())]);
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_mix(&mirror, 0.0);
        // Longer than COALESCE_WINDOW_MS while the field keeps focus.
        undo.break_coalescing();

        let messages = run_panel(&ctx, &mirror, vec![Event::Text(".5".to_string())]);
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_mix(&mirror, 0.5);
        undo.break_coalescing();

        // Enter commits and releases focus, ending the session.
        let _ = run_panel(&ctx, &mirror, vec![enter()]);
        assert!(
            undo.can_undo(),
            "the first edit should be on the undo stack"
        );

        // --- Edit 2: 0.5 -> 0.2 -------------------------------------------
        for events in click(center) {
            let _ = run_panel(&ctx, &mirror, events);
        }

        let messages = run_panel(&ctx, &mirror, vec![Event::Text("0".to_string())]);
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_mix(&mirror, 0.0);
        undo.break_coalescing();

        let messages = run_panel(&ctx, &mirror, vec![Event::Text(".2".to_string())]);
        feed(messages, &mut mirror, &mut dag, &mut undo);
        assert_mix(&mirror, 0.2);
        undo.break_coalescing();

        let _ = run_panel(&ctx, &mirror, vec![enter()]);

        // Two sessions, two undo steps.
        undo_once(&mut undo, &mut dag, &mut mirror);
        assert_mix(&mirror, 0.5);
        undo_once(&mut undo, &mut dag, &mut mirror);
        assert_mix(&mirror, 1.0);
        assert!(!undo.can_undo(), "the keystrokes must not add extra steps");

        redo_once(&mut undo, &mut dag, &mut mirror);
        assert_mix(&mirror, 0.5);
        redo_once(&mut undo, &mut dag, &mut mirror);
        assert_mix(&mirror, 0.2);
    }

    fn enter() -> Event {
        Event::Key {
            key: Key::Enter,
            physical_key: Some(Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }
}
