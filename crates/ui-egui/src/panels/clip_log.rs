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
//! Keyboard: Add from Source puts the cursor in the new row's Content cell; Enter commits a cell and
//! moves to the same column of the next row, Tab to the next cell, Escape drops what was typed.
//! Typed or dictated text goes into the focused cell only, so it cannot reach single-key shortcuts.
//!
//! Voice: the Voice button turns on listening (off by default; needs the `whisper` build and a
//! downloaded speech model). Watch a clip in the Source monitor and say "start" (a row begins at the
//! playhead), "stop" (it ends there and the cursor moves to its Content cell), then speak the content;
//! "content" moves the cursor to the active row's Content cell. With a cell of your own choosing
//! focused, what you say goes into that cell instead (Enter commits). Also "include", "exclude",
//! "order 3", "next row", "delete row", and times ("start at 1 minute 12"). Push to talk hears only
//! what is said while the Hold to talk button or Ctrl+Shift+J is held. A level meter shows what the
//! microphone delivers, with a warning when it is silent. Use headphones: the microphone
//! also hears the Source clip's sound.
//!
//! The columns share the panel width (File and Content twice as wide as the others); a cell's
//! full text shows when the pointer rests on it.
//!
//! Automation ids: `clip_log.add`, `clip_log.time.<start|stop>.<back|forward|playhead>` (one row ticked), `clip_log.voice`, `clip_log.voice.push`, `clip_log.voice.talk`, `clip_log.voice.level`, `clip_log.voice.status`, `clip_log.voice.warning`, `clip_log.delete`, `clip_log.include`, `clip_log.exclude`,
//! `clip_log.number`, `clip_log.import`, `clip_log.select_all`, `clip_log.build`, `clip_log.ordered`, `clip_log.confirm.ok`, `clip_log.confirm.cancel`, `clip_log.order.<entry>.handle`, `clip_log.export`, `clip_log.filter`, `clip_log.empty`, and per row `clip_log.row.<entry>.<select|file|start|stop|content|include|order>`.

use egui::{Rect, Sense, vec2};
use filmcraft_engine::clip_log::{absolute_path, fit_time, included_rows, parse_time, rows, timecode};
use filmcraft_engine::voice_listen::Heard;
use filmcraft_engine::voice_log::{Intent, interpret};
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
    /// The clip's length in ticks (0 when unknown).
    duration: i64,
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
            Ok(done) if command == "log.addFromSource" => {
                if let Some(entry) = done["entry"].as_u64() {
                    state.focus = Some((entry, "content".to_owned()));
                }
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
    if state.focus.as_ref().is_some_and(|(id, _)| !existing.contains(id)) {
        state.focus = None;
    }
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
            duration: app.session.project.item(r.item).and_then(filmcraft_project::ProjectItem::as_media).map_or(0, |m| m.info.duration.0),
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
    if let (false, [one]) = (state.ordered, selected.as_slice())
        && let Some(r) = shown.iter().find(|r| r.entry == *one)
    {
        time_editor(app, ui, actions, r);
    }
    let widths = Widths::for_panel(ui.available_width());
    if state.ordered {
        state.focus = None; // no editable cells here
        if app.session.voice_listen.active() {
            filmcraft_engine::voice_listen::stop(&mut app.session);
        }
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
            for (i, r) in shown.iter().enumerate() {
                let below = shown.get(i + 1).map(|n| n.entry);
                row(app, ui, state, actions, (r, below), widths);
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
        voice_button(app, ui, state);
        if !selected.is_empty() {
            selection_buttons(app, ui, actions, selected);
        }
        import_button(app, ui, actions);
        export_button(app, ui, actions);
        let r = ui.add(egui::TextEdit::singleline(&mut state.filter).hint_text(tl!("Filter by file or content")).desired_width(180.0));
        app.auto.add_response("clip_log.filter", &r, tl!("Filter log entries"));
    });
}

