//! `log.importCsv`: read rows back from a CSV file, either this tool's own export or a
//! spreadsheet with the same headers (any column order; comma, semicolon or tab separated).
//!
//! Each row is matched to a media item of the project: by its `Path` when that is the clip's
//! path, else by `File` name when exactly one clip has it (`Item ID` breaks a tie between clips of
//! the same name). Rows that cannot be placed are skipped and reported, never guessed.

use std::collections::{BTreeMap, BTreeSet};

use filmcraft_project::{ItemId, LogEntry, LogEntryId, MAX_LOG_ENTRIES_PER_ITEM, MAX_LOG_TEXT_BYTES, Project};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use super::{IMPORT, MAX_CSV_BYTES, absolute_path, MAX_IDS, MAX_ORDER, check_range, file_name, media_path, parse_time, sort_rows};
use crate::commands::bad;
use crate::{EngineError, Result, Session};

/// How many skipped rows are described in the result.
const MAX_PROBLEMS: usize = 20;

/// The separator the header line uses: the most frequent of comma, semicolon and tab.
fn delimiter(text: &str) -> char {
    let head = text.lines().next().unwrap_or("");
    [',', ';', '\t'].into_iter().max_by_key(|d| (head.matches(*d).count(), *d == ',')).unwrap_or(',')
}

