//! Clip Log panel: the rows noted while watching raw footage (file, start, stop, content, include,
//! order). The table only shows and edits what the `log.*` commands own: every committed cell edit
//! is one command (undoable, saved in the project), and agents drive the same rows with them.
//!
//! A text cell commits when it loses focus (Enter, Tab or a click elsewhere). Start and Stop take
//! `hh:mm:ss:ff` or a frame count; Include cycles blank (undecided) → Yes → No; Order is a whole
//! number from 1, or blank for none (0, negative and non-numbers are refused); a double click on an
//! empty Order cell takes the next free number.
//! Number gives the
//! ticked rows that have none the next unused numbers. Build Sequence makes a new sequence from the
//! rows set to Yes, in Order (Order view only), optionally with a sound crossfade and/or a picture
//! dip to black of 0.5, 1, 2, 3 or 4 seconds at each cut (Fade sound, Fade image). Export CSV… writes every row to a file and Import CSV… adds rows from one (rows already in the log are skipped). Include, Exclude,
//! Number and Delete appear only while rows are ticked.
//!
//! A number another row already uses asks first: the rows numbered that or higher move up by one
//! (`log.insertOrder`). Order view lists the rows set to Yes in edit order; dragging a row by its
//! handle renumbers them 1, 2, 3… (`log.setOrder`).
//!
//! The columns share the panel width (File and Content twice as wide as the others); a cell's
//! full text shows when the pointer rests on it.
//!
//! Automation ids: `clip_log.add`, `clip_log.delete`, `clip_log.include`, `clip_log.exclude`,
//! `clip_log.number`, `clip_log.import`, `clip_log.select_all`, `clip_log.build`, `clip_log.ordered`, `clip_log.confirm.ok`, `clip_log.confirm.cancel`, `clip_log.order.<entry>.handle`, `clip_log.export`, `clip_log.filter`, `clip_log.empty`, and per row `clip_log.row.<entry>.<select|file|start|stop|content|include|order>`.

use egui::{Rect, Sense, vec2};
use filmcraft_engine::clip_log::{absolute_path, included_rows, parse_time, rows, timecode};
use filmcraft_project::ItemId;
use filmcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use super::panel_state::ClipLogState;
use crate::FilmcraftApp;

/// One table row, copied out of the project so drawing can change the app.
struct RowView {
    entry: u64,
    item: ItemId,
    file: String,
    /// The file's full path (its folder), shown on hover; the file name when it has none.
    path: String,
    rate: FrameRate,
    start: Tick,
    end: Tick,
    content: String,
    include: Option<bool>,
    order: Option<u32>,
}

/// A command to run once the table is drawn.
type Action = (&'static str, Value);

/// Cell widths for the current panel width: File and Content take two units, the rest one.
#[derive(Clone, Copy)]
struct Widths {
    wide: f32,
    narrow: f32,
}

impl Widths {
    fn for_panel(available: f32) -> Widths {
        // 8 units across 6 columns, plus the tick box, the column gaps and a scroll bar
        let unit = ((available - 24.0 - 6.0 * 8.0 - 16.0) / 8.0).max(56.0);
        Widths { wide: 2.0 * unit, narrow: unit }
    }
}

/// `text` as the tooltip of `response` (nothing for an empty cell).
fn hover(response: egui::Response, text: &str) -> egui::Response {
    if text.is_empty() { response } else { response.on_hover_text(text) }
}

pub fn row_id(entry: u64, column: &str) -> String {
    format!("clip_log.row.{entry}.{column}")
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let mut state = std::mem::take(&mut app.ui.panels.clip_log);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 6.0))));
    let mut actions: Vec<Action> = Vec::new();
    draw(app, &mut child, &mut state, &mut actions);
    confirm_insert(app, ui.ctx(), &mut state, &mut actions);
    for (command, params) in actions {
        match app.session.execute(command, params) {
            Ok(done) if command == "log.exportCsv" => {
                app.ui.status = tlf!("Exported {count} log entries to {path}", count = done["entries"], path = done["path"].as_str().unwrap_or(""));
            }
            Ok(done) if command == "log.importCsv" => {
                let first = done["problems"][0].as_str().unwrap_or("");
                app.ui.status = tlf!(
                    "Imported {count} log entries; {dupes} already in the log, {skipped} skipped. {first}",
                    count = done["imported"],
                    dupes = done["duplicates"],
                    skipped = done["skipped"],
                    first
                );
            }
            Ok(done) if command == "log.buildSequence" => {
                app.ui.status = tlf!("Built a sequence of {count} clips from the log", count = done["clips"]);
            }
            Ok(_) => {}
            Err(e) => app.ui.status = e.to_string(),
        }
    }
    // rows that no longer exist (deleted, undone) cannot stay ticked
    let existing: std::collections::BTreeSet<u64> = rows(&app.session.project, None).iter().map(|r| r.entry.id.0).collect();
    state.selected.retain(|id| existing.contains(id));
    app.ui.panels.clip_log = state;
}

