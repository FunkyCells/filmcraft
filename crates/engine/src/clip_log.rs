//! Engine side of the Clip Log: rows noted while watching raw footage.
//!
//! | Id | What |
//! |---|---|
//! | `log.list` | the rows, for every media item or one (`item`) |
//! | `log.add` | add a row for a time range of a media item |
//! | `log.addFromSource` | add a row for the marked range of the Source monitor |
//! | `log.edit` | change one or several rows (range, content, include, order, notes), atomically |
//! | `log.delete` | remove rows |
//! | `log.setInclude` | set the include flag of several rows |
//! | `log.setOrder` | number the listed rows 1, 2, 3… in the given order |
//! | `log.insertOrder` | give a row a number, moving the rows numbered that or higher up by one |
//! | `log.numberNext` | give the listed rows that have no number yet the next unused numbers |
//! | `log.buildSequence` | build a new sequence from the included rows, in order |
//! | `log.exportCsv` | write the rows as a spreadsheet-safe CSV |
//! | `log.importCsv` | add the rows of a CSV (an export, or a spreadsheet with the same headers) |
//!
//! Rows live in [`Project::logs`], keyed by media item; times are media ticks, `end` exclusive.
//! Every mutation is one undo step and is checked before anything changes.

use std::collections::{BTreeMap, BTreeSet};

use filmcraft_project::{ItemId, LogEntry, LogEntryId, MAX_LOG_ENTRIES_PER_ITEM, MAX_LOG_TEXT_BYTES, MediaRef, Project, ProjectItem};
use filmcraft_time::{FrameRate, Tick, format_timecode_frames};
use serde_json::{Value, json};

mod import;

use crate::commands::{CommandSpec, NamedKeys, always, bad};
use crate::{EngineError, Result, Session};

const ADD: &str = "log.add";
const FROM_SOURCE: &str = "log.addFromSource";
const EDIT: &str = "log.edit";
const DELETE: &str = "log.delete";
const SET_INCLUDE: &str = "log.setInclude";
const SET_ORDER: &str = "log.setOrder";
const NUMBER_NEXT: &str = "log.numberNext";
const INSERT_ORDER: &str = "log.insertOrder";
const BUILD: &str = "log.buildSequence";
const LIST: &str = "log.list";
const EXPORT: &str = "log.exportCsv";
const IMPORT: &str = "log.importCsv";

/// The largest CSV `log.exportCsv` will write.
const MAX_CSV_BYTES: usize = 16 * 1024 * 1024;
/// The most rows one `log.edit`, `log.delete` or `log.setOrder` call may name.
const MAX_IDS: usize = 100_000;
/// The largest edit-order number.
const MAX_ORDER: u64 = 1_000_000;

// ---------- parameters ----------

fn tick_p(p: &Value, key: &str, cmd: &str) -> Result<Option<Tick>> {
    match p.get(key) {
        None => Ok(None),
        Some(v) => v.as_i64().map(|t| Some(Tick(t))).ok_or_else(|| bad(cmd, format!("`{key}` must be a whole number of ticks"))),
    }
}

fn text_p(p: &Value, key: &str, cmd: &str) -> Result<Option<String>> {
    match p.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .filter(|t| t.len() <= MAX_LOG_TEXT_BYTES)
            .map(|t| Some(t.to_owned()))
            .ok_or_else(|| bad(cmd, format!("`{key}` must be text of at most {MAX_LOG_TEXT_BYTES} bytes"))),
    }
}

/// `include`: absent = leave alone, `null` = undecided, a boolean = yes / no.
fn include_p(p: &Value, cmd: &str) -> Result<Option<Option<bool>>> {
    match p.get("include") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::Bool(b)) => Ok(Some(Some(*b))),
        Some(_) => Err(bad(cmd, "`include` must be true, false or null")),
    }
}

/// `order`: absent = leave alone, `null` = no position, a whole number from 1 = the position.
fn order_p(p: &Value, cmd: &str) -> Result<Option<Option<u32>>> {
    match p.get("order") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_ORDER).contains(n))
            .and_then(|n| u32::try_from(n).ok())
            .map(|n| Some(Some(n)))
            .ok_or_else(|| bad(cmd, format!("`order` must be null or a whole number from 1 to {MAX_ORDER}"))),
    }
}

