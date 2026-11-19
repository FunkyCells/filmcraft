//! Listening for speech: the microphone, cut into phrases, recognised by Whisper.
//!
//! Off until `voice.listen` turns it on (the Clip Log's Voice button). While on, the host calls
//! [`poll`] every few tens of milliseconds: it reads what the input captured, cuts it into phrases
//! at pauses, and hands each phrase to a worker thread that runs the [`Transcriber`] (the session's,
//! else the Whisper model chosen in Settings). Each recognised phrase comes back as a [`Heard`] with
//! the Source playhead at the moment the phrase began, so "start" lands where the editor said it
//! even though recognition takes a moment. [`crate::voice_log`] turns phrases into log rows.
//!
//! In `auto` mode phrases are cut at pauses; in `push` mode only what is said while the talk key is
//! held counts (a phrase is sent when it is released).
//!
//! The microphone hears the speakers too: use headphones while the Source clip has sound.
//!
//! | Id | What |
//! |---|---|
//! | `voice.listen` | `{on: bool, mode: "auto"|"push"}`: start or stop listening (default on, `auto`) |
//! | `voice.talk` | `{down: bool}`: in `push` mode, hold (true) or release (false) the talk key |
//! | `voice.poll` | read the input, recognise, and apply each phrase to the Clip Log (headless hosts) |
//! | `voice.status` | whether it listens, the level, a phrase being recognised, the last error |

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use filmcraft_speech::{Options, Transcriber};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, always, bad, bool_p};
use crate::{EngineError, Result, Session};

/// Samples per second every recogniser takes.
const RATE: u32 = filmcraft_speech::SAMPLE_RATE;
/// Samples per analysis block (10 ms).
const BLOCK: usize = 160;
/// Blocks kept before a phrase starts (200 ms), so its first sound is not cut off.
const PRE_BLOCKS: usize = 20;
/// Blocks of quiet that end a phrase (600 ms).
const END_BLOCKS: usize = 60;
/// Fewest loud blocks that make a phrase (150 ms); shorter bursts are clicks and knocks.
const MIN_LOUD_BLOCKS: usize = 15;
/// The longest phrase, in samples (15 s); a longer one is cut here.
const MAX_SAMPLES: usize = 15 * RATE as usize;
/// The most captured frames taken from the input per read.
const MAX_READS: usize = 40;

/// A recognised phrase.
#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    pub text: String,
    /// The Source playhead (media ticks) when the phrase began.
    pub at: Tick,
}

// ------------------------------------------------------------------------------------- phrases

/// Cuts a stream of 16 kHz mono samples into phrases at pauses.
pub struct Segmenter {
    floor: f32,
    carry: Vec<f32>,
    pre: VecDeque<Vec<f32>>,
    phrase: Vec<f32>,
    speaking: bool,
    loud_run: usize,
    loud: usize,
    quiet: usize,
    at: Tick,
    /// Level of the last block, dBFS.
    pub level_db: f32,
    /// Loudest block of the last [`Segmenter::push`], dBFS.
    pub max_db: f32,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self {
            floor: -60.0,
            carry: Vec::new(),
            pre: VecDeque::new(),
            phrase: Vec::new(),
            speaking: false,
            loud_run: 0,
            loud: 0,
            quiet: 0,
            at: Tick(0),
            level_db: -120.0,
            max_db: -120.0,
        }
    }
}

impl Segmenter {
    /// Add samples; `now` is the Source playhead. Returns the phrases that ended.
    pub fn push(&mut self, samples: &[f32], now: Tick) -> Vec<(Vec<f32>, Tick)> {
        self.carry.extend_from_slice(samples);
        self.max_db = -120.0;
        let mut done = Vec::new();
        let whole = self.carry.len() / BLOCK * BLOCK;
        let blocks: Vec<f32> = self.carry.drain(..whole).collect();
        for block in blocks.chunks(BLOCK) {
            if let Some(phrase) = self.block(block, now) {
                done.push(phrase);
            }
        }
        done
    }

