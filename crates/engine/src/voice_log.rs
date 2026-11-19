//! Voice control of the Clip Log: what the editor says while watching a clip in the Source monitor.
//!
//! | Say | What happens |
//! |---|---|
//! | "start" (also "start here", "new row") | a row is added at the Source playhead (its start) |
//! | "start at 1 minute 12" (also "1:12", "twelve seconds") | the same, at that time of the clip |
//! | "stop" (also "stop here", "end"; "stop at …") | the open row ends at the playhead (or the time said); what is said next is its content |
//! | "content" (optionally followed by text) | the cursor goes to the content of the active row; text after it is added |
//! | "include" / "exclude" | the active row's Include is set to Yes / No |
//! | "order 3" (also "number three") | the active row gets that Order; another row with it keeps it and the rows from it up move up by one |
//! | "next row" | the row below becomes the active one (the first row when none is) |
//! | "delete row" | the active row is deleted |
//! | anything else | added to the content of the active row, after Stop or Content; ignored before that |
//!
//! Speech is recognised by [`crate::voice_listen`]; this module only interprets text, so the whole
//! flow can be driven and tested with `voice.logHear {text}` and no microphone. Each step is one
//! `log.*` command, so every spoken action is one undo step. The command words are English.
//!
//! | Id | What |
//! |---|---|
//! | `voice.logHear` | act on a heard phrase (`text`), at the Source playhead or at `at` (media ticks) |
//! | `voice.logState` | the phase (`idle`, `open`, `writing`), the active row and the last phrase |

use filmcraft_project::ItemId;
use filmcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, always, bad, str_p};
use crate::{Result, Session};

const HEAR: &str = "voice.logHear";
const STATE: &str = "voice.logState";

/// Phrases a recogniser invents from silence or noise.
const NOISE: &[&str] = &["you", "thank you", "thanks for watching", "thank you for watching", "bye"];

/// The longest time a phrase may name, in seconds (a day).
const MAX_SPOKEN_SECONDS: f64 = 86_400.0;

/// What a heard phrase means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Begin a row, at the media time said (ticks) or at the playhead.
    Start(Option<i64>),
    /// End the open row, at the media time said (ticks) or at the playhead.
    Stop(Option<i64>),
    /// "content", with the text spoken after it (may be empty).
    Content(String),
    Include,
    Exclude,
    /// Give the active row this Order.
    Order(u32),
    NextRow,
    DeleteRow,
    /// Free text.
    Text(String),
    /// Nothing worth acting on.
    Ignore,
}

/// Where the voice flow is for the current row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// No row is being logged by voice.
    #[default]
    Idle,
    /// "start" was said; the row waits for "stop".
    Open,
    /// The row has its end; text is added to its content.
    Writing,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Open => "open",
            Phase::Writing => "writing",
        }
    }
}

/// The voice flow of a session (not saved with the project).
#[derive(Default)]
pub struct VoiceLog {
    pub phase: Phase,
    /// The row being logged by voice.
    pub entry: Option<u64>,
    /// The last phrase, tidied.
    pub heard: String,
    /// What it did, or why it did nothing.
    pub note: String,
}

impl VoiceLog {
    /// The row text is being added to, if any.
    pub fn writing(&self) -> Option<u64> {
        self.entry.filter(|_| self.phase == Phase::Writing)
    }
}

// ------------------------------------------------------------------------------------- words

/// `heard` without bracketed sound notes (`[BLANK_AUDIO]`, `(music)`) and with single spaces.
pub fn tidy(heard: &str) -> String {
    let mut depth = 0usize;
    let mut kept = String::new();
    for c in heard.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => kept.push(c),
            _ => {}
        }
    }
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Lower-case words without the punctuation around them ("twenty-one" is two words).
fn plain_words(text: &str) -> Vec<String> {
    text.replace('-', " ").split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()).filter(|w| !w.is_empty()).collect()
}

fn small(w: &str) -> Option<u32> {
    const NAMES: [&str; 20] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen",
        "seventeen", "eighteen", "nineteen",
    ];
    NAMES.iter().position(|n| *n == w).and_then(|i| u32::try_from(i).ok())
}