fn collect(app: &FilmcraftApp, filter: &str) -> Vec<RowView> {
    let filter = filter.trim().to_lowercase();
    rows(&app.session.project, None)
        .into_iter()
        .filter(|r| filter.is_empty() || r.file.to_lowercase().contains(&filter) || r.entry.content.to_lowercase().contains(&filter))
        .map(|r| RowView {
            entry: r.entry.id.0,
            item: r.item,
            path: if r.path.is_empty() { r.file.clone() } else { absolute_path(&app.session.media, &r.path) },
            file: r.file,
            rate: r.rate,
            start: r.entry.start,
            end: r.entry.end,
            content: r.entry.content.clone(),
            include: r.entry.include,
            order: r.entry.order,
        })
        .collect()
}

fn draw(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState, actions: &mut Vec<Action>) {
    let shown = collect(app, &state.filter);
    let selected: Vec<u64> = shown.iter().filter(|r| state.selected.contains(&r.entry)).map(|r| r.entry).collect();
    toolbar(app, ui, state, actions, &selected);
    let widths = Widths::for_panel(ui.available_width());
    if state.ordered {
        ordered_view(app, ui, state, actions, widths);
        return;
    }
    if shown.is_empty() {
        let text = tl!("No log entries yet. Open a clip in the Source monitor, mark In and Out, then press Add from Source.");
        let r = ui.label(text);
        app.auto.add("clip_log.empty", r.rect, text);
        return;
    }
    egui::ScrollArea::both().id_salt("clip-log-scroll").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("clip-log-grid").striped(true).num_columns(7).spacing([8.0, 4.0]).show(ui, |ui| {
            select_all(app, ui, state, &shown);
            for head in ["File", "Start", "Stop", "Content", "Include", "Order"] {
                ui.label(egui::RichText::new(crate::i18n::t(head)).strong());
            }
            ui.end_row();
            for r in &shown {
                row(app, ui, state, actions, r, widths);
                ui.end_row();
            }
        });
    });
}

/// The buttons above the table. The normal view has Add from Source, the buttons for the ticked
/// rows (only while some are ticked), Export CSV… and the filter; Order view has only Build Sequence.
/// The header checkbox: ticks or unticks every row shown (the filter applies); half-ticked while
/// only some are.
fn select_all(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState, shown: &[RowView]) {
    let count = shown.iter().filter(|r| state.selected.contains(&r.entry)).count();
    let mut all = !shown.is_empty() && count == shown.len();
    let r = ui.add(egui::Checkbox::new(&mut all, "").indeterminate(count > 0 && count < shown.len()));
    let r = r.on_hover_text(tl!("Select or deselect all rows"));
    app.auto.add_response("clip_log.select_all", &r, tl!("Select or deselect all rows"));
    if r.changed() {
        for row in shown {
            if all {
                state.selected.insert(row.entry);
            } else {
                state.selected.remove(&row.entry);
            }
        }
    }
}

fn toolbar(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState, actions: &mut Vec<Action>, selected: &[u64]) {
    ui.horizontal_wrapped(|ui| {
        if state.ordered {
            order_view_toggle(app, ui, state);
            fade_controls(app, ui, state);
            build_button(app, ui, state, actions);
            return;
        }
        let has_source = app.session.state.source_item.is_some();
        let r = ui.add_enabled(has_source, egui::Button::new(tl!("Add from Source")));
        app.auto.add_response("clip_log.add", &r, tl!("Add a log entry for the Source In and Out"));
        if r.clicked() {
            actions.push(("log.addFromSource", json!({})));
        }
        order_view_toggle(app, ui, state);
        if !selected.is_empty() {
            selection_buttons(app, ui, actions, selected);
        }
        import_button(app, ui, actions);
        export_button(app, ui, actions);
        let r = ui.add(egui::TextEdit::singleline(&mut state.filter).hint_text(tl!("Filter by file or content")).desired_width(180.0));
        app.auto.add_response("clip_log.filter", &r, tl!("Filter log entries"));
    });
}