/// Shown while exactly one row is ticked: step its Start or Stop by a frame, or set it to the Source
/// playhead (enabled when that row's clip is the one open in Source). Typing a time in the cell works too.
fn time_editor(app: &mut FilmcraftApp, ui: &mut egui::Ui, actions: &mut Vec<Action>, r: &RowView) {
    let on_source = app.session.state.source_item == Some(r.item);
    let head = app.session.state.source_playhead;
    let frame = r.rate.tick_of(1).0;
    ui.horizontal_wrapped(|ui| {
        for (column, key, now) in [("start", "start", r.start), ("stop", "end", r.end)] {
            ui.label(egui::RichText::new(if column == "start" { tl!("Start") } else { tl!("Stop") }).strong());
            let mut set = |ui: &mut egui::Ui, id: &str, label: &str, tip: &str, to: Option<i64>| {
                let resp = ui.add_enabled(to.is_some(), egui::Button::new(label)).on_hover_text(tip);
                app.auto.add_response(&format!("clip_log.time.{column}.{id}"), &resp, tip);
                if let (true, Some(t)) = (resp.clicked(), to) {
                    let mut params = json!({"entry": r.entry});
                    params[key] = json!(t);
                    actions.push(("log.edit", params));
                }
            };
            set(ui, "back", "-1f", tl!("One frame earlier"), (frame > 0).then_some(now.0.saturating_sub(frame)));
            set(ui, "forward", "+1f", tl!("One frame later"), (frame > 0).then_some(now.0.saturating_add(frame)));
            let at = r.rate.tick_of(r.rate.frame_at(head)).0;
            set(ui, "playhead", tl!("Playhead"), tl!("Set to the Source playhead"), on_source.then_some(at));
            ui.add_space(12.0);
        }
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

/// One row; `below` is the entry of the row under it (where Enter moves the cursor).
fn row(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState, actions: &mut Vec<Action>, (r, below): (&RowView, Option<u64>), w: Widths) {
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
        let want = state.take_focus(r.entry, column);
        let (resp, committed) = cell(ui, r.entry, column, &text, w.narrow, want);
        enter_moves_down(ui, state, &resp, column, below);
        let resp = hover(resp, &text);
        app.auto.add_response(&row_id(r.entry, column), &resp, &text);
        if let Some(typed) = committed {
            commit_time(app, actions, r, column, &typed);
        }
    }

    let want = state.take_focus(r.entry, "content");
    let (resp, committed) = cell(ui, r.entry, "content", &r.content, w.wide, want);
    enter_moves_down(ui, state, &resp, "content", below);
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
    let want = state.take_focus(r.entry, "order");
    let (resp, committed) = cell(ui, r.entry, "order", &order, w.narrow, want);
    enter_moves_down(ui, state, &resp, "order", below);
    app.auto.add_response(&row_id(r.entry, "order"), &resp, &order);
    // a double click on an empty, untouched Order cell takes the next free number (the row need
    // not be ticked); a cell that has a number, or text typed into it, is left alone
    let typed = ui.data(|d| d.get_temp::<String>(draft_id(r.entry, "order"))).unwrap_or_default();
    let resp = if order.is_empty() { hover(resp, tl!("Double-click for the next free number")) } else { resp };
    let double = resp.double_clicked() || pressed_twice(ui, &resp, r.entry);
    if double && order.is_empty() && typed.trim().is_empty() {
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

/// A typed Start or Stop: read as a time, fitted to the clip (a time outside it, or crossing the
/// row's other end, is set to the nearest limit) and stored with `log.edit`.
fn commit_time(app: &mut FilmcraftApp, actions: &mut Vec<Action>, r: &RowView, column: &str, typed: &str) {
    let Some(t) = parse_time(r.rate, typed) else {
        app.ui.status = tlf!("`{text}` is not a time. Use hh:mm:ss:ff, seconds like 12.5, or a frame count.", text = typed.trim());
        return;
    };
    let is_start = column == "start";
    let (t, moved) = fit_time(r.rate, r.duration, is_start, if is_start { r.end } else { r.start }, t);
    if moved {
        app.ui.status = tlf!("`{text}` is outside the clip; set to {limit}.", text = typed.trim(), limit = timecode(r.rate, t));
    }
    let mut params = json!({"entry": r.entry});
    params[if is_start { "start" } else { "end" }] = json!(t.0);
    actions.push(("log.edit", params));
}

/// Two presses on the cell within 0.8 s. Counted here, from the pointer, because a text field does
/// not always report `double_clicked()` (its own click handling takes the presses).
fn pressed_twice(ui: &egui::Ui, resp: &egui::Response, entry: u64) -> bool {
    let pressed = ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary));
    if !pressed || !ui.rect_contains_pointer(resp.rect) {
        return false;
    }
    let id = egui::Id::unique(("clip-log-press", entry));
    let now = ui.input(|i| i.time);
    let last: Option<f64> = ui.data(|d| d.get_temp(id));
    let twice = last.is_some_and(|t| now - t < 0.8);
    ui.data_mut(|d| {
        if twice {
            d.remove::<f64>(id);
        } else {
            d.insert_temp(id, now);
        }
    });
    twice
}

/// Where a cell keeps what is being typed into it.
fn draft_id(entry: u64, column: &str) -> egui::Id {
    egui::Id::unique(("clip-log-draft", entry, column))
}

/// Enter in a cell moves the cursor to the same column of the next row (the last row stays put).
fn enter_moves_down(ui: &egui::Ui, state: &mut ClipLogState, resp: &egui::Response, column: &str, below: Option<u64>) {
    if resp.lost_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter))
        && let Some(next) = below
    {
        state.focus = Some((next, column.to_owned()));
    }
}