    fn block(&mut self, block: &[f32], now: Tick) -> Option<(Vec<f32>, Tick)> {
        let energy = block.iter().map(|v| v * v).sum::<f32>() / block.len().max(1) as f32;
        let db = 10.0 * (energy + 1e-12).log10();
        self.level_db = db;
        self.max_db = self.max_db.max(db);
        let loud = db > (self.floor + 12.0).max(-48.0);
        if !loud {
            // the noise floor falls quickly and rises slowly, and never above -30 dBFS
            let rate = if db < self.floor { 0.2 } else { 0.005 };
            self.floor = (self.floor + rate * (db - self.floor)).clamp(-70.0, -30.0);
        }
        if !self.speaking {
            self.pre.push_back(block.to_vec());
            while self.pre.len() > PRE_BLOCKS {
                self.pre.pop_front();
            }
            self.loud_run = if loud { self.loud_run + 1 } else { 0 };
            if self.loud_run >= 2 {
                self.speaking = true;
                self.phrase = self.pre.drain(..).flatten().collect();
                self.at = now;
                self.loud = self.loud_run;
                self.quiet = 0;
            }
            return None;
        }
        self.phrase.extend_from_slice(block);
        if loud {
            self.loud += 1;
            self.quiet = 0;
        } else {
            self.quiet += 1;
        }
        if self.quiet < END_BLOCKS && self.phrase.len() < MAX_SAMPLES {
            return None;
        }
        let enough = self.loud >= MIN_LOUD_BLOCKS;
        let phrase = std::mem::take(&mut self.phrase);
        self.speaking = false;
        self.loud_run = 0;
        self.pre.clear();
        enough.then_some((phrase, self.at))
    }
}

/// Mono, 16 kHz, by linear interpolation.
struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
}

impl Resampler {
    fn new(rate: u32) -> Self {
        Self { step: f64::from(rate.max(1)) / f64::from(RATE), pos: 0.0, last: 0.0 }
    }

    fn feed(&mut self, mono: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(mono);
            return;
        }
        let mut b = Vec::with_capacity(mono.len() + 1);
        b.push(self.last);
        b.extend_from_slice(mono);
        while self.pos + 1.0 < b.len() as f64 {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            if let (Some(a), Some(c)) = (b.get(i), b.get(i + 1)) {
                out.push(a * (1.0 - frac) + c * frac);
            }
            self.pos += self.step;
        }
        self.pos -= (b.len() - 1) as f64;
        self.last = b.last().copied().unwrap_or(0.0);
    }
}

fn downmix(planar: &[Vec<f32>]) -> Vec<f32> {
    let frames = planar.iter().map(Vec::len).min().unwrap_or(0);
    let n = planar.len().max(1) as f32;
    (0..frames).map(|i| planar.iter().map(|c| c.get(i).copied().unwrap_or(0.0)).sum::<f32>() / n).collect()
}

// ------------------------------------------------------------------------------------- worker

struct Job {
    audio: Vec<f32>,
    at: Tick,
}

enum Out {
    Loaded,
    Heard(Heard),
    Failed(String),
}

fn load_model(model: &str) -> std::result::Result<Arc<dyn Transcriber>, String> {
    let Some(info) = filmcraft_speech::models::find(model) else {
        return Err(format!("unknown speech model `{model}`"));
    };
    if !filmcraft_speech::available() {
        return Err(crate::transcript::NO_SPEECH.to_owned());
    }
    let dir = crate::transcript::models_dir_for(model).ok_or("no data directory for speech models")?;
    if !filmcraft_speech::models::installed(&dir, info) {
        return Err(format!("the speech model `{model}` is not installed: run transcript.downloadModel first"));
    }
    filmcraft_speech::load(&dir, model).map_err(|e| e.to_string())
}

fn work(given: Option<Arc<dyn Transcriber>>, model: String, language: Option<String>, jobs: Receiver<Job>, out: &Sender<Out>) {
    let loaded = match given {
        Some(t) => Ok(t),
        None => load_model(&model),
    };
    let t = match loaded {
        Ok(t) => t,
        Err(e) => {
            let _ = out.send(Out::Failed(e));
            return;
        }
    };
    let _ = out.send(Out::Loaded);
    let opts = Options { language, diarize: false, max_speakers: 1 };
    for job in jobs.iter() {
        let message = match t.transcribe(&job.audio, &opts, &mut |_, _| true) {
            Ok(tr) => Out::Heard(Heard { text: tr.text(), at: job.at }),
            Err(e) => Out::Failed(e.to_string()),
        };
        if out.send(message).is_err() {
            return;
        }
    }
}

// ------------------------------------------------------------------------------------- listener