fn order_view_toggle(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState) {
    let r = ui.checkbox(&mut state.ordered, tl!("Order view"));
    let r = r.on_hover_text(tl!("List the entries set to Yes in edit order; drag a row to move it"));
    app.auto.add_response("clip_log.ordered", &r, tl!("Show the included entries in edit order"));
}

/// The fade lengths the dropdowns offer, in seconds.
const FADE_SECS: [f64; 5] = [0.5, 1.0, 2.0, 3.0, 4.0];

/// The saved length when it is one of the choices, else 1 second.
fn fade_secs(saved: f64) -> f64 {
    if FADE_SECS.contains(&saved) { saved } else { 1.0 }
}

/// Order view: whether the build adds a sound crossfade / a picture dip to black at each cut, and
/// how long it is (0.5, 1, 2, 3 or 4 seconds, shown once the box is ticked).
fn fade_controls(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState) {
    let r = ui.checkbox(&mut state.fade_sound, tl!("Fade sound"));
    let r = r.on_hover_text(tl!("Crossfade the sound between consecutive clips"));
    app.auto.add_response("clip_log.fade_sound", &r, tl!("Fade the sound between clips"));
    if state.fade_sound {
        fade_choice(app, ui, "clip_log.fade_sound_secs", &mut state.fade_sound_secs);
    }
    let r = ui.checkbox(&mut state.fade_image, tl!("Fade image"));
    let r = r.on_hover_text(tl!("Fade the picture out to black and back in between consecutive clips"));
    app.auto.add_response("clip_log.fade_image", &r, tl!("Fade the image between clips"));
    if state.fade_image {
        fade_choice(app, ui, "clip_log.fade_image_secs", &mut state.fade_image_secs);
    }
}

fn fade_choice(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &'static str, saved: &mut f64) {
    let mut pick = fade_secs(*saved);
    let r = craft_ui::ComboBox::from_id_salt(id).selected_text(format!("{pick} s")).width(70.0).show_ui(ui, |ui| {
        for v in FADE_SECS {
            let o = ui.selectable_value(&mut pick, v, format!("{v} s"));
            app.auto.add_response(&format!("{id}.option.{v}"), &o, &format!("{v} s"));
        }
    });
    app.auto.add_response(id, &r.response, tl!("Fade length"));
    *saved = pick;
}

/// Include, Exclude, Number and Delete: shown only while rows are ticked.
fn selection_buttons(app: &mut FilmcraftApp, ui: &mut egui::Ui, actions: &mut Vec<Action>, selected: &[u64]) {
    let r = ui.button(tl!("Include"));
    app.auto.add_response("clip_log.include", &r, tl!("Include the ticked entries"));
    if r.clicked() {
        actions.push(("log.setInclude", json!({"entries": selected, "include": true})));
    }
    let r = ui.button(tl!("Exclude"));
    app.auto.add_response("clip_log.exclude", &r, tl!("Exclude the ticked entries"));
    if r.clicked() {
        actions.push(("log.setInclude", json!({"entries": selected, "include": false})));
    }
    let r = ui.button(tl!("Number"));
    let r = r.on_hover_text(tl!("Give the ticked entries without a number the next unused numbers, in table order"));
    app.auto.add_response("clip_log.number", &r, tl!("Number the ticked entries that have no number with the next unused numbers"));
    if r.clicked() {
        actions.push(("log.numberNext", json!({"entries": selected})));
    }
    let r = ui.button(tl!("Delete"));
    app.auto.add_response("clip_log.delete", &r, tl!("Delete the ticked entries"));
    if r.clicked() {
        actions.push(("log.delete", json!({"entries": selected})));
    }
}

/// Build Sequence: shown in Order view only.
fn build_button(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &ClipLogState, actions: &mut Vec<Action>) {
    let included = rows(&app.session.project, None).iter().any(|r| r.entry.include == Some(true));
    let r = ui.add_enabled(included, egui::Button::new(tl!("Build Sequence")));
    let r = r.on_hover_text(tl!("Make a new sequence from the entries set to Yes, in Order"));
    app.auto.add_response("clip_log.build", &r, tl!("Build a sequence from the included entries"));
    if r.clicked() {
        let mut params = json!({});
        if state.fade_sound {
            params["fadeAudio"] = json!(fade_secs(state.fade_sound_secs));
        }
        if state.fade_image {
            params["fadeVideo"] = json!(fade_secs(state.fade_image_secs));
        }
        actions.push(("log.buildSequence", params));
    }
}

