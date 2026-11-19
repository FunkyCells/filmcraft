//! Tests of the Clip Log commands (`clip_log`): undo, persistence, hostile parameters, CSV.

use std::sync::{Arc, Mutex};

use filmcraft_project::ItemId;
use serde_json::{Value, json};

use crate::{Services, Session};

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

fn first_media(s: &Session) -> ItemId {
    s.project.items.values().filter(|i| i.as_media().is_some()).map(|i| i.id).min().expect("a media item")
}

/// A range inside the first media item: `(item, start, end)` in ticks.
fn range(s: &Session) -> (ItemId, i64, i64) {
    let id = first_media(s);
    let duration = s.project.item(id).and_then(|i| i.as_media()).map(|m| m.info.duration.0).unwrap_or(0);
    (id, 0, if duration >= 4 { duration / 2 } else { 1000 })
}

fn add(s: &mut Session, item: ItemId, start: i64, end: i64, content: &str) -> u64 {
    s.execute("log.add", json!({"item": item.0, "start": start, "end": end, "content": content})).unwrap()["entry"].as_u64().unwrap()
}

fn entries(s: &mut Session) -> Vec<Value> {
    s.execute("log.list", json!({})).unwrap()["entries"].as_array().unwrap().clone()
}

#[test]
fn rows_are_added_edited_and_deleted_with_undo() {
    let mut s = demo();
    let (item, start, end) = range(&s);
    let before = s.project.to_json();
    let undo_depth = s.history.undo.len();
    let id = add(&mut s, item, start, end, "interview intro");
    assert_eq!(s.history.undo.len(), undo_depth + 1);
    let rows = entries(&mut s);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["content"], "interview intro");
    assert_eq!(rows[0]["include"], Value::Null);
    assert!(rows[0]["file"].as_str().is_some_and(|f| !f.is_empty()));

    s.execute("log.edit", json!({"entry": id, "content": "b-roll", "include": true, "order": 2, "notes": "good light"})).unwrap();
    let rows = entries(&mut s);
    assert_eq!((rows[0]["content"].clone(), rows[0]["include"].clone(), rows[0]["order"].clone()), (json!("b-roll"), json!(true), json!(2)));
    s.execute("log.setInclude", json!({"entries": [id], "include": null})).unwrap();
    assert_eq!(entries(&mut s)[0]["include"], Value::Null);

    s.execute("log.delete", json!({"entry": id})).unwrap();
    assert!(entries(&mut s).is_empty());
    assert!(s.project.logs.is_empty(), "an item without rows leaves no empty list behind");

    while s.history.undo.len() > undo_depth {
        s.execute("edit.undo", json!({})).unwrap();
    }
    assert_eq!(s.project.to_json(), before);
}

#[test]
fn rows_survive_save_and_load_and_old_files_load_empty() {
    let mut s = demo();
    let (item, start, end) = range(&s);
    let plain = filmcraft_format::encode(&s.project, false);
    assert!(!String::from_utf8_lossy(&plain).contains("\"logs\""), "a project without rows writes no `logs` key");
    assert!(filmcraft_format::decode(&plain).unwrap().project.logs.is_empty());

    let id = add(&mut s, item, start, end, "wide shot");
    s.execute("log.edit", json!({"entry": id, "include": false, "order": 3})).unwrap();
    let back = filmcraft_format::decode(&filmcraft_format::encode(&s.project, false)).unwrap().project;
    assert_eq!(back.logs, s.project.logs);
    assert!(back.next_id > id, "ids of saved rows are never handed out again");
}