struct Running {
    jobs: Sender<Job>,
    out: Mutex<Receiver<Out>>,
    segmenter: Segmenter,
    resampler: Resampler,
    channels_seen: usize,
    chunk: usize,
    loaded: bool,
    pending: usize,
    device: String,
    /// Push-to-talk: only what is said while the key is held counts.
    push: bool,
    talking: bool,
    releasing: bool,
    held: Vec<f32>,
    held_at: Tick,
    /// Meter: the recent peak, dBFS, falling slowly.
    level_db: f32,
    last_sound: Instant,
}

/// The listening state of a session.
#[derive(Default)]
pub struct Listener {
    run: Option<Running>,
    /// Why listening stopped by itself, until the next start.
    pub error: Option<String>,
}

impl Listener {
    pub fn active(&self) -> bool {
        self.run.is_some()
    }
    /// `off`, `loading` (the model is being loaded), `listening` or `recognising`.
    pub fn state(&self) -> &'static str {
        match &self.run {
            None => "off",
            Some(r) if !r.loaded => "loading",
            Some(r) if r.pending > 0 => "recognising",
            Some(_) => "listening",
        }
    }
    /// Level of the input (recent peak), dBFS.
    pub fn level_db(&self) -> f32 {
        self.run.as_ref().map_or(-120.0, |r| r.level_db)
    }
    /// Push-to-talk mode.
    pub fn push(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.push)
    }
    /// The talk key is held.
    pub fn talking(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.talking)
    }
    /// The input device (`default input` for the system's).
    pub fn device(&self) -> String {
        self.run.as_ref().map_or_else(String::new, |r| if r.device.is_empty() { "default input".to_owned() } else { r.device.clone() })
    }
    /// Seconds since the input last delivered anything but silence (0 until the model is loaded).
    pub fn silent_secs(&self) -> f32 {
        self.run.as_ref().filter(|r| r.loaded).map_or(0.0, |r| r.last_sound.elapsed().as_secs_f32())
    }
}

/// Start listening on the default input (`push`: push-to-talk). Already listening: only the mode changes.
pub fn start(s: &mut Session, push: bool) -> Result<()> {
    if let Some(run) = s.voice_listen.run.as_mut() {
        run.push = push;
        run.talking = false;
        run.releasing = false;
        run.held.clear();
        return Ok(());
    }
    if s.voiceover.recording() {
        return Err(bad("voice.listen", "a voice-over is being recorded: the input is in use"));
    }
    if s.transcriber.is_none() && !filmcraft_speech::available() {
        return Err(EngineError::Other(crate::transcript::NO_SPEECH.into()));
    }
    let device = s.prefs.audio_hardware.default_input.clone();
    let input = s.voiceover.input.get_or_insert_with(|| Box::new(crate::voiceover::SyntheticInput::clicks()));
    let format = input.start(&device, RATE).map_err(|e| EngineError::Other(format!("cannot open the microphone: {e}")))?;
    let ma = &s.prefs.media_analysis;
    let language = if ma.language_auto_detect { None } else { Some(ma.default_language.clone()).filter(|l| !l.is_empty()) };
    let (jobs, jobs_rx) = channel();
    let (out_tx, out) = channel();
    let (given, model) = (s.transcriber.clone(), ma.whisper_model.clone());
    std::thread::Builder::new()
        .name("voice-listen".into())
        .spawn(move || {
            if catch_unwind(AssertUnwindSafe(|| work(given, model, language, jobs_rx, &out_tx))).is_err() {
                let _ = out_tx.send(Out::Failed("speech recognition stopped unexpectedly".into()));
            }
        })
        .map_err(|e| EngineError::Other(format!("cannot start speech recognition: {e}")))?;
    s.voice_listen.error = None;
    s.voice_listen.run = Some(Running {
        jobs,
        out: Mutex::new(out),
        segmenter: Segmenter::default(),
        resampler: Resampler::new(format.sample_rate),
        channels_seen: usize::from(format.channels),
        chunk: (format.sample_rate / 20).max(1) as usize,
        loaded: false,
        pending: 0,
        device,
        push,
        talking: false,
        releasing: false,
        held: Vec::new(),
        held_at: Tick(0),
        level_db: -120.0,
        last_sound: Instant::now(),
    });
    Ok(())
}