/// RFC 4180 records: quoted fields may hold separators, quotes (doubled) and line breaks.
fn parse_csv(text: &str) -> Result<Vec<Vec<String>>> {
    let delim = delimiter(text);
    let (mut rows, mut row, mut field) = (Vec::<Vec<String>>::new(), Vec::<String>::new(), String::new());
    let (mut quoted, mut chars) = (false, text.chars().peekable());
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
        } else if c == '"' && field.is_empty() {
            quoted = true;
        } else if c == delim {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut field));
            let record = std::mem::take(&mut row);
            if !(record.len() == 1 && record[0].is_empty()) {
                rows.push(record);
            }
            if rows.len() > MAX_IDS + 1 {
                return Err(bad(IMPORT, format!("more than {MAX_IDS} rows")));
            }
        } else {
            field.push(c);
        }
    }
    if quoted {
        return Err(bad(IMPORT, "a quoted cell is never closed"));
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

/// Where each field is in the header.
#[derive(Default)]
struct Columns {
    file: Option<usize>,
    path: Option<usize>,
    item: Option<usize>,
    start_code: Option<usize>,
    end_code: Option<usize>,
    start_ticks: Option<usize>,
    end_ticks: Option<usize>,
    content: Option<usize>,
    include: Option<usize>,
    order: Option<usize>,
    notes: Option<usize>,
}

impl Columns {
    fn from_header(header: &[String]) -> Result<Columns> {
        let mut c = Columns::default();
        for (i, h) in header.iter().enumerate() {
            let key: String = h.chars().filter(|ch| ch.is_alphanumeric()).flat_map(char::to_lowercase).collect();
            let slot = match key.as_str() {
                "file" | "filename" => &mut c.file,
                "path" | "filepath" | "fullpath" => &mut c.path,
                "itemid" => &mut c.item,
                "starttimecode" | "start" | "in" => &mut c.start_code,
                "endtimecode" | "end" | "stop" | "stoptimecode" | "out" => &mut c.end_code,
                "startticks" => &mut c.start_ticks,
                "endticks" | "stopticks" => &mut c.end_ticks,
                "content" | "description" => &mut c.content,
                "include" => &mut c.include,
                "order" => &mut c.order,
                "notes" => &mut c.notes,
                _ => continue,
            };
            slot.get_or_insert(i);
        }
        if c.file.is_none() && c.path.is_none() {
            return Err(bad(IMPORT, "the header needs a `File` or `Path` column"));
        }
        if (c.start_code.is_none() && c.start_ticks.is_none()) || (c.end_code.is_none() && c.end_ticks.is_none()) {
            return Err(bad(IMPORT, "the header needs Start and Stop (or Start Timecode / End Timecode) columns"));
        }
        Ok(c)
    }
}

fn cell(row: &[String], column: Option<usize>) -> &str {
    column.and_then(|i| row.get(i)).map_or("", String::as_str)
}

/// Undo the apostrophe the export puts before text that starts like a spreadsheet formula.
fn unescape(text: &str) -> String {
    match text.strip_prefix('\'') {
        Some(rest) if rest.trim_start().starts_with(['=', '+', '-', '@']) => rest.to_owned(),
        _ => text.to_owned(),
    }
}

/// The project's clips by path and by lower-case file name.
struct MediaIndex {
    by_path: BTreeMap<String, ItemId>,
    by_name: BTreeMap<String, Vec<ItemId>>,
}

impl MediaIndex {
    /// `absolute` turns a stored path into the full one, so a CSV that holds either matches.
    fn new(project: &Project, absolute: &dyn Fn(&str) -> String) -> MediaIndex {
        let mut index = MediaIndex { by_path: BTreeMap::new(), by_name: BTreeMap::new() };
        for item in project.items.values().filter(|i| i.as_media().is_some()) {
            let path = media_path(item);
            if !path.is_empty() {
                index.by_path.entry(absolute(&path)).or_insert(item.id);
                index.by_path.entry(path).or_insert(item.id);
            }
            index.by_name.entry(file_name(item).to_lowercase()).or_default().push(item.id);
        }
        index
    }

    fn resolve(&self, path: &str, file: &str, item_id: Option<u64>) -> std::result::Result<ItemId, String> {
        if let Some(id) = self.by_path.get(path).filter(|_| !path.is_empty()) {
            return Ok(*id);
        }
        let name = if file.is_empty() { path.rsplit(['/', '\\']).find(|part| !part.is_empty()).unwrap_or("") } else { file };
        if name.is_empty() {
            return Err("no file name".into());
        }
        match self.by_name.get(&name.to_lowercase()).map(Vec::as_slice) {
            None | Some([]) => Err(format!("no clip named {name} in this project")),
            Some([one]) => Ok(*one),
            Some(many) => item_id.map(ItemId).filter(|i| many.contains(i)).ok_or_else(|| format!("several clips are named {name}: a Path column tells them apart")),
        }
    }
}

struct Planned {
    item: ItemId,
    start: Tick,
    end: Tick,
    content: String,
    include: Option<bool>,
    order: Option<u32>,
    notes: String,
}

fn time_of(row: &[String], ticks: Option<usize>, code: Option<usize>, rate: filmcraft_time::FrameRate) -> std::result::Result<Tick, String> {
    let t = cell(row, ticks).trim();
    if !t.is_empty() {
        return t.parse::<i64>().map(Tick).map_err(|_| format!("`{t}` is not a number of ticks"));
    }
    let c = cell(row, code).trim();
    parse_time(rate, c).ok_or_else(|| format!("`{c}` is not a time"))
}

fn include_of(text: &str) -> std::result::Result<Option<bool>, String> {
    match text.trim().to_lowercase().as_str() {
        "yes" | "y" | "true" | "1" => Ok(Some(true)),
        "no" | "n" | "false" | "0" => Ok(Some(false)),
        "" => Ok(None),
        other => Err(format!("`{other}` is not yes or no")),
    }
}

fn order_of(text: &str) -> std::result::Result<Option<u32>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<u64>().ok().filter(|n| (1..=MAX_ORDER).contains(n)).and_then(|n| u32::try_from(n).ok()).map(Some).ok_or_else(|| format!("`{t}` is not an order from 1 to {MAX_ORDER}"))
}

fn text_of(text: &str, what: &str) -> std::result::Result<String, String> {
    let t = unescape(text);
    if t.len() > MAX_LOG_TEXT_BYTES { Err(format!("{what} is over {MAX_LOG_TEXT_BYTES} bytes")) } else { Ok(t) }
}