/// The row fields a call may carry.
struct Fields {
    start: Option<Tick>,
    end: Option<Tick>,
    content: Option<String>,
    notes: Option<String>,
    include: Option<Option<bool>>,
    order: Option<Option<u32>>,
}

impl Fields {
    fn parse(p: &Value, cmd: &str) -> Result<Fields> {
        Ok(Fields {
            start: tick_p(p, "start", cmd)?,
            end: tick_p(p, "end", cmd)?,
            content: text_p(p, "content", cmd)?,
            notes: text_p(p, "notes", cmd)?,
            include: include_p(p, cmd)?,
            order: order_p(p, cmd)?,
        })
    }

    fn is_empty(&self) -> bool {
        self.start.is_none() && self.end.is_none() && self.content.is_none() && self.notes.is_none() && self.include.is_none() && self.order.is_none()
    }
}

fn item_id_p(p: &Value, key: &str, cmd: &str) -> Result<Option<ItemId>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(|n| Some(ItemId(n))).ok_or_else(|| bad(cmd, format!("`{key}` must be an item id"))),
    }
}

/// `entry` (one id) or `entries` (a list), as the marker commands take them.
fn entry_ids_p(p: &Value, cmd: &str) -> Result<Vec<LogEntryId>> {
    let ids: Vec<LogEntryId> = match (p.get("entry"), p.get("entries")) {
        (Some(_), Some(_)) => return Err(bad(cmd, "use `entry` or `entries`, not both")),
        (Some(v), None) => vec![LogEntryId(v.as_u64().ok_or_else(|| bad(cmd, "`entry` must be a log entry id"))?)],
        (None, Some(v)) => {
            let list =
                v.as_array().filter(|a| !a.is_empty() && a.len() <= MAX_IDS).ok_or_else(|| bad(cmd, format!("`entries` must hold 1 to {MAX_IDS} ids")))?;
            list.iter().map(|v| v.as_u64().map(LogEntryId).ok_or_else(|| bad(cmd, "`entries` must hold log entry ids"))).collect::<Result<_>>()?
        }
        (None, None) => return Err(bad(cmd, "need `entry` or `entries`")),
    };
    Ok(ids)
}

/// `0 <= start < end`, and inside the media when its length is known (`duration` ticks, 0 = unknown).
fn check_range(duration: i64, start: Tick, end: Tick, cmd: &str) -> Result<()> {
    if start.0 < 0 || end <= start {
        return Err(bad(cmd, "need 0 <= start < end, in media ticks"));
    }
    if duration > 0 && end.0 > duration {
        return Err(bad(cmd, "`end` is past the end of the media"));
    }
    Ok(())
}

fn sort_rows(rows: &mut [LogEntry]) {
    rows.sort_by_key(|e| (e.start, e.id));
}

// ---------- add ----------

fn add(s: &mut Session, p: &Value) -> Result<Value> {
    let item = match item_id_p(p, "item", ADD)? {
        Some(i) => i,
        None => s.state.source_item.ok_or_else(|| bad(ADD, "no item: pass `item` or open a clip in the Source monitor"))?,
    };
    let f = Fields::parse(p, ADD)?;
    let (Some(start), Some(end)) = (f.start, f.end) else {
        return Err(bad(ADD, "need `start` and `end` in ticks"));
    };
    insert(s, item, start, end, f, ADD)
}

fn add_from_source(s: &mut Session, p: &Value) -> Result<Value> {
    let src = match item_id_p(p, "item", FROM_SOURCE)? {
        Some(i) => i,
        None => s.state.source_item.ok_or_else(|| bad(FROM_SOURCE, "no item: pass `item` or open a clip in the Source monitor"))?,
    };
    let f = Fields::parse(p, FROM_SOURCE)?;
    if f.start.is_some() || f.end.is_some() {
        return Err(bad(FROM_SOURCE, "the range comes from the Source marks; use log.add to give `start` and `end`"));
    }
    let view = crate::clip_ops::source_view(s, src).ok_or_else(|| bad(FROM_SOURCE, "no such source"))?;
    let range = view.selected_range();
    let (start, end) = (range.start, range.end());
    insert(s, view.media, start, end, f, FROM_SOURCE)
}