/// A text cell: shows `current` until it is focused, keeps the typed draft while it is, and
/// returns the draft once it loses focus with a changed value. Escape drops the draft instead;
/// `want_focus` puts the cursor in the cell (and scrolls it into view). Text a command adds to the
/// row meanwhile (voice) shows in the cell as long as nothing was typed into it.
fn cell(ui: &mut egui::Ui, entry: u64, column: &'static str, current: &str, width: f32, want_focus: bool) -> (egui::Response, Option<String>) {
    let draft_id = draft_id(entry, column);
    let base_id = egui::Id::unique(("clip-log-base", entry, column));
    let base: Option<String> = ui.data(|d| d.get_temp(base_id));
    let mut draft: String = ui.data(|d| d.get_temp(draft_id)).unwrap_or_else(|| current.to_owned());
    if base.as_ref().is_some_and(|b| b != current && draft == *b) {
        draft = current.to_owned();
    }
    let response = ui.add(egui::TextEdit::singleline(&mut draft).id_salt(("clip-log-edit", entry, column)).desired_width(width));
    // Order takes digits only; Start and Stop take a time (digits, ':' and '.')
    match column {
        "order" => draft.retain(|c| c.is_ascii_digit()),
        "start" | "stop" => draft.retain(|c| c.is_ascii_digit() || c == ':' || c == '.'),
        _ => {}
    }
    if want_focus {
        response.request_focus();
        response.scroll_to_me(None);
    }
    // Escape leaves the cell and drops what was typed
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) && (response.has_focus() || response.lost_focus()) {
        response.surrender_focus();
        ui.data_mut(|d| {
            d.remove::<String>(draft_id);
            d.remove::<String>(base_id);
            d.remove::<(u64, String)>(last_cell_id());
        });
        return (response, None);
    }
    let mut committed = None;
    if response.has_focus() || want_focus {
        let now = ui.input(|i| i.time);
        ui.data_mut(|d| {
            d.insert_temp(draft_id, draft);
            d.insert_temp(base_id, current.to_owned());
            d.insert_temp(editing_id(), (entry, column.to_owned(), now));
            d.insert_temp(last_cell_id(), (entry, column.to_owned()));
        });
    } else {
        // leaving by Enter or Tab ends the editing; a click elsewhere (the talk button) keeps the cell as the voice target
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Tab)) {
            ui.data_mut(|d| d.remove::<(u64, String)>(last_cell_id()));
        }
        if response.lost_focus() && draft != current {
            committed = Some(draft);
        }
        ui.data_mut(|d| {
            d.remove::<String>(draft_id);
            d.remove::<String>(base_id);
            if d.get_temp::<(u64, String, f64)>(editing_id()).is_some_and(|(e, c, _)| e == entry && c == column) {
                d.remove::<(u64, String, f64)>(editing_id());
            }
        });
    }
    (response, committed)
}

/// Where the cell being edited by hand is noted (entry, column, time), so a heard phrase can go into it.
fn editing_id() -> egui::Id {
    egui::Id::unique("clip-log-editing")
}