/// Import CSV…: add the rows of a CSV file (an export, or a spreadsheet with the same headers).
fn import_button(app: &mut FilmcraftApp, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    let r = ui.button(tl!("Import CSV…"));
    let r = r.on_hover_text(tl!("Add the entries of a CSV file. Entries already in the log are skipped"));
    app.auto.add_response("clip_log.import", &r, tl!("Import log entries from a CSV file"));
    if r.clicked() {
        match app.hooks.pick_open_file.as_mut().map(|pick| pick(tl!("CSV"), &["csv"])) {
            Some(Some(path)) => actions.push(("log.importCsv", json!({"path": path}))),
            Some(None) => {}
            None => app.ui.status = tl!("No file dialog is available here. Use the log.importCsv command with a path.").to_owned(),
        }
    }
}

fn export_button(app: &mut FilmcraftApp, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    let any = !rows(&app.session.project, None).is_empty();
    let r = ui.add_enabled(any, egui::Button::new(tl!("Export CSV…")));
    let r = r.on_hover_text(tl!("Write all log entries to a CSV file"));
    app.auto.add_response("clip_log.export", &r, tl!("Export the log as a CSV file"));
    if r.clicked() {
        match app.hooks.pick_save_as.as_mut().map(|pick| pick(tl!("CSV"), &["csv"], "clip-log.csv")) {
            Some(Some(path)) => actions.push(("log.exportCsv", json!({"path": path}))),
            Some(None) => {}
            None => app.ui.status = tl!("No file dialog is available here. Use the log.exportCsv command with a path.").to_owned(),
        }
    }
}

fn row(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState, actions: &mut Vec<Action>, r: &RowView, w: Widths) {
    let mut ticked = state.selected.contains(&r.entry);
    let tick = ui.checkbox(&mut ticked, "");
    app.auto.add_response(&row_id(r.entry, "select"), &tick, tl!("Select log entry"));
    if tick.changed() {
        if ticked {
            state.selected.insert(r.entry);
        } else {
            state.selected.remove(&r.entry);
        }
    }

    let file = ui
        .scope(|ui| {
            ui.set_width(w.wide);
            // a truncated label shows its own full-text tooltip; ours (the path) replaces it
            ui.add(egui::Label::new(&r.file).truncate().show_tooltip_when_elided(false).sense(Sense::click()))
        })
        .inner;
    let file = hover(file, &r.path);
    app.auto.add_response(&row_id(r.entry, "file"), &file, &r.file);
    if file.clicked() {
        actions.push(("source.open", json!({"item": r.item.0})));
    }

    for (column, time) in [("start", r.start), ("stop", r.end)] {
        let text = timecode(r.rate, time);
        let (resp, committed) = cell(ui, r.entry, column, &text, w.narrow);
        let resp = hover(resp, &text);
        app.auto.add_response(&row_id(r.entry, column), &resp, &text);
        if let Some(typed) = committed {
            match parse_time(r.rate, &typed) {
                Some(t) => {
                    let mut params = json!({"entry": r.entry});
                    params[if column == "start" { "start" } else { "end" }] = json!(t.0);
                    actions.push(("log.edit", params));
                }
                None => app.ui.status = tlf!("`{text}` is not a time. Use hh:mm:ss:ff or a frame count.", text = typed.trim()),
            }
        }
    }

    let (resp, committed) = cell(ui, r.entry, "content", &r.content, w.wide);
    let resp = hover(resp, &r.content);
    app.auto.add_response(&row_id(r.entry, "content"), &resp, tl!("Content"));
    if let Some(typed) = committed {
        actions.push(("log.edit", json!({"entry": r.entry, "content": typed})));
    }

    let (label, next) = match r.include {
        None => ("", json!(true)),
        Some(true) => (tl!("Yes"), json!(false)),
        Some(false) => (tl!("No"), Value::Null),
    };
    let include = ui.add(egui::Button::new(label).min_size(vec2(w.narrow, 0.0)));
    app.auto.add_response(&row_id(r.entry, "include"), &include, label);
    if include.clicked() {
        actions.push(("log.setInclude", json!({"entries": [r.entry], "include": next})));
    }

    let order = r.order.map(|n| n.to_string()).unwrap_or_default();
    let (resp, committed) = cell(ui, r.entry, "order", &order, w.narrow);
    app.auto.add_response(&row_id(r.entry, "order"), &resp, &order);
    // a double click on an empty, untouched Order cell takes the next free number (the row need
    // not be ticked); a cell that has a number, or text typed into it, is left alone
    let typed = ui.data(|d| d.get_temp::<String>(draft_id(r.entry, "order"))).unwrap_or_default();
    let resp = if order.is_empty() { hover(resp, tl!("Double-click for the next free number")) } else { resp };
    if resp.double_clicked() && order.is_empty() && typed.trim().is_empty() {
        // leave the cell, or its empty draft would be committed over the new number
        resp.surrender_focus();
        ui.data_mut(|d| d.remove::<String>(draft_id(r.entry, "order")));
        actions.push(("log.numberNext", json!({"entries": [r.entry]})));
    }
    if let Some(typed) = committed {
        let typed = typed.trim();
        match (typed.is_empty(), typed.parse::<u32>()) {
            (true, _) => actions.push(("log.edit", json!({"entry": r.entry, "order": null}))),
            (false, Ok(n)) if n >= 1 => {
                let taken = rows(&app.session.project, None).iter().any(|x| x.entry.id.0 != r.entry && x.entry.order == Some(n));
                if taken {
                    state.pending_order = Some((r.entry, n));
                } else {
                    actions.push(("log.edit", json!({"entry": r.entry, "order": n})));
                }
            }
            _ => app.ui.status = tlf!("`{text}` is not an order. Use a whole number from 1, or leave it blank.", text = typed),
        }
    }
}