/// Push-to-talk: the talk key goes down (`true`) or up (`false`). Ignored in `auto` mode.
pub fn set_talking(s: &mut Session, down: bool) {
    let now = s.state.source_playhead;
    let Some(run) = s.voice_listen.run.as_mut().filter(|r| r.push) else { return };
    if down && !run.talking {
        run.talking = true;
        run.releasing = false;
        run.held.clear();
        run.held_at = now;
    } else if !down && run.talking {
        // the audio still on its way from the device belongs to the phrase: finish on the next poll
        run.releasing = true;
    }
}

/// Stop listening (a phrase being recognised is dropped).
pub fn stop(s: &mut Session) {
    if s.voice_listen.run.take().is_some()
        && let Some(input) = s.voiceover.input.as_mut()
    {
        input.stop();
    }
}

fn fail(s: &mut Session, message: String) {
    stop(s);
    s.voice_listen.error = Some(message);
}

type Phrase = (Vec<f32>, Tick);

impl Running {
    /// One chunk from the input: meter it, and cut phrases (auto) or keep it for the held key (push).
    fn absorb(&mut self, planar: &[Vec<f32>], now: Tick) -> Vec<Phrase> {
        self.channels_seen = planar.len();
        let mut mono16 = Vec::new();
        self.resampler.feed(&downmix(planar), &mut mono16);
        let cut = self.segmenter.push(&mono16, now);
        self.level_db = self.segmenter.max_db.max(self.level_db - 1.0);
        if self.segmenter.max_db > -65.0 {
            self.last_sound = Instant::now();
        }
        if !self.push {
            return cut;
        }
        if self.talking && self.held.len() < MAX_SAMPLES {
            self.held.extend_from_slice(&mono16);
        }
        Vec::new()
    }

    /// The talk key was released: what was held is a phrase (a tap shorter than 0.2 s is not).
    fn release(&mut self) -> Option<Phrase> {
        if !(self.push && self.releasing) {
            return None;
        }
        self.talking = false;
        self.releasing = false;
        let held = std::mem::take(&mut self.held);
        (held.len() >= (RATE / 5) as usize).then_some((held, self.held_at))
    }

    /// Queue phrases for recognition; a message when the worker is gone.
    fn submit(&mut self, phrases: Vec<Phrase>) -> Option<String> {
        let mut problem = None;
        for (audio, at) in phrases {
            self.pending += 1;
            if self.jobs.send(Job { audio, at }).is_err() {
                problem = Some("speech recognition is not running".to_owned());
            }
        }
        problem
    }

    /// What the worker has finished: phrases heard, and a failure if there was one.
    fn collect(&mut self) -> (Vec<Heard>, Option<String>) {
        let (mut heard, mut problem) = (Vec::new(), None);
        let rx = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        while let Ok(message) = rx.try_recv() {
            match message {
                Out::Loaded => self.loaded = true,
                Out::Heard(h) => {
                    self.pending = self.pending.saturating_sub(1);
                    heard.push(h);
                }
                Out::Failed(e) => problem = Some(e),
            }
        }
        (heard, problem)
    }
}

/// Read the input, cut phrases, collect recognised ones. Call every ~50 ms while listening.
pub fn poll(s: &mut Session) -> Vec<Heard> {
    let now = s.state.source_playhead;
    let (Some(run), Some(input)) = (s.voice_listen.run.as_mut(), s.voiceover.input.as_mut()) else {
        return Vec::new();
    };
    let mut phrases = Vec::new();
    for _ in 0..MAX_READS {
        let planar = input.read(run.chunk);
        let frames = planar.iter().map(Vec::len).min().unwrap_or(0);
        if frames == 0 {
            break;
        }
        phrases.extend(run.absorb(&planar, now));
        if frames < run.chunk {
            break;
        }
    }
    phrases.extend(run.release());
    let mut problem = input.error().map(|e| format!("microphone: {e}"));
    problem = run.submit(phrases).or(problem);
    let (heard, failed) = run.collect();
    if let Some(p) = failed.or(problem) {
        fail(s, p);
    }
    heard
}

fn listen(s: &mut Session, p: &Value) -> Result<Value> {
    let push = match p.get("mode").and_then(Value::as_str) {
        None | Some("auto") => false,
        Some("push") => true,
        Some(other) => return Err(bad("voice.listen", format!("`mode` is `auto` or `push`, not `{other}`"))),
    };
    if bool_p(p, "on").unwrap_or(true) {
        start(s, push)?;
    } else {
        stop(s);
    }
    status(s, &Value::Null)
}