fn tens(w: &str) -> Option<u32> {
    const NAMES: [&str; 8] = ["twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];
    NAMES.iter().position(|n| *n == w).and_then(|i| u32::try_from(i + 2).ok()).map(|n| n * 10)
}

/// A whole number written as digits ("12") or words ("twelve", "twenty one").
fn number(words: &[String]) -> Option<u32> {
    match words {
        [w] => w.parse::<u32>().ok().or_else(|| small(w)).or_else(|| tens(w)),
        [t, u] => Some(tens(t)? + small(u).filter(|n| (1..10).contains(n))?),
        _ => None,
    }
}

/// Seconds for a unit word.
fn unit(w: &str) -> Option<f64> {
    match w {
        "hour" | "hours" => Some(3600.0),
        "minute" | "minutes" | "min" | "mins" => Some(60.0),
        "second" | "seconds" | "sec" | "secs" => Some(1.0),
        _ => None,
    }
}

/// "1:12" or "1:02:03" as seconds.
fn clock(token: &str) -> Option<f64> {
    let parts: Vec<u32> = token.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    match parts.as_slice() {
        [m, s] => Some(f64::from(*m) * 60.0 + f64::from(*s)),
        [h, m, s] => Some(f64::from(*h) * 3600.0 + f64::from(*m) * 60.0 + f64::from(*s)),
        _ => None,
    }
}

/// A time said aloud, in seconds: "1 minute 12", "one minute twelve seconds", "1:12", "twelve
/// seconds", "1 hour 5" (a bare number counts the next smaller unit; alone it is seconds).
pub fn spoken_seconds(words: &[String]) -> Option<f64> {
    let words: Vec<&String> = words.iter().filter(|w| w.as_str() != "and").collect();
    if let [one] = words.as_slice()
        && one.contains(':')
    {
        return clock(one).filter(|s| *s <= MAX_SPOKEN_SECONDS);
    }
    let (mut total, mut last) = (0.0, None::<f64>);
    let mut pending: Vec<String> = Vec::new();
    for w in words {
        if let Some(seconds) = unit(w) {
            total += f64::from(number(&pending)?) * seconds;
            pending.clear();
            last = Some(seconds);
        } else {
            pending.push(w.clone());
        }
    }
    if pending.is_empty() {
        last?;
    } else {
        let seconds = if last.is_some_and(|l| l >= 3600.0) { 60.0 } else { 1.0 };
        total += f64::from(number(&pending)?) * seconds;
    }
    (0.0..=MAX_SPOKEN_SECONDS).contains(&total).then_some(total)
}

/// A whole number said or typed ("3", "twelve", "twenty one"); `None` for anything else.
pub fn spoken_number(text: &str) -> Option<u32> {
    number(&plain_words(text))
}

/// A time said or typed ("1 minute 12", "1:12", "twelve seconds"), in seconds; `None` for anything else.
pub fn spoken_time(text: &str) -> Option<f64> {
    spoken_seconds(&plain_words(text))
}

/// Points the voice flow at `entry` (the ticked row or the row with the cursor): what is heard next
/// changes that row. Moving to another row means the editor is writing about it, so text goes in.
pub fn aim(s: &mut Session, entry: u64) {
    if s.voice_log.entry != Some(entry) && row_start(s, entry).is_some() {
        s.voice_log.entry = Some(entry);
        s.voice_log.phase = Phase::Writing;
    }
}

fn is_one_of(rest: &[String], options: &[&str]) -> bool {
    matches!(rest, [w] if options.contains(&w.as_str()))
}

/// What follows "start" or "stop": nothing, "here"/"now"/"row", or "at <time>". `Some(None)` = at
/// the playhead, `Some(Some(ticks))` = at that time, `None` = not a marker phrase.
fn marker(rest: &[String]) -> Option<Option<i64>> {
    match rest {
        [] => Some(None),
        [w] if matches!(w.as_str(), "here" | "now" | "row") => Some(None),
        [at, time @ ..] if at.as_str() == "at" => spoken_seconds(time).map(|s| Some(Tick::from_seconds_f64(s).0)),
        _ => None,
    }
}

/// A one-phrase command, if `words` is exactly one.
fn command(words: &[String]) -> Option<Intent> {
    let (head, rest) = words.split_first()?;
    let rows = ["row", "it", "entry"];
    match head.as_str() {
        "start" | "begin" => marker(rest).map(Intent::Start),
        "stop" | "end" => marker(rest).map(Intent::Stop),
        "new" if is_one_of(rest, &["row"]) => Some(Intent::Start(None)),
        "include" if rest.is_empty() || is_one_of(rest, &rows) => Some(Intent::Include),
        "exclude" if rest.is_empty() || is_one_of(rest, &rows) => Some(Intent::Exclude),
        "order" | "number" => {
            let rest = rest.strip_prefix(&["number".to_owned()]).unwrap_or(rest);
            number(rest).filter(|n| *n >= 1).map(Intent::Order)
        }
        "next" if rest.is_empty() || is_one_of(rest, &["row", "one", "entry"]) => Some(Intent::NextRow),
        "delete" | "remove" if is_one_of(rest, &["row", "entry", "it"]) => Some(Intent::DeleteRow),
        _ => None,
    }
}

/// What `heard` means for the log.
pub fn interpret(heard: &str) -> Intent {
    let text = tidy(heard);
    let words = plain_words(&text);
    if words.is_empty() || NOISE.contains(&words.join(" ").as_str()) {
        return Intent::Ignore;
    }
    if let Some(intent) = command(&words) {
        return intent;
    }
    if words.first().is_some_and(|w| w == "content") {
        let rest = text.split_once(char::is_whitespace).map_or("", |(_, rest)| rest);
        let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || ":,;-".contains(c));
        return Intent::Content(rest.to_owned());
    }
    Intent::Text(text)
}