fn insert(s: &mut Session, item: ItemId, start: Tick, end: Tick, f: Fields, cmd: &str) -> Result<Value> {
    let entry = s.edit("Add Log Entry", |project, _| {
        let duration = project.item(item).and_then(ProjectItem::as_media).map(|m| m.info.duration.0).ok_or_else(|| bad(cmd, "the item is not a media clip"))?;
        check_range(duration, start, end, cmd)?;
        if project.logs.get(&item).is_some_and(|rows| rows.len() >= MAX_LOG_ENTRIES_PER_ITEM) {
            return Err(bad(cmd, format!("a clip can have at most {MAX_LOG_ENTRIES_PER_ITEM} log entries")));
        }
        let id = LogEntryId(project.alloc_id());
        let rows = project.logs.entry(item).or_default();
        rows.push(LogEntry {
            id,
            start,
            end,
            content: f.content.unwrap_or_default(),
            include: f.include.unwrap_or(None),
            order: f.order.unwrap_or(None),
            notes: f.notes.unwrap_or_default(),
        });
        sort_rows(rows);
        Ok(id)
    })?;
    Ok(json!({"entry": entry.0, "item": item.0}))
}

// ---------- edit / delete ----------

fn edit(s: &mut Session, p: &Value) -> Result<Value> {
    change(s, p, EDIT, false)
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    change(s, p, DELETE, true)
}

fn set_include(s: &mut Session, p: &Value) -> Result<Value> {
    if p.get("include").is_none() {
        return Err(bad(SET_INCLUDE, "need `include`: true, false or null"));
    }
    if ["start", "end", "content", "notes", "order"].iter().any(|k| p.get(*k).is_some()) {
        return Err(bad(SET_INCLUDE, "only `include` can be set; use log.edit for the other fields"));
    }
    change(s, p, SET_INCLUDE, false)
}

fn change(s: &mut Session, p: &Value, cmd: &str, delete: bool) -> Result<Value> {
    let ids: BTreeSet<LogEntryId> = entry_ids_p(p, cmd)?.into_iter().collect();
    let f = Fields::parse(p, cmd)?;
    if !delete && f.is_empty() {
        return Err(bad(cmd, "nothing to change"));
    }
    if ids.len() > 1 && (f.start.is_some() || f.end.is_some()) {
        return Err(bad(cmd, "`start` and `end` can only change one entry at a time"));
    }
    let label = if delete { "Delete Log Entries" } else { "Edit Log Entries" };
    s.edit(label, |project, _| {
        let found = change_rows(project, &ids, &f, delete, cmd)?;
        if found != ids.len() {
            return Err(bad(cmd, "no such log entry"));
        }
        project.logs.retain(|_, rows| !rows.is_empty());
        Ok(())
    })?;
    Ok(json!({"count": ids.len()}))
}

/// Edit or delete the named rows; how many were found.
fn change_rows(project: &mut Project, ids: &BTreeSet<LogEntryId>, f: &Fields, delete: bool, cmd: &str) -> Result<usize> {
    let items = &project.items;
    let mut found = 0;
    for (item, rows) in project.logs.iter_mut() {
        if !rows.iter().any(|e| ids.contains(&e.id)) {
            continue;
        }
        if delete {
            let before = rows.len();
            rows.retain(|e| !ids.contains(&e.id));
            found += before - rows.len();
            continue;
        }
        let duration = items.get(item).and_then(ProjectItem::as_media).map_or(0, |m| m.info.duration.0);
        for e in rows.iter_mut().filter(|e| ids.contains(&e.id)) {
            found += 1;
            apply(e, f, duration, cmd)?;
        }
        sort_rows(rows);
    }
    Ok(found)
}

fn apply(e: &mut LogEntry, f: &Fields, duration: i64, cmd: &str) -> Result<()> {
    if f.start.is_some() || f.end.is_some() {
        let (start, end) = (f.start.unwrap_or(e.start), f.end.unwrap_or(e.end));
        check_range(duration, start, end, cmd)?;
        (e.start, e.end) = (start, end);
    }
    if let Some(v) = &f.content {
        e.content.clone_from(v);
    }
    if let Some(v) = &f.notes {
        e.notes.clone_from(v);
    }
    if let Some(v) = f.include {
        e.include = v;
    }
    if let Some(v) = f.order {
        e.order = v;
    }
    Ok(())
}

// ---------- order ----------