/// Add `text` to what is typed in a cell (it is focused: its draft exists).
fn inject(ctx: &egui::Context, entry: u64, column: &str, text: &str) {
    let Some(column) = ["start", "stop", "content", "order"].into_iter().find(|c| *c == column) else { return };
    let id = draft_id(entry, column);
    ctx.data_mut(|d| {
        let typed: String = d.get_temp(id).unwrap_or_default();
        let joined = if typed.trim().is_empty() { text.to_owned() } else { format!("{} {}", typed.trim_end(), text) };
        d.insert_temp(id, joined);
    });
}

/// The cell last edited by hand (entry, column). It stays after a click on the talk button took the
/// cursor away, so push-to-talk can still put what is said into it.
fn last_cell_id() -> egui::Id {
    egui::Id::unique("clip-log-last-cell")
}

fn last_cell(ctx: &egui::Context) -> Option<(u64, String)> {
    ctx.data(|d| d.get_temp::<(u64, String)>(last_cell_id()))
}

/// The cell the user is typing in, if one was focused in the last moments.
fn hand_cell(ctx: &egui::Context) -> Option<(u64, String)> {
    let (entry, column, at) = ctx.data(|d| d.get_temp::<(u64, String, f64)>(editing_id()))?;
    (ctx.input(|i| i.time) - at < 0.5).then_some((entry, column))
}

/// Hold this with Ctrl+Shift to talk in push-to-talk mode.
const TALK_KEY: egui::Key = egui::Key::J;

/// Voice: the button that turns listening on (off by default), the push-to-talk choice and what it hears.
fn voice_button(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState) {
    let on = app.session.voice_listen.active();
    let can = app.session.transcriber.is_some() || filmcraft_engine::transcript::speech_available();
    let r = ui.add_enabled(can || on, egui::Button::new(if on { tl!("Voice: on") } else { tl!("Voice") }).selected(on));
    let tip = if can {
        tl!("Listen for \"start\", \"stop\", \"content\", \"include\", \"exclude\", \"order 3\", \"next row\" and \"delete row\" while you watch the Source clip. Use headphones.")
    } else {
        tl!("Speech recognition is not available in this build (it needs the `whisper` feature).")
    };
    let r = r.on_hover_text(tip);
    app.auto.add_response("clip_log.voice", &r, tl!("Turn voice control of the log on or off"));
    if r.clicked() {
        match app.session.execute("voice.listen", json!({"on": !on, "mode": push_mode(state)})) {
            Ok(_) => app.ui.status = if on { String::new() } else { tl!("Listening. Open a clip in Source, then say \"start\".").to_owned() },
            Err(e) => app.ui.status = e.to_string(),
        }
    }
    let push = ui.checkbox(&mut state.push_to_talk, tl!("Push to talk"));
    let push = push.on_hover_text(tl!("Only what you say while holding the button (or Ctrl+Shift+J) is heard"));
    app.auto.add_response("clip_log.voice.push", &push, tl!("Hear only what is said while the talk button is held"));
    if push.changed() && on {
        let _ = app.session.execute("voice.listen", json!({"on": true, "mode": push_mode(state)}));
    }
    if on {
        voice_status(app, ui, state);
    }
}

fn push_mode(state: &ClipLogState) -> &'static str {
    if state.push_to_talk { "push" } else { "auto" }
}

/// Hold the button to talk; a quick tap keeps it on until the next tap (for a mouse that cannot hold).
fn talk_button(ui: &egui::Ui, talk: &egui::Response, state: &mut ClipLogState) {
    let now = ui.input(|i| i.time);
    let down = talk.is_pointer_button_down_on();
    if down && state.talk_pressed_at.is_none() {
        state.talk_pressed_at = Some(now);
    }
    if talk.clicked() {
        let tap = state.talk_pressed_at.is_some_and(|t| now - t < 0.35);
        state.talk_latched = tap && !state.talk_latched;
    }
    if !down {
        state.talk_pressed_at = None;
    }
    state.talk_button = down || state.talk_latched;
}