/// Where a cell keeps what is being typed into it.
fn draft_id(entry: u64, column: &'static str) -> egui::Id {
    egui::Id::unique(("clip-log-draft", entry, column))
}

/// A text cell: shows `current` until it is focused, keeps the typed draft while it is, and
/// returns the draft once it loses focus with a changed value.
fn cell(ui: &mut egui::Ui, entry: u64, column: &'static str, current: &str, width: f32) -> (egui::Response, Option<String>) {
    let draft_id = draft_id(entry, column);
    let mut draft: String = ui.data(|d| d.get_temp(draft_id)).unwrap_or_else(|| current.to_owned());
    let response = ui.add(egui::TextEdit::singleline(&mut draft).id_salt(("clip-log-edit", entry, column)).desired_width(width));
    let mut committed = None;
    if response.has_focus() {
        ui.data_mut(|d| d.insert_temp(draft_id, draft));
    } else {
        if response.lost_focus() && draft != current {
            committed = Some(draft);
        }
        ui.data_mut(|d| d.remove::<String>(draft_id));
    }
    (response, committed)
}

/// Asks before a typed Order that another row has: the rows numbered that or higher move up by one.
fn confirm_insert(app: &mut FilmcraftApp, ctx: &egui::Context, state: &mut ClipLogState, actions: &mut Vec<Action>) {
    let Some((entry, n)) = state.pending_order else { return };
    let all = rows(&app.session.project, None);
    if !all.iter().any(|x| x.entry.id.0 == entry) {
        state.pending_order = None; // deleted or undone meanwhile
        return;
    }
    let moved = all.iter().filter(|x| x.entry.id.0 != entry && x.entry.order.is_some_and(|o| o >= n)).count();
    drop(all);
    let (mut ok, mut cancel) = (false, false);
    egui::Window::new(tl!("Number already in use"))
        .id(egui::Id::unique("clip-log-confirm"))
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.label(tlf!("Number {n} is already used. The {moved} other entries numbered {n} or higher move up by one, and this entry takes {n}.", n, moved));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let b = ui.button(tl!("Renumber"));
                app.auto.add_response("clip_log.confirm.ok", &b, tl!("Renumber the entries"));
                ok = b.clicked();
                let c = ui.button(tl!("Cancel"));
                app.auto.add_response("clip_log.confirm.cancel", &c, tl!("Keep the numbers as they are"));
                cancel = c.clicked();
            });
        });
    cancel |= ctx.input(|i| i.key_pressed(egui::Key::Escape));
    if ok {
        actions.push(("log.insertOrder", json!({"entry": entry, "order": n})));
    }
    if ok || cancel {
        state.pending_order = None;
    }
}

/// One line of the ordered view.
struct OrderedRow {
    entry: u64,
    file: String,
    /// The file's full path for the tooltip; the file name when it has none.
    path: String,
    start: String,
    stop: String,
    content: String,
    order: Option<u32>,
}

/// A fixed-width, truncated text with `tip` on hover (the label's own tooltip is switched off, so
/// the tip shows once).
fn text_cell(ui: &mut egui::Ui, text: &str, tip: &str, width: f32) {
    let shown = ui.scope(|ui| {
        ui.set_width(width);
        ui.add(egui::Label::new(text).truncate().show_tooltip_when_elided(false).sense(Sense::hover()))
    });
    let _ = hover(shown.inner, tip);
}