/// `log.setOrder {entries:[id…]}`: the listed rows get order 1, 2, 3… in that sequence; rows not
/// listed keep theirs.
fn set_order(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = entry_ids_p(p, SET_ORDER)?;
    if ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
        return Err(bad(SET_ORDER, "`entries` names a log entry twice"));
    }
    let position: BTreeMap<LogEntryId, u32> =
        ids.iter().enumerate().map(|(i, id)| u32::try_from(i + 1).map(|n| (*id, n)).map_err(|_| bad(SET_ORDER, "too many entries"))).collect::<Result<_>>()?;
    s.edit("Set Log Order", |project, _| {
        let mut found = 0;
        for e in project.logs.values_mut().flatten() {
            if let Some(n) = position.get(&e.id) {
                e.order = Some(*n);
                found += 1;
            }
        }
        if found != ids.len() {
            return Err(bad(SET_ORDER, "no such log entry"));
        }
        Ok(())
    })?;
    Ok(json!({"count": ids.len()}))
}

/// `log.numberNext {entries:[id…]}`: each listed row without a number gets the next number that
/// no row of the project uses yet (the largest in use plus one, then on), in the given sequence.
/// Rows that already have a number keep it.
fn number_next(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = entry_ids_p(p, NUMBER_NEXT)?;
    if ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
        return Err(bad(NUMBER_NEXT, "`entries` names a log entry twice"));
    }
    let mut count = 0usize;
    let known = s.project.logs.values().flatten().filter(|e| ids.contains(&e.id)).count();
    if known != ids.len() {
        return Err(bad(NUMBER_NEXT, "no such log entry"));
    }
    let used = s.project.logs.values().flatten().filter_map(|e| e.order).max().unwrap_or(0);
    let needs: BTreeSet<LogEntryId> = s.project.logs.values().flatten().filter(|e| e.order.is_none() && ids.contains(&e.id)).map(|e| e.id).collect();
    if needs.is_empty() {
        return Ok(json!({"count": 0}));
    }
    if u64::from(used).saturating_add(u64::try_from(needs.len()).unwrap_or(u64::MAX)) > MAX_ORDER {
        return Err(bad(NUMBER_NEXT, format!("numbers go up to {MAX_ORDER}")));
    }
    let mut next = used;
    let assigned: BTreeMap<LogEntryId, u32> = ids
        .iter()
        .filter(|id| needs.contains(id))
        .map(|id| {
            next += 1;
            (*id, next)
        })
        .collect();
    s.edit("Number Log Entries", |project, _| {
        for e in project.logs.values_mut().flatten() {
            if let Some(n) = assigned.get(&e.id) {
                e.order = Some(*n);
                count += 1;
            }
        }
        Ok(())
    })?;
    Ok(json!({"count": count, "first": used + 1}))
}

/// `log.insertOrder {entry, order}`: the row gets number `order`. When another row already has
/// that number, every other row numbered `order` or higher moves up by one to make room; when it
/// is free nothing else changes. Refused when a moved number would pass the largest allowed.
fn insert_order(s: &mut Session, p: &Value) -> Result<Value> {
    let id = match entry_ids_p(p, INSERT_ORDER)?.as_slice() {
        [one] if p.get("entry").is_some() => *one,
        _ => return Err(bad(INSERT_ORDER, "need one `entry`")),
    };
    let Some(Some(n)) = order_p(p, INSERT_ORDER)? else {
        return Err(bad(INSERT_ORDER, format!("need `order`: a whole number from 1 to {MAX_ORDER}")));
    };
    if !s.project.logs.values().flatten().any(|e| e.id == id) {
        return Err(bad(INSERT_ORDER, "no such log entry"));
    }
    let taken = s.project.logs.values().flatten().any(|e| e.id != id && e.order == Some(n));
    let shifted = if taken { s.project.logs.values().flatten().filter(|e| e.id != id && e.order.is_some_and(|o| o >= n)).count() } else { 0 };
    let top = s.project.logs.values().flatten().filter(|e| e.id != id).filter_map(|e| e.order).max().unwrap_or(0);
    if taken && u64::from(top) + 1 > MAX_ORDER {
        return Err(bad(INSERT_ORDER, format!("numbers go up to {MAX_ORDER}")));
    }
    s.edit("Insert Log Order", |project, _| {
        for e in project.logs.values_mut().flatten() {
            if e.id == id {
                e.order = Some(n);
            } else if taken && let Some(o) = e.order.filter(|o| *o >= n) {
                e.order = Some(o.saturating_add(1));
            }
        }
        Ok(())
    })?;
    Ok(json!({"entry": id.0, "order": n, "shifted": shifted}))
}