#[test]
fn hostile_parameters_are_refused_and_change_nothing() {
    let mut s = demo();
    let (item, start, end) = range(&s);
    let id = add(&mut s, item, start, end, "ok");
    let before = s.project.to_json();
    let depth = s.history.undo.len();
    let huge = "x".repeat(70_000);
    let mut bad: Vec<(&str, Value)> = vec![
        ("log.add", json!({"item": item.0, "start": 10, "end": 10})),
        ("log.add", json!({"item": item.0, "start": 20, "end": 10})),
        ("log.add", json!({"item": item.0, "start": -1, "end": 10})),
        ("log.add", json!({"item": item.0, "start": 0.5, "end": 10})),
        ("log.add", json!({"item": item.0, "start": "0", "end": 10})),
        ("log.add", json!({"item": item.0, "start": 0})),
        ("log.add", json!({"item": 999_999_999u64, "start": 0, "end": 10})),
        ("log.add", json!({"item": item.0, "start": 0, "end": 10, "content": huge})),
        ("log.add", json!({"item": item.0, "start": 0, "end": 10, "include": "yes"})),
        ("log.add", json!({"item": item.0, "start": 0, "end": 10, "order": 0})),
        ("log.add", json!({"item": item.0, "start": 0, "end": 10, "order": -3})),
        ("log.add", json!({"item": item.0, "start": 0, "end": 10, "order": u64::MAX})),
        ("log.edit", json!({"entry": id})),
        ("log.edit", json!({"entry": 999_999_999u64, "content": "x"})),
        ("log.edit", json!({"entry": id, "entries": [id], "content": "x"})),
        ("log.edit", json!({"entries": [], "content": "x"})),
        ("log.edit", json!({"entries": ["a"], "content": "x"})),
        ("log.edit", json!({"entry": id, "start": 50, "end": 40})),
        ("log.edit", json!({"entry": id, "content": 5})),
        ("log.delete", json!({"entry": 999_999_999u64})),
        ("log.delete", json!({})),
        ("log.setInclude", json!({"entries": [id]})),
        ("log.setInclude", json!({"entries": [id], "include": true, "content": "x"})),
        ("log.setOrder", json!({"entries": [id, id]})),
        ("log.setOrder", json!({"entries": [id, 999_999_999u64]})),
        ("log.list", json!({"item": "x"})),
        ("log.exportCsv", json!({})),
        ("log.exportCsv", json!({"path": "a\0b"})),
    ];
    let known_length = s.project.item(item).and_then(|i| i.as_media()).is_some_and(|m| m.info.duration.0 > 0);
    if known_length {
        bad.push(("log.add", json!({"item": item.0, "start": 0, "end": i64::MAX})));
    }
    for (cmd, params) in bad {
        assert!(s.execute(cmd, params.clone()).is_err(), "{cmd} {params} should be refused");
    }
    assert_eq!(s.project.to_json(), before, "a refused call changes nothing");
    assert_eq!(s.history.undo.len(), depth, "a refused call adds no undo step");
}

#[test]
fn a_multi_row_edit_is_atomic() {
    let mut s = demo();
    let (item, start, end) = range(&s);
    let a = add(&mut s, item, start, end, "a");
    let before = s.project.to_json();
    assert!(s.execute("log.edit", json!({"entries": [a, 999_999_999u64], "content": "changed"})).is_err());
    assert_eq!(s.project.to_json(), before);
}

#[test]
fn set_order_numbers_rows_in_the_given_sequence() {
    let mut s = demo();
    let (item, _, end) = range(&s);
    let a = add(&mut s, item, 0, end, "a");
    let b = add(&mut s, item, 0, end, "b");
    let c = add(&mut s, item, 0, end, "c");
    s.execute("log.setOrder", json!({"entries": [c, a, b]})).unwrap();
    let order = |rows: &[Value], id: u64| rows.iter().find(|r| r["entry"] == id).map(|r| r["order"].clone()).unwrap();
    let rows = entries(&mut s);
    assert_eq!((order(&rows, c), order(&rows, a), order(&rows, b)), (json!(1), json!(2), json!(3)));
    s.execute("edit.undo", json!({})).unwrap();
    assert!(entries(&mut s).iter().all(|r| r["order"].is_null()));
}

