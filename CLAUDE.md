@AGENTS.md

# FilmCraft: working instructions

FilmCraft is a Rust (edition 2024, Rust 1.95+) non-linear video editor: an egui desktop app, a headless CLI, an MCP server and a JSON-lines control channel. `AGENTS.md` (imported above) holds the rules that must never be broken; this file is orientation. If they disagree, `AGENTS.md` wins.

## Layers

Dependencies only point downward. `cargo xtask layers` enforces this, and forbids egui, eframe, winit, rfd, cpal and muda below L5.

| Layer | Crates (`crates/<name>`, package `filmcraft-<name>`) |
|---|---|
| L0 | `time`, `geom`, `color`, `bitstream`, `testkit`; codec/container crates `isobmff`, `matroska`, `cfb`, `dng` |
| L1 | `frame`, `media`, `project` (document model), `audio-dsp`, `text` |
| L2 | `edit` (pure edit algebra), `codecs`, `interchange` (EDL/XML/OTIO/AAF/ALE), `captions`, `speech` (Whisper), `tts`, `tts-text` |
| L3 | `render`, `gpu`, `export`, `scopes`, `format` (`.fcproj` load/save), `golden` |
| L4 | `engine`: `Session`, command registry, undo, jobs |
| L5 | `ui-egui` (panels, docking, control channel), `automation` (MCP), `platform` (the only crate allowed `unsafe`) |
| L6 | `apps/filmcraft` (desktop), `apps/filmcraft-cli` (CLI + MCP), `apps/filmcraft-web` |

Much codec and format code lives outside this repo in `storytold/craft-libs`, pulled in as pinned git revisions (for example `craft-timeline`, which owns `Tick`, and `craft-fcproj-format`, which owns project encode/decode/migrations). Changes there need a PR in that repo and a revision bump in the root `Cargo.toml`.

## Commands

```sh
cargo run --release -p filmcraft                       # desktop app with demo project (--empty for blank)
FILMCRAFT_CONTROL_TOKEN=<64 hex chars> cargo run --release -p filmcraft -- --control 9876
cargo build --release -p filmcraft-cli                 # needed by .mcp.json (both servers fail with ENOENT until built)
cargo run --release -p filmcraft-cli -- commands       # list all command ids
cargo test -p <crate>                                  # e.g. filmcraft-engine, filmcraft-ui-egui
cargo xtask ci                                         # fmt, clippy -D warnings, tests, layers, assets, wasm: run before every commit
cargo xtask fixtures                                   # ffmpeg-generated fixtures into target/fixtures (never commit media)
```

Never put the control token in argv, logs or checked-in files. ffmpeg is a test oracle only.

## Core concepts

- **Everything is a command.** A command is a `CommandSpec` (id, label, menu path, shortcut, params doc string, `enabled`, `run`) in `crates/engine/src/commands.rs`, registered through each module's `commands()` near the end of `build()`. `Session::execute(id, params)` runs it. UI, CLI, control channel and MCP all dispatch by id.
- **Undo is snapshot-based.** Mutate through `Session::edit(label, |project, state| ..)`; use `edit_merged` to coalesce typing and `edit_group` for several edits as one step. No inverse command is needed.
- **Time is `Tick`** (254,016,000,000 per second). Never use f64 seconds for edit math. `MediaClip.mark_out` is the start of the last kept frame (inclusive).
- **Document model** is in `crates/project`: `Project` (bins, `items: BTreeMap<ItemId, ProjectItem>`, side maps such as `transcripts`), `ProjectItem.metadata` (free-form per-item strings, `LOG_FIELDS` in `crates/engine/src/panels.rs`), `MediaClip` (`mark_in`/`mark_out`, `markers`), `Marker`. Ids come from `Project::alloc_id`; a new id-bearing collection must be counted in `Project::max_id`.
- **Project files** are `.fcproj` JSON with a schema version (`docs/project-files.md`). New `#[serde(default)]` fields load fine; add a migration step (with a fixture in `crates/format/tests/fixtures/`) when an older build would silently drop data. Never edit a shipped migration.
- **UI**: panels are `PanelKind` variants (`crates/ui-egui/src/dock.rs`) drawn by `panels/<name>.rs` and dispatched in `crates/ui-egui/src/panels.rs`. Every interactive widget registers an automation id (`app.auto.add*`). Per-panel view state is serde and lives in `PanelsState` (`panels/panel_state.rs`) or `UiState` (`state.rs`). Good templates: `panels/notes.rs` (smallest), `panels/metadata.rs` (cell commit on `lost_focus`), `panels/markers_panel.rs` (list, selection, bulk actions).
- **Source monitor**: `EditorState.source_item` / `source_playhead` in `crates/engine/src/lib.rs`; `clip_ops::source_view` reads marks; `project.setMarks`, `source.open`, `source.setPlayhead` are the commands.
- **Speech/voice today**: `crates/speech` (Whisper on candle, VAD, `Transcriber` trait; cargo feature `whisper`, off by default), `crates/engine/src/voiceover.rs` and `apps/filmcraft/src/audio_in.rs` (cpal mic capture), `docs/transcripts.md`. Voice control of the Clip Log (off until the Voice button or `voice.listen`) is `crates/engine/src/voice_listen.rs` (microphone → phrases → Whisper worker) and `voice_log.rs` (phrase → `log.*` commands; `voice.logHear {text}` drives it without a microphone).

## How to add things

- **Command**: new `crates/engine/src/<feature>.rs` with `pub(crate) fn commands() -> Vec<CommandSpec>`; add `pub mod`/`mod` in `lib.rs` and `v.extend(crate::<feature>::commands())` in `commands.rs`. Copy `metadata.set` (`panels.rs`) for per-item edits and `markers.edit` (`marker_edit.rs`) for bulk, bounded, atomic ones. Declare `"item":id` / `"items":[id]` in the params string if `run` reads them.
- **Panel**: add the variant at the end of `PanelKind`, `ALL` (update the array length) and `title()`; add `pub mod` and a dispatch arm in `panels.rs`; handle placement in `app/window.rs`; add it to `every_panel_opens_from_the_window_menu` in `crates/ui-egui/tests/it/panels_ui.rs`. Append only: saved workspaces serialize variant names.
- **Project field**: `#[serde(default, skip_serializing_if = ..)]` on `Project`, initialise in `Project::new`, count in `max_id`, then the schema/migration steps above.

## Tests and gotchas

- Engine tests are `crates/engine/src/*_tests.rs` (`Session::default()`, `file.openDemoProject`, `s.execute(..)`); UI tests use the kittest `Driver` in `crates/ui-egui/tests/it/`.
- Every new command needs hostile-parameter tests; crash fixes need a test that panicked before. Check undo/redo, encode/decode round trip and loading of a legacy file.
- `commands_are_unique_and_described` and the named-targets test fail on duplicate ids or undeclared param keys.
- A focused egui text field owns unmodified keys (`crates/ui-egui/src/app/keys.rs`); with no field focused, typed or dictated letters trigger single-key shortcuts.
- Production crate roots carry the `deny(clippy::unwrap_used, expect_used, panic, ..)` attribute (copy from `crates/project/src/lib.rs`); new crates set `[lints] workspace = true`.
- Commit subjects start with a task id (`M3.2: ...`); commit only green states; never push to `main`.

## Docs

`docs/architecture.md`, `docs/contributing.md`, `docs/testing.md`, `docs/agents.md` (MCP, CLI, work loop), `docs/control-protocol.md`, `docs/project-files.md`, `docs/keyboard.md`, `docs/transcripts.md`, `ROADMAP.md`, `docs/gaps.md`.