// ---------- build a sequence ----------

/// The rows that go into the edit (include = yes) in edit order: by `order` (1 first), rows
/// without a number after the numbered ones, ties in file then time order.
pub fn included_rows(project: &Project) -> Vec<Row<'_>> {
    let mut v: Vec<Row<'_>> = rows(project, None).into_iter().filter(|r| r.entry.include == Some(true)).collect();
    v.sort_by_key(|r| (r.entry.order.unwrap_or(u32::MAX), r.item, r.entry.start, r.entry.id));
    v
}

/// `fadeAudio` / `fadeVideo`: absent or `null` = none, else seconds from 0.1 to 10.
fn fade_p(p: &Value, key: &str) -> Result<Option<f64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_f64().filter(|x| x.is_finite() && (0.1..=10.0).contains(x)).map(Some).ok_or_else(|| bad(BUILD, format!("`{key}` must be null or seconds from 0.1 to 10"))),
    }
}

/// The clip ids that start after the first one on the first video and the first audio track
/// that hold clips: the incoming clip of each cut.
fn incoming_clips(s: &Session) -> (Vec<u64>, Vec<u64>) {
    let Some(q) = s.active_sequence() else { return (Vec::new(), Vec::new()) };
    let of = |tracks: &[filmcraft_project::Track]| {
        let mut items: Vec<(Tick, u64)> = tracks.iter().find(|t| !t.items.is_empty()).map(|t| t.items.iter().map(|c| (c.start, c.id.0)).collect()).unwrap_or_default();
        items.sort();
        items.into_iter().skip(1).map(|(_, id)| id).collect::<Vec<u64>>()
    };
    (of(&q.video_tracks), of(&q.audio_tracks))
}

/// A dip to black (picture) or a constant-power crossfade (sound), `secs` long and centered on the
/// cut before each of `clips`. How many could not be added.
fn fade_cuts(s: &mut Session, clips: &[u64], command: &str, effect: &str, secs: f64) -> usize {
    let frames = (secs * s.sequence_rate().as_f64()).round().clamp(1.0, 100_000.0) as i64;
    clips.iter().filter(|id| s.execute(command, json!({"clip": **id, "effect": effect, "edge": "in", "frames": frames})).is_err()).count()
}

/// `log.buildSequence {name?, bin?}`: a new sequence that holds every row with `include` = yes,
/// back to back, each as the marked range of its file. Rows go by `order` (1 first); rows without a
/// number follow the numbered ones, in file then time order, as do ties. `fadeVideo` / `fadeAudio`
/// (seconds, 0.1 to 10) add a dip to black / a constant-power crossfade of that length centered on
/// each cut; cuts where one cannot be added are counted in `fadesSkipped`. One undo step; refused,
/// with nothing changed, when no row is included.
fn build_sequence(s: &mut Session, p: &Value) -> Result<Value> {
    let name = text_p(p, "name", BUILD)?;
    let (fade_video, fade_audio) = (fade_p(p, "fadeVideo")?, fade_p(p, "fadeAudio")?);
    let bin = p.get("bin").filter(|v| !v.is_null());
    if bin.is_some_and(|v| v.as_u64().is_none()) {
        return Err(bad(BUILD, "`bin` must be a bin id"));
    }
    let picked: Vec<(ItemId, Tick, Tick)> = included_rows(&s.project).iter().map(|r| (r.item, r.entry.start, r.entry.end)).collect();
    if picked.is_empty() {
        return Err(bad(BUILD, "no log entry is included: set Include to Yes on the rows to use"));
    }
    s.edit_group("Build Sequence from Log", |s| {
        let mut params = json!({});
        if let Some(n) = &name {
            params["name"] = json!(n);
        }
        if let Some(b) = bin {
            params["bin"] = b.clone();
        }
        let made = s.execute("file.newSequence", params)?;
        let mut placed = 0usize;
        for (i, (item, start, end)) in picked.iter().enumerate() {
            let at = s.active_sequence().map(|q| q.duration()).unwrap_or_default();
            let mut place = json!({"item": item.0, "time": at.0, "sourceIn": start.0, "duration": end.0 - start.0});
            // the first clip gives the empty sequence its frame size and rate
            if i == 0 {
                place["matchSequence"] = json!(true);
            }
            s.execute("timeline.place", place)?;
            placed += 1;
        }
        let (video, audio) = incoming_clips(s);
        let mut skipped = 0;
        if let Some(secs) = fade_video {
            skipped += fade_cuts(s, &video, "sequence.applyVideoTransition", "dip_to_black", secs);
        }
        if let Some(secs) = fade_audio {
            skipped += fade_cuts(s, &audio, "sequence.applyAudioTransition", "constant_power", secs);
        }
        Ok(json!({"sequence": made["sequence"], "clips": placed, "fadesSkipped": skipped}))
    })
}