#[test]
fn number_next_gives_unused_numbers_and_keeps_existing_ones() {
    let mut s = demo();
    let (item, _, end) = range(&s);
    let a = add(&mut s, item, 0, end, "a");
    let b = add(&mut s, item, 0, end, "b");
    let c = add(&mut s, item, 0, end, "c");
    s.execute("log.edit", json!({"entry": b, "order": 5})).unwrap();
    let depth = s.history.undo.len();
    let r = s.execute("log.numberNext", json!({"entries": [c, b, a]})).unwrap();
    assert_eq!(r["count"], 2);
    let order = |rows: &[Value], id: u64| rows.iter().find(|r| r["entry"] == id).map(|r| r["order"].clone()).unwrap();
    let rows = entries(&mut s);
    assert_eq!((order(&rows, c), order(&rows, b), order(&rows, a)), (json!(6), json!(5), json!(7)));
    assert_eq!(s.history.undo.len(), depth + 1);
    // nothing left to number: no change and no undo step
    assert_eq!(s.execute("log.numberNext", json!({"entries": [a, b, c]})).unwrap()["count"], 0);
    assert_eq!(s.history.undo.len(), depth + 1);
    assert!(s.execute("log.numberNext", json!({"entries": [a, a]})).is_err());
    assert!(s.execute("log.numberNext", json!({"entries": [999_999_999u64]})).is_err());
    assert!(s.execute("log.numberNext", json!({})).is_err());
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(order(&entries(&mut s), a), Value::Null);
}

#[test]
fn a_row_can_be_taken_from_the_source_monitor_range() {
    let mut s = demo();
    let item = first_media(&s);
    s.execute("source.open", json!({"item": item.0})).unwrap();
    let wanted = crate::clip_ops::source_view(&s, item).unwrap().selected_range();
    s.execute("log.addFromSource", json!({"content": "from source"})).unwrap();
    let rows = entries(&mut s);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0]["start"].as_i64(), rows[0]["end"].as_i64()), (Some(wanted.start.0), Some(wanted.end().0)));
    assert!(s.execute("log.addFromSource", json!({"start": 1})).is_err(), "the range comes from the marks");
}

#[test]
fn rows_are_kept_in_time_order() {
    let mut s = demo();
    let (item, _, end) = range(&s);
    let late = add(&mut s, item, end / 2, end, "late");
    let early = add(&mut s, item, 0, end / 2, "early");
    let ids: Vec<u64> = entries(&mut s).iter().map(|r| r["entry"].as_u64().unwrap()).collect();
    assert_eq!(ids, vec![early, late]);
    s.execute("log.edit", json!({"entry": early, "start": end / 2, "end": end})).unwrap();
    let rows = entries(&mut s);
    assert_eq!(rows.len(), 2);
}

#[test]
fn rows_of_a_deleted_item_are_not_listed_and_their_ids_stay_reserved() {
    let mut s = demo();
    let (item, start, end) = range(&s);
    let id = add(&mut s, item, start, end, "kept in the file");
    let mut p = (*s.project).clone();
    p.items.remove(&item);
    assert!(p.max_id() >= id, "a row's id counts toward the project's largest id");
    s.project = Arc::new(p);
    assert!(entries(&mut s).is_empty());
}

#[derive(Default)]
struct Files {
    writes: Mutex<Vec<(String, Vec<u8>)>>,
}
impl Services for Files {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let writes = self.writes.lock().unwrap();
        writes.iter().rev().find(|(p, _)| p == path).map(|(_, data)| data.clone()).ok_or_else(|| std::io::ErrorKind::NotFound.into())
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        self.writes.lock().unwrap().push((path.into(), data.to_vec()));
        Ok(())
    }
}

#[test]
fn csv_export_quotes_text_and_neutralises_formulas() {
    let files = Arc::new(Files::default());
    let mut s = Session { services: files.clone(), ..Default::default() };
    s.execute("file.openDemoProject", json!({})).unwrap();
    let (item, start, end) = range(&s);
    let id = add(&mut s, item, start, end, "=HYPERLINK(\"http://example.com\")");
    s.execute("log.edit", json!({"entry": id, "include": true, "order": 1, "notes": "say \"hi\", then go"})).unwrap();
    let r = s.execute("log.exportCsv", json!({"path": "log.csv"})).unwrap();
    assert_eq!(r["entries"], 1);
    let writes = files.writes.lock().unwrap();
    let text = String::from_utf8(writes[0].1.clone()).unwrap();
    let mut lines = text.split("\r\n");
    assert!(lines.next().unwrap().starts_with("File,Item ID,Entry ID,"));
    let row = lines.next().unwrap();
    assert!(row.contains("\"'=HYPERLINK(\"\"http://example.com\"\")\""), "formula text is neutralised and quotes doubled: {row}");
    assert!(row.contains("\"yes\",\"1\",\"say \"\"hi\"\", then go\""), "{row}");
}