// ------------------------------------------------------------------------------------- acting

/// Act on a heard phrase. `at` is the Source playhead (media ticks) when it began; `None` = now.
pub fn hear(s: &mut Session, text: &str, at: Option<Tick>) -> Result<Value> {
    s.voice_log.heard = tidy(text);
    let result = match interpret(text) {
        Intent::Start(said) => start(s, said.map(Tick).or(at)),
        Intent::Stop(said) => stop(s, said.map(Tick).or(at)),
        Intent::Content(rest) => content(s, &rest),
        Intent::Include => set_include(s, true),
        Intent::Exclude => set_include(s, false),
        Intent::Order(n) => set_order(s, n),
        Intent::NextRow => next_row(s),
        Intent::DeleteRow => delete_row(s),
        Intent::Text(t) => free_text(s, &t),
        Intent::Ignore => Ok(json!({"action": "ignored"})),
    };
    s.voice_log.note = match &result {
        Ok(v) => v["action"].as_str().unwrap_or("").to_owned(),
        Err(e) => e.to_string(),
    };
    result
}

/// The clip's frame rate and length in ticks.
fn media_facts(s: &Session, item: ItemId, cmd: &str) -> Result<(FrameRate, i64)> {
    let pi = s.project.item(item).ok_or_else(|| bad(cmd, "the Source clip is gone"))?;
    let duration = pi.as_media().map(|m| m.info.duration.0).ok_or_else(|| bad(cmd, "the Source item is not a media clip"))?;
    Ok((pi.frame_rate(), duration))
}

fn start(s: &mut Session, at: Option<Tick>) -> Result<Value> {
    let item = s.state.source_item.ok_or_else(|| bad(HEAR, "open a clip in the Source monitor first"))?;
    let at = at.unwrap_or(s.state.source_playhead);
    let (rate, duration) = media_facts(s, item, HEAR)?;
    let frame = rate.tick_of(1).0.max(1);
    if duration <= frame {
        return Err(bad(HEAR, "the Source clip is too short to log"));
    }
    let start = at.0.clamp(0, duration - frame);
    // the row is one frame long until "stop"
    let added = s.execute("log.add", json!({"item": item.0, "start": start, "end": start + frame}))?;
    s.voice_log.entry = added["entry"].as_u64();
    s.voice_log.phase = Phase::Open;
    Ok(json!({"action": "start", "entry": added["entry"], "start": start}))
}

/// `(item, start)` of a row.
fn row_start(s: &Session, entry: u64) -> Option<(ItemId, i64)> {
    crate::clip_log::rows(&s.project, None).into_iter().find(|r| r.entry.id.0 == entry).map(|r| (r.item, r.entry.start.0))
}

fn lose_row(s: &mut Session) {
    s.voice_log.phase = Phase::Idle;
    s.voice_log.entry = None;
}