// ---------- list / export ----------

/// The file name of a media item (its name when it is not a file).
fn file_name(item: &ProjectItem) -> String {
    match item.as_media().map(|m| &m.media) {
        Some(MediaRef::File { path }) => path.rsplit(['/', '\\']).find(|part| !part.is_empty()).unwrap_or(path).to_owned(),
        _ => item.name.clone(),
    }
}

/// `t` as a timecode at `rate` (media time, no drop frame); empty when the rate is unusable.
pub fn timecode(rate: FrameRate, t: Tick) -> String {
    let fps = rate.as_f64();
    if fps.is_finite() && fps > 0.0 {
        format_timecode_frames(rate.frame_at(t), rate, false)
    } else {
        String::new()
    }
}

/// Media ticks of a time typed into the table (`hh:mm:ss:ff` or a plain frame count) at `rate`;
/// `None` when it is not a time or lies outside the supported range.
pub fn parse_time(rate: FrameRate, text: &str) -> Option<Tick> {
    let fps = rate.as_f64();
    if !(fps.is_finite() && fps > 0.0) {
        return None;
    }
    // `into_iter().next()` reads the frame whether the parser reports failure as an Option or a Result
    let frame = filmcraft_time::parse_timecode(text.trim(), rate, false, 0).into_iter().next()?;
    (0..=rate.frame_at(Tick::MAX)).contains(&frame).then(|| rate.tick_of(frame))
}

/// The path of a media item's file, empty when it is not a file (a generator).
fn media_path(item: &ProjectItem) -> String {
    match item.as_media().map(|m| &m.media) {
        Some(MediaRef::File { path }) => path.clone(),
        _ => String::new(),
    }
}

/// The full path of a stored media path: a path stored relative to the project file (as saved
/// projects keep media next to them) is taken from the project's folder, the way the file is
/// opened. The stored path itself when it cannot be resolved (an unsaved project).
pub fn absolute_path(pool: &crate::MediaPool, stored: &str) -> String {
    if stored.is_empty() {
        return String::new();
    }
    pool.follow(stored).unwrap_or_else(|_| stored.to_owned())
}

/// One row with the facts of its media item, in file then time order.
pub struct Row<'a> {
    pub item: ItemId,
    pub file: String,
    /// The path as the project stores it (maybe relative to the project file; see [`absolute_path`]);
    /// empty for media that is not a file.
    pub path: String,
    pub rate: FrameRate,
    pub entry: &'a LogEntry,
}

/// Every row of the project (or of `only`), with its file name and frame rate.
pub fn rows(project: &Project, only: Option<ItemId>) -> Vec<Row<'_>> {
    let mut out = Vec::new();
    for (item, entries) in &project.logs {
        if only.is_some_and(|o| o != *item) {
            continue;
        }
        let Some(pi) = project.item(*item) else { continue };
        let rate = pi.frame_rate();
        let (file, path) = (file_name(pi), media_path(pi));
        out.extend(entries.iter().map(|entry| Row { item: *item, file: file.clone(), path: path.clone(), rate, entry }));
    }
    out
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    let only = item_id_p(p, "item", LIST)?;
    let rows = rows(&s.project, only);
    let entries: Vec<Value> = rows
        .iter()
        .map(|r| {
            let e = r.entry;
            json!({
                "entry": e.id.0, "item": r.item.0, "file": r.file, "start": e.start.0, "end": e.end.0,
                "path": absolute_path(&s.media, &r.path), "startTimecode": timecode(r.rate, e.start), "endTimecode": timecode(r.rate, e.end),
                "content": e.content, "include": e.include, "order": e.order, "notes": e.notes,
            })
        })
        .collect();
    Ok(json!({"count": entries.len(), "entries": entries}))
}