#[test]
fn typed_times_round_trip_and_junk_is_refused() {
    use crate::clip_log::{parse_time, timecode};
    use filmcraft_time::{FrameRate, Tick};
    let rate = FrameRate::default();
    let t = parse_time(rate, " 00:00:02:00 ").expect("a timecode");
    assert_eq!(parse_time(rate, &timecode(rate, t)), Some(t));
    assert_eq!(parse_time(rate, "00:00:00:00"), Some(Tick::ZERO));
    for junk in ["", "abc", "-5", "99:99:99:99:99"] {
        assert_eq!(parse_time(rate, junk), None, "{junk:?}");
    }
}

#[test]
fn typed_times_are_fitted_to_the_clip() {
    use crate::clip_log::{fit_time, parse_time};
    use filmcraft_time::{FrameRate, Tick};
    let rate = FrameRate::default();
    let frame = rate.tick_of(1).0;
    let (dur, other) = (rate.tick_of(100).0, Tick(rate.tick_of(50).0));
    assert_eq!(fit_time(rate, dur, true, other, Tick(rate.tick_of(10).0)), (Tick(rate.tick_of(10).0), false));
    assert_eq!(fit_time(rate, dur, true, other, Tick(-5)), (Tick(0), true));
    assert_eq!(fit_time(rate, dur, true, other, Tick(dur + 9)), (Tick(other.0 - frame), true));
    assert_eq!(fit_time(rate, dur, false, other, Tick(dur + 9)), (Tick(dur), true));
    assert_eq!(fit_time(rate, dur, false, other, Tick(0)), (Tick(other.0 + frame), true));
    assert_eq!(fit_time(rate, 0, false, other, Tick(dur * 3)), (Tick(dur * 3), false));
    assert_eq!(parse_time(rate, "1.5"), Some(Tick::from_seconds_f64(1.5)));
    assert_eq!(parse_time(rate, "nan"), None);
}

#[test]
fn build_sequence_uses_included_rows_in_order_as_one_undo_step() {
    let mut s = demo();
    let (item, _, end) = range(&s);
    let third = end / 3;
    let a = add(&mut s, item, 0, third, "a");
    let b = add(&mut s, item, third, 2 * third, "b");
    let c = add(&mut s, item, 2 * third, end, "c");
    // nothing included yet: refused, nothing changes
    let before = s.project.to_json();
    assert!(s.execute("log.buildSequence", json!({})).is_err());
    assert_eq!(s.project.to_json(), before);
    s.execute("log.edit", json!({"entry": a, "include": true, "order": 2})).unwrap();
    s.execute("log.edit", json!({"entry": b, "include": true, "order": 1})).unwrap();
    s.execute("log.edit", json!({"entry": c, "include": false, "order": 3})).unwrap();
    let depth = s.history.undo.len();
    let sequences = s.project.sequences().count();
    let r = s.execute("log.buildSequence", json!({"name": "From log"})).unwrap();
    assert_eq!(r["clips"], 2);
    assert_eq!(s.project.sequences().count(), sequences + 1);
    assert_eq!(s.history.undo.len(), depth + 1, "one undo step for the whole build");
    let seq = s.project.sequence(ItemId(r["sequence"].as_u64().unwrap())).unwrap();
    let mut clips: Vec<_> = seq.video_tracks.iter().flat_map(|t| t.items.iter()).collect();
    clips.sort_by_key(|c| c.start);
    assert_eq!(clips.len(), 2);
    assert!(clips[0].source_in > clips[1].source_in, "order 1 (b, later in the file) comes first, then order 2 (a)");
    assert_eq!(clips[1].start, clips[0].start + clips[0].duration, "back to back");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.sequences().count(), sequences);

    // fades: a dip to black on the one cut; hostile values are refused before anything changes
    let before = s.project.to_json();
    for bad in [json!(0), json!(-1), json!(99), json!("1")] {
        assert!(s.execute("log.buildSequence", json!({"fadeVideo": bad})).is_err());
        assert!(s.execute("log.buildSequence", json!({"fadeAudio": bad})).is_err());
    }
    assert_eq!(s.project.to_json(), before);
    let r = s.execute("log.buildSequence", json!({"fadeVideo": 0.5, "fadeAudio": 0.5})).unwrap();
    let seq = s.project.sequence(ItemId(r["sequence"].as_u64().unwrap())).unwrap();
    assert_eq!(seq.video_tracks.iter().map(|t| t.transitions.len()).sum::<usize>(), 1, "one dip to black at the cut: {r}");
}