/// The row being logged, if it still exists.
fn active(s: &mut Session) -> Result<u64> {
    let Some(entry) = s.voice_log.entry else {
        return Err(bad(HEAR, "no row to change: say \"start\" first"));
    };
    if row_start(s, entry).is_none() {
        lose_row(s);
        return Err(bad(HEAR, "the row being logged is gone"));
    }
    Ok(entry)
}

fn stop(s: &mut Session, at: Option<Tick>) -> Result<Value> {
    let (Phase::Open, Some(entry)) = (s.voice_log.phase, s.voice_log.entry) else {
        return Err(bad(HEAR, "nothing to stop: say \"start\" first"));
    };
    let Some((item, start)) = row_start(s, entry) else {
        lose_row(s);
        return Err(bad(HEAR, "the row being logged is gone"));
    };
    let (rate, duration) = media_facts(s, item, HEAR)?;
    let at = at.unwrap_or(s.state.source_playhead);
    let end = at.0.max(start + rate.tick_of(1).0.max(1)).min(duration);
    s.execute("log.edit", json!({"entry": entry, "end": end}))?;
    s.voice_log.phase = Phase::Writing;
    Ok(json!({"action": "stop", "entry": entry, "end": end}))
}

fn content(s: &mut Session, rest: &str) -> Result<Value> {
    let entry = active(s)?;
    s.voice_log.phase = Phase::Writing;
    if !rest.is_empty() {
        append(s, entry, rest)?;
    }
    Ok(json!({"action": "content", "entry": entry}))
}

fn free_text(s: &mut Session, text: &str) -> Result<Value> {
    // before "stop" the editor is watching and talking to someone else: nothing is taken down
    let Some(entry) = s.voice_log.writing() else {
        return Ok(json!({"action": "ignored"}));
    };
    append(s, entry, text)?;
    Ok(json!({"action": "text", "entry": entry}))
}

fn append(s: &mut Session, entry: u64, text: &str) -> Result<()> {
    let old = crate::clip_log::rows(&s.project, None).into_iter().find(|r| r.entry.id.0 == entry).map(|r| r.entry.content.clone());
    let Some(old) = old else {
        lose_row(s);
        return Err(bad(HEAR, "the row being logged is gone"));
    };
    let joined = if old.trim().is_empty() { text.to_owned() } else { format!("{} {}", old.trim_end(), text) };
    s.execute("log.edit", json!({"entry": entry, "content": joined}))?;
    Ok(())
}

fn set_include(s: &mut Session, yes: bool) -> Result<Value> {
    let entry = active(s)?;
    s.execute("log.setInclude", json!({"entries": [entry], "include": yes}))?;
    Ok(json!({"action": if yes { "include" } else { "exclude" }, "entry": entry}))
}

fn set_order(s: &mut Session, n: u32) -> Result<Value> {
    let entry = active(s)?;
    let taken = crate::clip_log::rows(&s.project, None).iter().any(|r| r.entry.id.0 != entry && r.entry.order == Some(n));
    // a number another row has moves that row and the ones above it up by one
    let command = if taken { "log.insertOrder" } else { "log.edit" };
    s.execute(command, json!({"entry": entry, "order": n}))?;
    Ok(json!({"action": "order", "entry": entry, "order": n}))
}

fn next_row(s: &mut Session) -> Result<Value> {
    let ids: Vec<u64> = crate::clip_log::rows(&s.project, None).iter().map(|r| r.entry.id.0).collect();
    let here = s.voice_log.entry.and_then(|e| ids.iter().position(|id| *id == e));
    let next = here.map_or(0, |i| i + 1);
    let Some(entry) = ids.get(next).copied() else {
        return Err(bad(HEAR, if ids.is_empty() { "the log has no rows" } else { "this is the last row" }));
    };
    s.voice_log.entry = Some(entry);
    s.voice_log.phase = Phase::Writing;
    Ok(json!({"action": "row", "entry": entry}))
}

fn delete_row(s: &mut Session) -> Result<Value> {
    let entry = active(s)?;
    s.execute("log.delete", json!({"entry": entry}))?;
    lose_row(s);
    Ok(json!({"action": "delete", "entry": entry}))
}

fn hear_command(s: &mut Session, p: &Value) -> Result<Value> {
    let text = str_p(p, "text").ok_or_else(|| bad(HEAR, "need `text`"))?;
    let at = p.get("at").map(|v| v.as_i64().ok_or_else(|| bad(HEAR, "`at` must be whole ticks"))).transpose()?.map(Tick);
    hear(s, text, at)
}