fn csv_field(out: &mut String, field: &str, free_text: bool) {
    out.push('"');
    // Quoting alone does not stop spreadsheet formulas: escape arbitrary text that starts like one.
    if free_text && field.trim_start().starts_with(['=', '+', '-', '@']) {
        out.push('\'');
    }
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
}

fn export_csv(s: &mut Session, p: &Value) -> Result<Value> {
    let path = p
        .get("path")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty() && !v.contains('\0'))
        .ok_or_else(|| bad(EXPORT, "need a non-empty `path` without NUL characters"))?;
    let only = item_id_p(p, "item", EXPORT)?;
    let mut csv = String::from("File,Item ID,Entry ID,Start Timecode,End Timecode,Start Ticks,End Ticks,Content,Include,Order,Notes,Path\r\n");
    let rows = rows(&s.project, only);
    for r in &rows {
        let e = r.entry;
        let include = match e.include {
            Some(true) => "yes",
            Some(false) => "no",
            None => "",
        };
        let fields = [
            r.file.clone(),
            r.item.0.to_string(),
            e.id.0.to_string(),
            timecode(r.rate, e.start),
            timecode(r.rate, e.end),
            e.start.0.to_string(),
            e.end.0.to_string(),
            e.content.clone(),
            include.to_owned(),
            e.order.map(|n| n.to_string()).unwrap_or_default(),
            e.notes.clone(),
            absolute_path(&s.media, &r.path),
        ];
        for (i, field) in fields.iter().enumerate() {
            if i > 0 {
                csv.push(',');
            }
            csv_field(&mut csv, field, matches!(i, 0 | 7 | 10 | 11));
        }
        csv.push_str("\r\n");
        if csv.len() > MAX_CSV_BYTES {
            return Err(bad(EXPORT, "the log exceeds the 16 MiB limit"));
        }
    }
    s.services.write_file(path, csv.as_bytes()).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    Ok(json!({"path": path, "entries": rows.len(), "bytes": csv.len()}))
}

// ---------- registry ----------

fn spec(id: &'static str, label: &'static str, params: &'static str, run: fn(&mut Session, &Value) -> Result<Value>, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal, named: NamedKeys::NONE }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec(LIST, "List Log Entries", r#"{"item":id?}"#, list, false),
        spec(ADD, "Add Log Entry", r#"{"item":id?,"start":ticks,"end":ticks,"content":str?,"include":bool|null?,"order":int|null?,"notes":str?}"#, add, true),
        spec(
            FROM_SOURCE,
            "Add Log Entry from Source Marks",
            r#"{"item":id?,"content":str?,"include":bool|null?,"order":int|null?,"notes":str?}"#,
            add_from_source,
            true,
        ),
        spec(
            EDIT,
            "Edit Log Entries",
            r#"{"entry":logId} | {"entries":[logId]} plus {"start":ticks?,"end":ticks?,"content":str?,"include":bool|null?,"order":int|null?,"notes":str?}"#,
            edit,
            true,
        ),
        spec(DELETE, "Delete Log Entries", r#"{"entry":logId} | {"entries":[logId]}"#, delete, true),
        spec(SET_INCLUDE, "Set Log Include", r#"{"entry":logId | "entries":[logId],"include":bool|null}"#, set_include, true),
        spec(SET_ORDER, "Set Log Order", r#"{"entries":[logId in edit order]}"#, set_order, true),
        spec(NUMBER_NEXT, "Number Log Entries", r#"{"entries":[logId in edit order]}"#, number_next, true),
        spec(INSERT_ORDER, "Insert Log Order", r#"{"entry":logId,"order":int}"#, insert_order, true),
        spec(BUILD, "Build Sequence from Clip Log", r#"{"name":str?,"bin":binId?,"fadeVideo":seconds|null?,"fadeAudio":seconds|null?}"#, build_sequence, true),
        spec(IMPORT, "Import Clip Log from CSV", r#"{"path":str}"#, import::import_csv, true),
        spec(EXPORT, "Export Clip Log as CSV", r#"{"path":str,"item":id?}"#, export_csv, false),
    ]
}