/// While listening: the talk button, the level, the phrase last heard, and a warning when the
/// microphone delivers nothing.
fn voice_status(app: &mut FilmcraftApp, ui: &mut egui::Ui, state: &mut ClipLogState) {
    if app.session.voice_listen.push() {
        let label = if app.session.voice_listen.talking() { tl!("Talking… (tap to stop)") } else { tl!("Hold to talk") };
        let talk = ui.add(egui::Button::new(label).selected(app.session.voice_listen.talking()));
        app.auto.add_response("clip_log.voice.talk", &talk, tl!("Hold while speaking"));
        talk_button(ui, &talk, state);
    } else {
        state.talk_latched = false;
        state.talk_button = false;
    }
    let level = ((app.session.voice_listen.level_db() + 60.0) / 60.0).clamp(0.0, 1.0);
    let meter = ui.add(egui::ProgressBar::new(level).desired_width(70.0));
    app.auto.add_response("clip_log.voice.level", &meter, tl!("Microphone level"));
    let l = &app.session.voice_listen;
    let status = match l.state() {
        "loading" => tl!("loading the speech model…"),
        "recognising" => tl!("recognising…"),
        _ if l.push() => tl!("hold to talk"),
        _ => tl!("listening"),
    };
    let v = &app.session.voice_log;
    let text = if v.heard.is_empty() { status.to_owned() } else { format!("{status} · “{}” {}", v.heard, v.note) };
    let r = ui.label(text.clone());
    app.auto.add_response("clip_log.voice.status", &r, &text);
    if l.silent_secs() > 4.0 {
        let warning = tlf!(
            "No sound from the microphone ({device}). Check System Settings ▸ Privacy & Security ▸ Microphone and the input device.",
            device = l.device()
        );
        let r = ui.colored_label(ui.visuals().warn_fg_color, warning.clone());
        app.auto.add_response("clip_log.voice.warning", &r, &warning);
    }
}

/// Every frame while listening (even when the panel is hidden, so the microphone never backs up):
/// pass on the talk key, read the input and act on each phrase heard.
pub fn pump_voice(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if !app.session.voice_listen.active() {
        return;
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(50));
    let mut state = std::mem::take(&mut app.ui.panels.clip_log);
    let key = ctx.input(|i| i.modifiers.ctrl && i.modifiers.shift && i.key_down(TALK_KEY));
    filmcraft_engine::voice_listen::set_talking(&mut app.session, state.talk_button || key);
    let heard = filmcraft_engine::voice_listen::poll(&mut app.session);
    if let Some(error) = app.session.voice_listen.error.clone().filter(|_| !app.session.voice_listen.active()) {
        app.ui.status = error;
    }
    for phrase in &heard {
        apply_heard(app, ctx, &mut state, phrase);
    }
    app.ui.panels.clip_log = state;
}

/// The row voice control changes: the ticked row (the first one in table order when several are),
/// else the row whose cell has the cursor, else the row being logged.
fn voice_target(app: &FilmcraftApp, ctx: &egui::Context, state: &ClipLogState) -> Option<u64> {
    let ticked = rows(&app.session.project, None).iter().map(|r| r.entry.id.0).find(|id| state.selected.contains(id));
    ticked.or_else(|| hand_cell(ctx).or_else(|| last_cell(ctx)).map(|(entry, _)| entry))
}

/// What a heard phrase becomes in a Start, Stop or Order cell: a time in the clip's timecode, or a
/// whole number. `Err` carries the message when it is neither.
fn spoken_cell(app: &FilmcraftApp, entry: u64, column: &str, text: &str) -> Result<String, String> {
    if column == "order" {
        return filmcraft_engine::voice_log::spoken_number(text).filter(|n| *n >= 1).map(|n| n.to_string()).ok_or_else(|| tlf!("`{text}` is not an order number.", text));
    }
    let rate = rows(&app.session.project, None).iter().find(|r| r.entry.id.0 == entry).map(|r| r.rate);
    let secs = filmcraft_engine::voice_log::spoken_time(text);
    match (rate, secs) {
        (Some(rate), Some(secs)) => Ok(timecode(rate, Tick::from_seconds_f64(secs))),
        _ => Err(tlf!("`{text}` is not a time.", text)),
    }
}