#[test]
fn insert_order_moves_the_rows_at_or_above_up_by_one() {
    let mut s = demo();
    let (item, _, end) = range(&s);
    let ids: Vec<u64> = (0..4).map(|i| add(&mut s, item, 0, end, &format!("row {i}"))).collect();
    for (id, n) in ids.iter().zip([1, 2, 3]) {
        s.execute("log.edit", json!({"entry": id, "order": n})).unwrap();
    }
    let order = |s: &mut Session, id: u64| entries(s).iter().find(|r| r["entry"] == id).map(|r| r["order"].clone()).unwrap();
    // number 2 is taken: the new row takes it and the old 2 and 3 become 3 and 4
    let depth = s.history.undo.len();
    let r = s.execute("log.insertOrder", json!({"entry": ids[3], "order": 2})).unwrap();
    assert_eq!(r["shifted"], 2);
    let got: Vec<Value> = ids.iter().map(|id| order(&mut s, *id)).collect();
    assert_eq!(got, vec![json!(1), json!(3), json!(4), json!(2)]);
    assert_eq!(s.history.undo.len(), depth + 1);
    // a free number changes nothing else
    let r = s.execute("log.insertOrder", json!({"entry": ids[0], "order": 9})).unwrap();
    assert_eq!(r["shifted"], 0);
    assert_eq!(order(&mut s, ids[1]), json!(3));
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(order(&mut s, ids[3]), Value::Null);
    // hostile calls change nothing
    let before = s.project.to_json();
    for params in [
        json!({"entry": ids[0]}),
        json!({"entry": ids[0], "order": 0}),
        json!({"entry": ids[0], "order": -1}),
        json!({"entry": ids[0], "order": null}),
        json!({"entry": 999_999_999u64, "order": 1}),
        json!({"entries": [ids[0], ids[1]], "order": 1}),
        json!({"entry": ids[0], "order": 1_000_001u64}),
    ] {
        assert!(s.execute("log.insertOrder", params.clone()).is_err(), "{params}");
    }
    assert_eq!(s.project.to_json(), before);
    // the top number cannot be pushed past the limit
    s.execute("log.edit", json!({"entry": ids[1], "order": 1_000_000u64})).unwrap();
    let before = s.project.to_json();
    assert!(s.execute("log.insertOrder", json!({"entry": ids[3], "order": 1_000_000u64})).is_err());
    assert_eq!(s.project.to_json(), before);
}

fn with_files() -> (Session, Arc<Files>) {
    let files = Arc::new(Files::default());
    let mut s = Session { services: files.clone(), ..Default::default() };
    s.execute("file.openDemoProject", json!({})).unwrap();
    (s, files)
}

fn put(files: &Files, name: &str, bytes: &[u8]) {
    files.writes.lock().unwrap().push((name.into(), bytes.to_vec()));
}