fn plan_row(project: &Project, index: &MediaIndex, c: &Columns, row: &[String]) -> std::result::Result<Planned, String> {
    let item_id = cell(row, c.item).trim().parse::<u64>().ok();
    let item = index.resolve(&unescape(cell(row, c.path).trim()), cell(row, c.file).trim(), item_id)?;
    let pi = project.item(item).ok_or("the clip is gone")?;
    let rate = pi.frame_rate();
    let start = time_of(row, c.start_ticks, c.start_code, rate)?;
    let end = time_of(row, c.end_ticks, c.end_code, rate)?;
    let duration = pi.as_media().map_or(0, |m| m.info.duration.0);
    check_range(duration, start, end, IMPORT).map_err(|e| e.to_string())?;
    Ok(Planned {
        item,
        start,
        end,
        content: text_of(cell(row, c.content), "Content")?,
        include: include_of(cell(row, c.include))?,
        order: order_of(cell(row, c.order))?,
        notes: text_of(cell(row, c.notes), "Notes")?,
    })
}

struct Plan {
    rows: Vec<Planned>,
    problems: Vec<String>,
    skipped: usize,
    duplicates: usize,
}

/// Decide, without changing anything, which data rows are added.
fn plan(project: &Project, c: &Columns, data: &[Vec<String>], absolute: &dyn Fn(&str) -> String) -> Plan {
    let index = MediaIndex::new(project, absolute);
    let mut known: BTreeSet<(ItemId, Tick, Tick)> = project.logs.iter().flat_map(|(i, rows)| rows.iter().map(|e| (*i, e.start, e.end))).collect();
    let mut per_item: BTreeMap<ItemId, usize> = project.logs.iter().map(|(i, rows)| (*i, rows.len())).collect();
    let mut out = Plan { rows: Vec::new(), problems: Vec::new(), skipped: 0, duplicates: 0 };
    for (n, row) in data.iter().enumerate() {
        let line = n + 2; // the header is line 1
        let outcome = plan_row(project, &index, c, row).and_then(|p| {
            if per_item.get(&p.item).copied().unwrap_or(0) >= MAX_LOG_ENTRIES_PER_ITEM {
                Err(format!("a clip can have at most {MAX_LOG_ENTRIES_PER_ITEM} log entries"))
            } else {
                Ok(p)
            }
        });
        match outcome {
            Ok(p) if !known.insert((p.item, p.start, p.end)) => out.duplicates += 1,
            Ok(p) => {
                *per_item.entry(p.item).or_default() += 1;
                out.rows.push(p);
            }
            Err(why) => {
                out.skipped += 1;
                if out.problems.len() < MAX_PROBLEMS {
                    out.problems.push(format!("line {line}: {why}"));
                }
            }
        }
    }
    out
}

pub(super) fn import_csv(s: &mut Session, p: &Value) -> Result<Value> {
    let path = p
        .get("path")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty() && !v.contains('\0'))
        .ok_or_else(|| bad(IMPORT, "need a non-empty `path` without NUL characters"))?;
    let bytes = s.services.read_file_bounded(path, MAX_CSV_BYTES as u64).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| bad(IMPORT, "the file is not UTF-8 text"))?;
    let table = parse_csv(text.strip_prefix('\u{feff}').unwrap_or(text))?;
    let Some((header, data)) = table.split_first() else { return Err(bad(IMPORT, "the file is empty")) };
    let columns = Columns::from_header(header)?;
    let plan = plan(&s.project, &columns, data, &|stored| absolute_path(&s.media, stored));
    if plan.rows.is_empty() && plan.duplicates == 0 {
        let why = plan.problems.first().map_or(String::new(), |p| format!(" ({p})"));
        return Err(bad(IMPORT, format!("no row could be imported{why}")));
    }
    if !plan.rows.is_empty() {
        s.edit("Import Clip Log", |project, _| {
            for r in &plan.rows {
                let id = LogEntryId(project.alloc_id());
                let entry = LogEntry { id, start: r.start, end: r.end, content: r.content.clone(), include: r.include, order: r.order, notes: r.notes.clone() };
                project.logs.entry(r.item).or_default().push(entry);
            }
            project.logs.values_mut().for_each(|rows| sort_rows(rows));
            Ok(())
        })?;
    }
    Ok(json!({"imported": plan.rows.len(), "skipped": plan.skipped, "duplicates": plan.duplicates, "problems": plan.problems}))
}