/// The rows set to Yes in edit order. Dragging a row by its handle and dropping it elsewhere
/// renumbers them 1, 2, 3… in the new sequence.
fn ordered_view(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState, actions: &mut Vec<Action>, w: Widths) {
    let list: Vec<OrderedRow> = included_rows(&app.session.project)
        .iter()
        .map(|r| OrderedRow {
            entry: r.entry.id.0,
            file: r.file.clone(),
            path: if r.path.is_empty() { r.file.clone() } else { absolute_path(&app.session.media, &r.path) },
            start: timecode(r.rate, r.entry.start),
            stop: timecode(r.rate, r.entry.end),
            content: r.entry.content.clone(),
            order: r.entry.order,
        })
        .collect();
    if list.is_empty() {
        let text = tl!("No entries are set to Yes. Set Include to Yes on the entries that belong in the edit.");
        let r = ui.label(text);
        app.auto.add("clip_log.ordered.empty", r.rect, text);
        return;
    }
    let mut slots: Vec<(u64, egui::Rect)> = Vec::new();
    let mut dropped: Option<u64> = None;
    egui::ScrollArea::both().id_salt("clip-log-order-scroll").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("clip-log-order-grid").striped(true).num_columns(7).spacing([8.0, 4.0]).show(ui, |ui| {
            for head in ["", "#", "File", "Start", "Stop", "Content", "Order"] {
                ui.label(egui::RichText::new(crate::i18n::t(head)).strong());
            }
            ui.end_row();
            for (i, r) in list.iter().enumerate() {
                let (handle, painter_rect) = {
                    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 18.0), Sense::click_and_drag());
                    (resp, rect)
                };
                let color = ui.style().interact(&handle).fg_stroke.color;
                for k in [-4.0, 0.0, 4.0] {
                    let y = painter_rect.center().y + k;
                    ui.painter().hline(painter_rect.x_range().shrink(4.0), y, egui::Stroke::new(1.5, color));
                }
                let handle = handle.on_hover_cursor(egui::CursorIcon::Grab);
                app.auto.add_response(&format!("clip_log.order.{}.handle", r.entry), &handle, tl!("Drag to change the order"));
                if handle.drag_started() {
                    state.dragging = Some(r.entry);
                }
                if handle.drag_stopped() {
                    dropped = Some(r.entry);
                }
                slots.push((r.entry, painter_rect));
                ui.label((i + 1).to_string());
                text_cell(ui, &r.file, &r.path, w.wide);
                text_cell(ui, &r.start, &r.start, w.narrow);
                text_cell(ui, &r.stop, &r.stop, w.narrow);
                text_cell(ui, &r.content, &r.content, w.wide);
                ui.label(r.order.map(|n| n.to_string()).unwrap_or_default());
                ui.end_row();
            }
        });
    });
    let current: Vec<u64> = list.iter().map(|r| r.entry).collect();
    finish_drag(ui, state, actions, &slots, dropped, &current);
}

/// While a row is dragged: the drop line, and on release the new order as `log.setOrder`.
fn finish_drag(ui: &mut egui::Ui, state: &mut ClipLogState, actions: &mut Vec<Action>, slots: &[(u64, egui::Rect)], dropped: Option<u64>, current: &[u64]) {
    let Some(moving) = state.dragging else { return };
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    let others: Vec<(u64, egui::Rect)> = slots.iter().copied().filter(|(id, _)| *id != moving).collect();
    let Some(pointer) = ui.ctx().pointer_latest_pos() else {
        state.dragging = None;
        return;
    };
    let target = others.iter().filter(|(_, r)| r.center().y < pointer.y).count();
    let line_y = match others.get(target) {
        Some((_, r)) => r.top() - 3.0,
        None => others.last().map_or(pointer.y, |(_, r)| r.bottom() + 3.0),
    };
    ui.painter().hline(ui.min_rect().x_range(), line_y, egui::Stroke::new(2.0, ui.visuals().selection.bg_fill));
    if dropped == Some(moving) {
        let mut ids: Vec<u64> = others.iter().map(|(id, _)| *id).collect();
        ids.insert(target.min(ids.len()), moving);
        if ids != current {
            actions.push(("log.setOrder", json!({"entries": ids})));
        }
    }
    if dropped.is_some() || !ui.input(|i| i.pointer.any_down()) {
        state.dragging = None;
    }
}