#[test]
fn csv_import_round_trips_an_export_and_does_not_duplicate() {
    let (mut s, _files) = with_files();
    let (item, _, end) = range(&s);
    let a = add(&mut s, item, 0, end / 2, "=SUM(1)");
    s.execute("log.edit", json!({"entry": a, "include": true, "order": 2, "notes": "line one\nline \"two\", ok"})).unwrap();
    let b = add(&mut s, item, end / 2, end, "plain");
    let shown = entries(&mut s);
    s.execute("log.exportCsv", json!({"path": "log.csv"})).unwrap();
    s.execute("log.delete", json!({"entries": [a, b]})).unwrap();
    let depth = s.history.undo.len();
    let r = s.execute("log.importCsv", json!({"path": "log.csv"})).unwrap();
    assert_eq!(r["imported"], 2);
    assert_eq!(s.history.undo.len(), depth + 1, "one undo step");
    let strip = |rows: &[Value]| -> Vec<Value> {
        rows.iter()
            .map(|r| {
                let mut r = r.clone();
                r.as_object_mut().unwrap().remove("entry");
                r
            })
            .collect()
    };
    assert_eq!(strip(&entries(&mut s)), strip(&shown), "content, notes, include, order and times survive");
    // the same file again adds nothing and makes no undo step
    let r = s.execute("log.importCsv", json!({"path": "log.csv"})).unwrap();
    assert_eq!((r["imported"].clone(), r["duplicates"].clone()), (json!(0), json!(2)));
    assert_eq!(s.history.undo.len(), depth + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(entries(&mut s).is_empty());
}

#[test]
fn csv_import_reads_semicolons_timecodes_and_reports_what_it_skips() {
    let (mut s, files) = with_files();
    let (item, start, end) = range(&s);
    add(&mut s, item, start, end, "x");
    let first = entries(&mut s)[0].clone();
    let name = first["file"].as_str().unwrap().to_owned();
    s.execute("log.delete", json!({"entry": first["entry"]})).unwrap();
    let text = format!(
        "\u{feff}File;Start;Stop;Content;Include;Order\r\n{name};00:00:00:00;00:00:00:02;hello;yes;3\r\nno-such-file.mov;00:00:00:00;00:00:00:02;;;\r\n{name};abc;00:00:00:02;;;\r\n{name};00:00:00:00;00:00:00:02;x;maybe;\r\n{name};00:00:00:00;00:00:00:03;neg;;0\r\n"
    );
    put(&files, "sheet.csv", text.as_bytes());
    let r = s.execute("log.importCsv", json!({"path": "sheet.csv"})).unwrap();
    assert_eq!((r["imported"].clone(), r["skipped"].clone()), (json!(1), json!(4)), "{r}");
    assert_eq!(r["problems"].as_array().unwrap().len(), 4);
    let rows = entries(&mut s);
    assert_eq!((rows[0]["content"].clone(), rows[0]["include"].clone(), rows[0]["order"].clone()), (json!("hello"), json!(true), json!(3)));
}

#[test]
fn csv_import_refuses_unusable_files_and_changes_nothing() {
    let (mut s, files) = with_files();
    let before = s.project.to_json();
    let depth = s.history.undo.len();
    put(&files, "empty.csv", b"");
    put(&files, "nohead.csv", b"Foo,Bar\r\n1,2\r\n");
    put(&files, "nostop.csv", b"File,Start\r\na,00:00:00:00\r\n");
    put(&files, "unknown.csv", b"File,Start,Stop\r\nnope.mov,00:00:00:00,00:00:00:02\r\n");
    put(&files, "binary.csv", &[0xff, 0xfe, 0x00, 0x41]);
    put(&files, "open.csv", b"File,Start,Stop\r\n\"never closed,1,2\r\n");
    for name in ["empty.csv", "nohead.csv", "nostop.csv", "unknown.csv", "binary.csv", "open.csv", "missing.csv"] {
        assert!(s.execute("log.importCsv", json!({"path": name})).is_err(), "{name}");
    }
    for params in [json!({}), json!({"path": ""}), json!({"path": "a\0b"}), json!({"path": 5})] {
        assert!(s.execute("log.importCsv", params.clone()).is_err(), "{params}");
    }
    assert_eq!(s.project.to_json(), before);
    assert_eq!(s.history.undo.len(), depth);
}