fn state_command(s: &mut Session, _: &Value) -> Result<Value> {
    let v = &s.voice_log;
    Ok(json!({"phase": v.phase.name(), "entry": v.entry, "heard": v.heard, "note": v.note}))
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    let spec = |id, label, params, run: fn(&mut Session, &Value) -> Result<Value>, journal| CommandSpec {
        id,
        label,
        menu: &[],
        shortcut: None,
        params,
        enabled: always,
        run,
        journal,
        named: NamedKeys::NONE,
    };
    vec![
        spec(HEAR, "Voice: Log a Heard Phrase", r#"{"text":str,"at":ticks?}"#, hear_command, true),
        spec(STATE, "Voice: Log State", "{}", state_command, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        plain_words(text)
    }

    #[test]
    fn phrases_are_understood_whatever_the_case_or_punctuation() {
        assert_eq!(interpret("Start."), Intent::Start(None));
        assert_eq!(interpret(" start here! "), Intent::Start(None));
        assert_eq!(interpret("Stop"), Intent::Stop(None));
        assert_eq!(interpret("Content."), Intent::Content(String::new()));
        assert_eq!(interpret("Content: The man opens the door."), Intent::Content("The man opens the door.".into()));
        assert_eq!(interpret("the man opens the door"), Intent::Text("the man opens the door".into()));
        // a sentence that merely contains a command word is text
        assert_eq!(interpret("we start with the kitchen"), Intent::Text("we start with the kitchen".into()));
        assert_eq!(interpret("Include."), Intent::Include);
        assert_eq!(interpret("exclude row"), Intent::Exclude);
        assert_eq!(interpret("Next row."), Intent::NextRow);
        assert_eq!(interpret("Delete row"), Intent::DeleteRow);
        assert_eq!(interpret("Order 3."), Intent::Order(3));
        assert_eq!(interpret("order twenty-one"), Intent::Order(21));
        assert_eq!(interpret("number seven"), Intent::Order(7));
        // not an order: no number, zero
        assert_eq!(interpret("order the pizza"), Intent::Text("order the pizza".into()));
        assert_eq!(interpret("order 0"), Intent::Text("order 0".into()));
    }

    #[test]
    fn order_and_time_cells_accept_only_numbers_and_times() {
        assert_eq!(spoken_number("3."), Some(3));
        assert_eq!(spoken_number("twenty one"), Some(21));
        assert_eq!(spoken_number("the man opens"), None);
        assert_eq!(spoken_time("1 minute 12"), Some(72.0));
        assert_eq!(spoken_time("the man opens"), None);
        assert_eq!(spoken_time(""), None);
    }

    #[test]
    fn times_can_be_said_in_digits_or_words() {
        let secs = |t: &str| spoken_seconds(&words(t));
        assert_eq!(secs("1 minute 12"), Some(72.0));
        assert_eq!(secs("one minute twelve seconds"), Some(72.0));
        assert_eq!(secs("1 minute and 12 seconds"), Some(72.0));
        assert_eq!(secs("1:12"), Some(72.0));
        assert_eq!(secs("1:02:03"), Some(3723.0));
        assert_eq!(secs("twelve seconds"), Some(12.0));
        assert_eq!(secs("45"), Some(45.0));
        assert_eq!(secs("2 minutes"), Some(120.0));
        assert_eq!(secs("1 hour 5"), Some(3900.0));
        assert_eq!(secs("twenty five seconds"), Some(25.0));
        for bad in ["", "soon", "minutes", "one twelve", "1:xx", "99999999 seconds"] {
            assert_eq!(secs(bad), None, "{bad:?}");
        }
        assert_eq!(interpret("Start at 1 minute 12."), Intent::Start(Some(Tick::from_seconds_f64(72.0).0)));
        assert_eq!(interpret("stop at 2:30"), Intent::Stop(Some(Tick::from_seconds_f64(150.0).0)));
        // "start at" with no time is not a command
        assert_eq!(interpret("start at the beginning"), Intent::Text("start at the beginning".into()));
    }

    #[test]
    fn noise_and_sound_notes_are_ignored() {
        for t in ["", "   ", "[BLANK_AUDIO]", "(music)", "Thank you.", "you"] {
            assert_eq!(interpret(t), Intent::Ignore, "{t:?}");
        }
        assert_eq!(tidy("hello [noise] there (laughs)"), "hello there");
    }

    fn demo_with_source() -> (Session, ItemId, i64) {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let item = s.project.items.values().filter(|i| i.as_media().is_some()).map(|i| i.id).min().unwrap();
        let duration = s.project.item(item).and_then(|i| i.as_media()).map(|m| m.info.duration.0).unwrap();
        s.execute("source.open", json!({"item": item.0})).unwrap();
        (s, item, duration)
    }

    fn rows_of(s: &Session) -> Vec<(i64, i64, String)> {
        crate::clip_log::rows(&s.project, None).iter().map(|r| (r.entry.start.0, r.entry.end.0, r.entry.content.clone())).collect()
    }

    #[test]
    fn start_stop_and_dictation_fill_one_row() {
        let (mut s, _, d) = demo_with_source();
        let undo0 = s.history.undo.len();
        // talk before "start" is not taken down
        assert_eq!(hear(&mut s, "the weather is nice", Some(Tick(0))).unwrap()["action"], "ignored");
        assert!(rows_of(&s).is_empty());
        let started = hear(&mut s, "Start.", Some(Tick(d / 4))).unwrap();
        assert_eq!(started["action"], "start");
        assert_eq!(s.voice_log.phase, Phase::Open);
        // while the clip plays and the editor talks, nothing is added
        assert_eq!(hear(&mut s, "that is a good shot", None).unwrap()["action"], "ignored");
        assert_eq!(hear(&mut s, "Stop.", Some(Tick(d / 2))).unwrap()["action"], "stop");
        assert_eq!(s.voice_log.phase, Phase::Writing);
        hear(&mut s, "A man opens the door.", None).unwrap();
        hear(&mut s, "Content: and walks in", None).unwrap();
        let rows = rows_of(&s);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0, rows[0].1), (d / 4, d / 2));
        assert_eq!(rows[0].2, "A man opens the door. and walks in");
        // add, stop, two text edits: four undo steps
        assert_eq!(s.history.undo.len(), undo0 + 4);
        for _ in 0..4 {
            s.execute("edit.undo", json!({})).unwrap();
        }
        assert!(rows_of(&s).is_empty());
    }

    #[test]
    fn aiming_at_another_row_sends_what_is_heard_there() {
        let (mut s, item, d) = demo_with_source();
        hear(&mut s, "Start.", Some(Tick(0))).unwrap();
        hear(&mut s, "Stop.", Some(Tick(d / 4))).unwrap();
        let other = s.execute("log.add", json!({"item": item.0, "start": d / 2, "end": d / 2 + 1000})).unwrap()["entry"].as_u64().unwrap();
        aim(&mut s, other);
        hear(&mut s, "Include.", None).unwrap();
        hear(&mut s, "A second shot.", None).unwrap();
        let rows = crate::clip_log::rows(&s.project, None);
        let of = |id: u64| rows.iter().find(|r| r.entry.id.0 == id).map(|r| (r.entry.include, r.entry.content.clone())).unwrap();
        assert_eq!(of(other), (Some(true), "A second shot.".to_owned()));
        let first = rows.iter().find(|r| r.entry.id.0 != other).unwrap().entry.id.0;
        assert_eq!(of(first), (None, String::new()));
        aim(&mut s, u64::MAX); // a row that does not exist changes nothing
        assert_eq!(s.voice_log.entry, Some(other));
    }

    #[test]
    fn a_time_said_aloud_beats_the_playhead() {
        let (mut s, _, d) = demo_with_source();
        let second = filmcraft_time::TICKS_PER_SECOND;
        if d < 4 * second {
            return; // a demo clip too short to name whole seconds in
        }
        hear(&mut s, "Start at 1 second.", Some(Tick(0))).unwrap();
        hear(&mut s, "Stop at 0:03.", Some(Tick(0))).unwrap();
        let rows = rows_of(&s);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0, rows[0].1), (second, 3 * second));
    }

    #[test]
    fn content_moves_to_the_active_row_and_needs_one() {
        let (mut s, _, d) = demo_with_source();
        assert!(hear(&mut s, "content", None).is_err(), "no row yet");
        assert!(hear(&mut s, "stop", None).is_err(), "nothing to stop");
        hear(&mut s, "start", Some(Tick(d / 4))).unwrap();
        let said = hear(&mut s, "content the red door", None).unwrap();
        assert_eq!(said["action"], "content");
        assert_eq!(s.voice_log.phase, Phase::Writing);
        assert_eq!(rows_of(&s)[0].2, "the red door");
    }

    #[test]
    fn include_order_next_row_and_delete_work_on_the_active_row() {
        let (mut s, _, d) = demo_with_source();
        assert!(hear(&mut s, "include", None).is_err(), "no row yet");
        hear(&mut s, "start", Some(Tick(d / 8))).unwrap();
        hear(&mut s, "stop", Some(Tick(d / 4))).unwrap();
        hear(&mut s, "start", Some(Tick(d / 2))).unwrap();
        hear(&mut s, "stop", Some(Tick(d * 3 / 4))).unwrap();
        let ids: Vec<u64> = crate::clip_log::rows(&s.project, None).iter().map(|r| r.entry.id.0).collect();
        assert_eq!(ids.len(), 2);
        let field = |s: &Session, id: u64| crate::clip_log::rows(&s.project, None).iter().find(|r| r.entry.id.0 == id).map(|r| (r.entry.include, r.entry.order)).unwrap();
        // the second row is active
        hear(&mut s, "Include.", None).unwrap();
        hear(&mut s, "Order 1.", None).unwrap();
        assert_eq!(field(&s, ids[1]), (Some(true), Some(1)));
        // "next row" from the last row has nowhere to go; from none it starts at the first
        assert!(hear(&mut s, "next row", None).is_err());
        s.voice_log.entry = None;
        assert_eq!(hear(&mut s, "next row", None).unwrap()["entry"], ids[0]);
        // order 1 is taken: the other row moves up
        hear(&mut s, "order 1", None).unwrap();
        hear(&mut s, "exclude", None).unwrap();
        assert_eq!(field(&s, ids[0]), (Some(false), Some(1)));
        assert_eq!(field(&s, ids[1]).1, Some(2));
        // and text now goes to this row
        hear(&mut s, "a long walk", None).unwrap();
        assert!(rows_of(&s).iter().any(|r| r.2 == "a long walk"));
        hear(&mut s, "delete row", None).unwrap();
        assert_eq!(rows_of(&s).len(), 1);
        assert_eq!(s.voice_log.phase, Phase::Idle);
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(rows_of(&s).len(), 2, "one undo brings the row back");
    }

    #[test]
    fn a_stop_before_the_start_still_makes_a_valid_row() {
        let (mut s, _, d) = demo_with_source();
        hear(&mut s, "start", Some(Tick(d / 2))).unwrap();
        hear(&mut s, "stop", Some(Tick(d / 4))).unwrap();
        let rows = rows_of(&s);
        assert!(rows[0].1 > rows[0].0, "{rows:?}");
        // a start past the end of the clip is moved back inside it
        hear(&mut s, "start", Some(Tick(d * 10))).unwrap();
        assert!(rows_of(&s).iter().all(|r| r.1 <= d));
    }

    #[test]
    fn deleting_the_row_ends_the_flow_without_a_crash() {
        let (mut s, _, d) = demo_with_source();
        hear(&mut s, "start", Some(Tick(d / 4))).unwrap();
        let entry = s.voice_log.entry.unwrap();
        s.execute("log.delete", json!({"entry": entry})).unwrap();
        assert!(hear(&mut s, "stop", None).is_err());
        assert_eq!(s.voice_log.phase, Phase::Idle);
    }

    #[test]
    fn the_command_refuses_hostile_parameters() {
        let (mut s, _, _) = demo_with_source();
        for p in [json!({}), json!({"text": 5}), json!({"text": "start", "at": "now"}), json!({"text": "start", "at": 1.5})] {
            assert!(s.execute("voice.logHear", p.clone()).is_err(), "{p}");
        }
        let mut closed = Session::default();
        assert!(closed.execute("voice.logHear", json!({"text": "start"})).is_err(), "no Source clip");
        assert_eq!(s.execute("voice.logState", json!({})).unwrap()["phase"], "idle");
        // absurd times and orders are text or refused, never a crash
        for t in ["start at 99999999999 hours", "order 99999999999999", "stop at :::", "order", "start at"] {
            let _ = hear(&mut s, t, None);
        }
    }
}