/// A time or number spoken for a cell that has no cursor now (the talk button took it): stored in the
/// row at once, a time fitted to the clip like a typed one.
fn say_into_row(app: &mut FilmcraftApp, entry: u64, column: &str, text: &str) {
    let Some((item, rate, begin, finish)) = rows(&app.session.project, None).iter().find(|r| r.entry.id.0 == entry).map(|r| (r.item, r.rate, r.entry.start, r.entry.end)) else {
        return;
    };
    let message = match column {
        "order" => match filmcraft_engine::voice_log::spoken_number(text).filter(|n| *n >= 1) {
            Some(n) => app.session.execute("voice.logHear", json!({"text": format!("order {n}")})).err().map(|e| e.to_string()),
            None => Some(tlf!("`{text}` is not an order number.", text)),
        },
        "start" | "stop" => match filmcraft_engine::voice_log::spoken_time(text) {
            Some(secs) => {
                let is_start = column == "start";
                let duration = app.session.project.item(item).and_then(filmcraft_project::ProjectItem::as_media).map_or(0, |m| m.info.duration.0);
                let (t, _) = fit_time(rate, duration, is_start, if is_start { finish } else { begin }, Tick::from_seconds_f64(secs));
                let mut params = json!({"entry": entry});
                params[if is_start { "start" } else { "end" }] = json!(t.0);
                app.session.execute("log.edit", params).err().map(|e| e.to_string())
            }
            None => Some(tlf!("`{text}` is not a time.", text)),
        },
        _ => None,
    };
    if let Some(m) = message {
        app.ui.status = m;
    }
}

/// Free text for the cell with the cursor, or the one last edited (`Some` when it took the phrase).
/// Content gets the words added; Start, Stop and Order get a time or a number, and anything else is
/// refused. `focused`: the cursor is in the cell (its draft takes the text); otherwise the row is
/// changed directly.
fn say_into_cell(app: &mut FilmcraftApp, ctx: &egui::Context, (entry, column): (u64, String), focused: bool, text: &str) -> bool {
    if column == "content" && app.session.voice_log.writing() == Some(entry) {
        return false; // the engine adds it to the row's content
    }
    if column == "content" && !focused {
        return false;
    }
    if column == "content" {
        inject(ctx, entry, &column, text);
    } else if focused {
        match spoken_cell(app, entry, &column, text) {
            Ok(value) => {
                ctx.data_mut(|d| d.insert_temp(draft_id(entry, &column), value));
            }
            Err(message) => app.ui.status = message,
        }
    } else {
        say_into_row(app, entry, &column, text);
    }
    app.session.voice_log.heard = text.to_owned();
    app.session.voice_log.note = tl!("typed into the cell").to_owned();
    true
}

/// One phrase. It changes the ticked row, or when none is ticked the row with the cursor in it. Free
/// text goes into the cell with the cursor (only digits or a time into Order, Start and Stop);
/// everything else is the voice flow (`voice.logHear`). "start" adds a row and ticks it; "stop" and
/// "content" move the cursor to the row's Content cell.
fn apply_heard(app: &mut FilmcraftApp, ctx: &egui::Context, state: &mut ClipLogState, phrase: &Heard) {
    let intent = interpret(&phrase.text);
    let starting = matches!(intent, Intent::Start(_));
    let target = if starting { None } else { voice_target(app, ctx, state) };
    if let Some(entry) = target {
        filmcraft_engine::voice_log::aim(&mut app.session, entry);
    }
    if let Intent::Text(text) = &intent {
        let fits = |c: &(u64, String)| target.is_none_or(|t| t == c.0);
        let hand = hand_cell(ctx).filter(fits);
        let cell = hand.clone().or_else(|| last_cell(ctx).filter(fits));
        if let Some(cell) = cell
            && say_into_cell(app, ctx, cell, hand.is_some(), text)
        {
            return;
        }
    }
    if starting {
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
    }
    match app.session.execute("voice.logHear", json!({"text": phrase.text, "at": phrase.at.0})) {
        Ok(done) => {
            let action = done["action"].as_str().unwrap_or("");
            let Some(entry) = done["entry"].as_u64() else { return };
            if matches!(action, "start" | "row") {
                state.selected = std::iter::once(entry).collect();
            }
            if matches!(action, "stop" | "content" | "row") {
                state.focus = Some((entry, "content".to_owned()));
            }
        }
        Err(e) => app.ui.status = e.to_string(),
    }
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