fn talk(s: &mut Session, p: &Value) -> Result<Value> {
    let down = bool_p(p, "down").ok_or_else(|| bad("voice.talk", "need `down`: true or false"))?;
    set_talking(s, down);
    status(s, &Value::Null)
}

fn poll_command(s: &mut Session, _: &Value) -> Result<Value> {
    let mut done = Vec::new();
    for h in poll(s) {
        let result = crate::voice_log::hear(s, &h.text, Some(h.at));
        done.push(json!({"text": h.text, "at": h.at.0, "result": result.map_err(|e| e.to_string())}));
    }
    Ok(json!({"phrases": done, "state": s.voice_listen.state(), "error": s.voice_listen.error}))
}

fn status(s: &mut Session, _: &Value) -> Result<Value> {
    let l = &s.voice_listen;
    let channels = l.run.as_ref().map_or(0, |r| r.channels_seen);
    Ok(json!({
        "listening": l.active(), "state": l.state(), "levelDb": l.level_db(), "channels": channels, "error": l.error,
        "mode": if l.push() { "push" } else { "auto" }, "talking": l.talking(), "device": l.device(), "silentSeconds": l.silent_secs(),
        "available": s.transcriber.is_some() || filmcraft_speech::available(),
    }))
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
        spec("voice.listen", "Voice: Listen", r#"{"on":bool?,"mode":"auto"|"push"?}"#, listen, false),
        spec("voice.talk", "Voice: Hold or Release the Talk Key", r#"{"down":bool}"#, talk, false),
        spec("voice.poll", "Voice: Process Heard Phrases", "{}", poll_command, true),
        spec("voice.status", "Voice: Status", "{}", status, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f32, amp: f32) -> Vec<f32> {
        (0..(secs * RATE as f32) as usize).map(|i| amp * (i as f32 * 0.17).sin()).collect()
    }

    fn quiet(secs: f32) -> Vec<f32> {
        vec![0.0; (secs * RATE as f32) as usize]
    }

    #[test]
    fn a_phrase_is_cut_at_the_pause_and_keeps_its_start() {
        let mut seg = Segmenter::default();
        let mut all = quiet(0.5);
        all.extend(tone(1.0, 0.3));
        all.extend(quiet(1.0));
        let mut phrases = Vec::new();
        for (i, chunk) in all.chunks(800).enumerate() {
            phrases.extend(seg.push(chunk, Tick(i as i64)));
        }
        assert_eq!(phrases.len(), 1, "one phrase");
        let (audio, at) = &phrases[0];
        assert!(audio.len() > RATE as usize, "the phrase has its second of sound: {}", audio.len());
        assert!((9..=11).contains(&at.0), "stamped when the sound began (chunk 10): {}", at.0);
    }

    #[test]
    fn clicks_and_silence_make_no_phrase() {
        let mut seg = Segmenter::default();
        let mut all = quiet(0.5);
        all.extend(tone(0.05, 0.5));
        all.extend(quiet(2.0));
        assert!(seg.push(&all, Tick(0)).is_empty());
        assert!(seg.push(&quiet(2.0), Tick(0)).is_empty());
    }

    #[test]
    fn a_long_phrase_is_cut_at_the_limit() {
        let mut seg = Segmenter::default();
        let phrases = seg.push(&tone(20.0, 0.3), Tick(0));
        assert_eq!(phrases.len(), 1);
        assert!(phrases[0].0.len() <= MAX_SAMPLES + BLOCK);
    }

    #[test]
    fn resampling_changes_the_length_by_the_rate_ratio() {
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        r.feed(&[0.5; 4800], &mut out);
        r.feed(&[0.5; 4800], &mut out);
        assert!((3190..=3202).contains(&out.len()), "{}", out.len());
        assert!(out.iter().skip(1).all(|v| (*v - 0.5).abs() < 1e-5));
    }

    #[test]
    fn stereo_is_averaged() {
        assert_eq!(downmix(&[vec![1.0, 0.0], vec![0.0, 0.0, 9.0]]), vec![0.5, 0.0]);
        assert!(downmix(&[]).is_empty());
    }

    fn session_hearing(words: &[&str], audio: Vec<f32>) -> Session {
        let mut t = filmcraft_project::Transcript { language: "en".into(), ..Default::default() };
        for (i, w) in words.iter().enumerate() {
            let at = Tick::from_seconds_f64(0.1 + 0.1 * i as f64);
            t.words.push(filmcraft_project::Word::new(*w, at, Tick(at.0 + Tick::from_seconds_f64(0.05).0)));
        }
        let mut s = Session::default();
        s.transcriber = Some(Arc::new(filmcraft_speech::FixedTranscriber { transcript: t, id: "fixed".into() }));
        s.voiceover.input = Some(Box::new(crate::voiceover::SyntheticInput::new(crate::voiceover::Synthetic::Buffer(vec![audio]))));
        s
    }

    fn wait_for_phrase(s: &mut Session) -> Vec<Heard> {
        for _ in 0..400 {
            let heard = poll(s);
            if !heard.is_empty() {
                return heard;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Vec::new()
    }

    #[test]
    fn a_spoken_phrase_is_recognised_with_the_playhead_it_began_at() {
        let mut audio = quiet(0.3);
        audio.extend(tone(1.0, 0.3));
        audio.extend(quiet(1.0));
        let mut s = session_hearing(&["Start"], audio);
        assert_eq!(s.voice_listen.state(), "off");
        s.execute("voice.listen", json!({})).unwrap();
        assert!(s.voice_listen.active());
        s.state.source_playhead = Tick(42);
        let heard = wait_for_phrase(&mut s);
        assert_eq!(heard.len(), 1, "state {:?}", s.voice_listen.error);
        assert_eq!(heard[0].text, "Start");
        assert_eq!(heard[0].at, Tick(42));
        s.execute("voice.listen", json!({"on": false})).unwrap();
        assert_eq!(s.voice_listen.state(), "off");
        assert!(poll(&mut s).is_empty(), "nothing is read while off");
    }

    #[test]
    fn listening_needs_a_recogniser_and_stopping_is_harmless() {
        let mut s = Session::default();
        s.voiceover.input = Some(Box::new(crate::voiceover::SyntheticInput::clicks()));
        if !filmcraft_speech::available() {
            assert!(s.execute("voice.listen", json!({})).is_err(), "no recogniser in this build");
            assert!(!s.voice_listen.active());
        }
        assert_eq!(s.execute("voice.status", json!({})).unwrap()["listening"], false);
        assert!(stop_is_harmless(&mut s));
    }

    fn stop_is_harmless(s: &mut Session) -> bool {
        stop(s);
        stop(s);
        !s.voice_listen.active()
    }

    #[test]
    fn push_to_talk_hears_only_what_is_said_while_the_key_is_held() {
        let mut audio = tone(1.0, 0.3);
        audio.extend(quiet(1.0));
        let mut s = session_hearing(&["Include"], audio);
        s.execute("voice.listen", json!({"mode": "push"})).unwrap();
        assert!(s.voice_listen.push());
        // nothing is heard before the key goes down, whatever the microphone delivers
        s.voice_listen.run.as_mut().unwrap().loaded = true;
        for _ in 0..3 {
            assert!(poll(&mut s).is_empty());
        }
        let mut audio = tone(1.0, 0.3);
        audio.extend(quiet(1.0));
        let mut s = session_hearing(&["Include"], audio);
        s.execute("voice.listen", json!({"mode": "push"})).unwrap();
        s.state.source_playhead = Tick(7);
        s.execute("voice.talk", json!({"down": true})).unwrap();
        assert!(s.voice_listen.talking());
        assert!(poll(&mut s).is_empty(), "nothing is sent while the key is down");
        s.execute("voice.talk", json!({"down": false})).unwrap();
        let heard = wait_for_phrase(&mut s);
        assert_eq!(heard.len(), 1, "{:?}", s.voice_listen.error);
        assert_eq!((heard[0].text.as_str(), heard[0].at), ("Include", Tick(7)));
        assert!(!s.voice_listen.talking());
        // a bad mode and a missing flag are refused
        assert!(s.execute("voice.listen", json!({"mode": "shout"})).is_err());
        assert!(s.execute("voice.talk", json!({})).is_err());
    }

    #[test]
    fn the_level_and_a_silent_microphone_are_reported() {
        let mut s = session_hearing(&["x"], tone(3.0, 0.3));
        s.execute("voice.listen", json!({})).unwrap();
        let _ = poll(&mut s);
        let level = s.execute("voice.status", json!({})).unwrap()["levelDb"].as_f64().unwrap();
        assert!(level > -30.0, "a loud tone reads loud: {level}");
        assert!(s.voice_listen.silent_secs() < 1.0, "sound was just heard");
    }
}
