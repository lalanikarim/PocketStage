//! PocketStage HTTP control plane (scratchpad R5: "browser is a remote
//! control, never an audio path").
//!
//! Slice 2: the router runs *inside* the engine process ([`spawn_engine_server`])
//! and talks to the audio thread exclusively through [`EngineCommand`]s on an
//! mpsc channel — the single-writer rule holds: the audio thread still owns
//! all mutable state; the server only ever *requests* changes.
//!
//! Route surface (authoritative list = `router()` below; the running set is
//! also visible in the web UI):
//! - `GET  /api/health`  — liveness + the model's hard limits (`max_tracks`,
//!   `max_scenes`) so the UI never mirrors a constant (D31)
//! - `GET  /api/project` — current project as a versioned [`ProjectFile`] envelope
//! - `PUT  /api/project` — replace the whole project: parsed through the model's
//!   version gate, validated, persisted atomically (tmp+rename), then handed to
//!   the engine. Bad payloads are rejected with the reason and leave disk,
//!   memory, and the live engine untouched.
//! - `GET  /ws`          — read-only [`LiveStatus`] push frames
//! - `POST /api/transport/{play,stop,record}`, `/api/scene/{n}`
//!   (→ `?bar=N` pending), `/api/voice`, `/api/overlay/{mix|seq|none}`,
//!   `/api/takes/rename`
//! - `GET  /api/presets` — preset catalog read from the LOADED soundfont
//!   (labels must never be hand-typed)
//! - `GET  /api/patches` — full multi-font patch catalog (D27 phase B)
//! - `GET  /api/projects`, `POST /api/projects/{save_as,load,delete}`,
//!   `GET /api/projects/raw` — project library
//! - `GET/PUT /api/keymap` + `POST /api/keymap/reset` — D26 keymap file
//! - `GET/PUT /api/mapping` + `POST /api/mapping/reset` — D29/D37 device
//!   mapping file (measured ground truth, editable with loud warnings)

use axum::extract::Path as PathParam;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pocketstage_model::{ActiveVoice, Project, ProjectFile, SCHEMA_VERSION, Take};
// Re-exported so embedders (engine) and handlers share one DawKeymap type;
// the schema stamp is logged at boot next to the keymap path.
pub use pocketstage_oxygen::{DawKeymap, KEYMAP_SCHEMA, Mapping, MappingPair};

// ----------------------------------------------------------------- commands

/// Requests the server may make of the engine (audio thread). One-directional
/// like everything else crossing into the audio domain — the engine applies
/// these at its own safe points and never replies synchronously.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineCommand {
    /// Swap the live project. Payload was validated by [`ProjectStore::replace`]
    /// before it got here.
    SetProject(ProjectFile),
    /// Start playback (master clock from zero) — same semantics as the ⏵ key.
    Play,
    /// Stop: finalizes a live take, halts playback — identical to hardware ⏹.
    Stop,
    /// ⏺ start capturing a take (metronome on while recording, D16).
    Record,
    /// Launch scene `n` (1-based as users count; engine maps to its 0-based).
    LaunchScene(u8),
    /// D25: set the single active performance voice (web picker) — same
    /// semantics as the DAW white-key browser's `Event::SelectVoice`.
    SelectVoice(ActiveVoice),
    /// Rename a live take so the audio thread's copy matches the store
    /// (take adoption after later swaps keys on names — renaming only the
    /// store side would duplicate the take on the next swap).
    RenameTake { from: String, to: String },
    /// Drop a take by name on the audio thread as well. Pairs with the web
    /// delete endpoint: the store has already forgotten it, but without this the
    /// engine keeps looping what the user just deleted until the next swap —
    /// and a `mine` entry left behind would resurrect it through the swap
    /// keep-rule.
    DeleteTake { name: String },
    /// Addition B (D26, keymap phase 2): force the overlay to an exact
    /// state. Anti-brick control — a bad keymap must never trap the user,
    /// so a web-side exit/entry that bypasses keymap bindings exists.
    SetOverlay(OverlayTarget),
}

/// Exact overlay target for [`EngineCommand::SetOverlay`] (`None` = exit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayTarget {
    Mix,
    Seq,
    None,
}

// ------------------------------------------------------------------ store

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Model(pocketstage_model::ModelError),
    #[error("{0}")]
    Validation(pocketstage_model::ValidationError),
    #[error("io: {0}")]
    Io(String),
}

/// Thread-safe project holder with atomic-ish disk persistence. The engine's
/// in-memory `Project` converges to this via [`EngineCommand::SetProject`].
pub struct ProjectStore {
    /// `Some` = library mode: sibling projects live in this directory and
    /// `path` names the current one (switchable at runtime). `None` =
    /// single-file mode (explicit POCKETSTAGE_PROJECT path).
    dir: Option<PathBuf>,
    path: Mutex<PathBuf>,
    project: Mutex<ProjectFile>,
}

impl ProjectStore {
    /// Load `path`; when it doesn't exist yet, start from
    /// [`Project::default_project()`] (not written to disk until first PUT).
    /// A project read FROM DISK is migrated (legacy values clamped to what the
    /// engine can actually apply) and then validated, so an old file keeps
    /// booting — see [`pocketstage_model::Project::normalize_legacy_values`].
    ///
    /// The HTTP write path deliberately does NOT do this: a PUT carrying an
    /// out-of-range value must be refused with a message (D33), not quietly
    /// rewritten into something the client never sent.
    fn migrate_and_validate(pf: ProjectFile) -> Result<ProjectFile, StoreError> {
        let mut pf = pf;
        pf.project.normalize_legacy_values();
        pf.validate().map_err(StoreError::Validation)?;
        Ok(pf)
    }

    pub fn open_or_default(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let project = match std::fs::read_to_string(&path) {
            Ok(text) => Self::migrate_and_validate(
                ProjectFile::from_json_str(&text).map_err(StoreError::Model)?,
            )?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProjectFile {
                version: SCHEMA_VERSION,
                project: Project::default_project(),
            },
            Err(e) => return Err(StoreError::Io(e.to_string())),
        };
        Ok(Self {
            dir: None,
            path: Mutex::new(path),
            project: Mutex::new(project),
        })
    }

    /// Library mode: projects live in `dir` as `<name>.json`. Loads the most
    /// recently modified one; seeds a "Default" project when empty. A legacy
    /// single file next to the dir is copied in on first use (migration).
    pub fn open_library(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
        // Sweep save-as temp files orphaned by a crash between write and
        // rename (review P2-6) — best-effort, never fatal.
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(".ps-tmp-"))
                {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        let mut names: Vec<(std::time::SystemTime, String)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json")
                    // hidden files are never projects (see sweep above)
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    && !stem.starts_with('.')
                {
                    let mt = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    names.push((mt, stem.to_string()));
                }
            }
        }
        names.sort_by_key(|(when, _)| *when);
        let current = match names.into_iter().next_back() {
            Some((_, name)) => dir.join(format!("{name}.json")),
            None => {
                // Empty library: seed Default from the legacy file if present.
                let target = dir.join("Default.json");
                let legacy = dir
                    .parent()
                    .map(|p| p.join("project.json"))
                    .filter(|l| l.exists());
                if let Some(legacy) = legacy {
                    std::fs::copy(&legacy, &target).map_err(|e| StoreError::Io(e.to_string()))?;
                }
                target
            }
        };
        let store = Self::open_or_default(&current)?;
        // A seeded (fresh-default) entry must exist on disk so the library
        // listing and save-as targets behave uniformly.
        if !current.exists() {
            store.replace(store.get())?;
        }
        Ok(Self {
            dir: Some(dir),
            path: Mutex::new(current),
            project: store.project,
        })
    }

    /// Validate `name` for library operations (no paths, no dots-only,
    /// filesystem-safe, max 64 chars).
    fn check_name(name: &str) -> Result<(), StoreError> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'));
        if ok {
            Ok(())
        } else {
            Err(StoreError::Io(format!("invalid project name '{name}'")))
        }
    }

    /// Names of all projects in the library (empty in single-file mode).
    pub fn list(&self) -> Vec<String> {
        let Some(dir) = &self.dir else { return vec![] };
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension().and_then(|x| x.to_str()) != Some("json") {
                            return None;
                        }
                        let stem = p.file_stem()?.to_str()?;
                        // Hidden entries (".ps-tmp-*" leftovers) are never projects.
                        if stem.starts_with('.') {
                            return None;
                        }
                        Some(stem.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Current project's name (file stem).
    pub fn current_name(&self) -> String {
        self.path
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }

    pub fn is_library(&self) -> bool {
        self.dir.is_some()
    }

    /// Snapshot the current project into the library as `<name>.json`
    /// (does not switch). Overwrites an existing project of that name.
    pub fn save_as(&self, name: &str) -> Result<(), StoreError> {
        Self::check_name(name)?;
        let dir = self
            .dir
            .as_ref()
            .ok_or(StoreError::Io("not a library".into()))?;
        let snapshot = self.get();
        let json =
            serde_json::to_string_pretty(&snapshot).map_err(|e| StoreError::Io(e.to_string()))?;
        let target = dir.join(format!("{name}.json"));
        let tmp = dir.join(format!(".ps-tmp-{name}.json"));
        std::fs::write(&tmp, json).map_err(|e| StoreError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &target).map_err(|e| StoreError::Io(e.to_string()))?;
        Ok(())
    }

    /// Switch the current project to library entry `<name>`.
    /// Returns the loaded payload so the caller can forward it to the engine.
    pub fn load(&self, name: &str) -> Result<ProjectFile, StoreError> {
        Self::check_name(name)?;
        let dir = self
            .dir
            .as_ref()
            .ok_or(StoreError::Io("not a library".into()))?;
        let path = dir.join(format!("{name}.json"));
        let text = std::fs::read_to_string(&path).map_err(|e| StoreError::Io(e.to_string()))?;
        let pf = Self::migrate_and_validate(
            ProjectFile::from_json_str(&text).map_err(StoreError::Model)?,
        )?;
        *self.path.lock().unwrap_or_else(|p| p.into_inner()) = path;
        *self.project.lock().unwrap_or_else(|p| p.into_inner()) = pf.clone();
        Ok(pf)
    }

    /// Delete a library entry; the current project cannot be deleted.
    pub fn delete(&self, name: &str) -> Result<(), StoreError> {
        Self::check_name(name)?;
        let dir = self
            .dir
            .as_ref()
            .ok_or(StoreError::Io("not a library".into()))?;
        if self.current_name() == name {
            return Err(StoreError::Io("cannot delete the current project".into()));
        }
        std::fs::remove_file(dir.join(format!("{name}.json")))
            .map_err(|e| StoreError::Io(e.to_string()))
    }

    /// Raw JSON of a library entry (for export download).
    pub fn raw(&self, name: &str) -> Result<String, StoreError> {
        Self::check_name(name)?;
        let dir = self
            .dir
            .as_ref()
            .ok_or(StoreError::Io("not a library".into()))?;
        std::fs::read_to_string(dir.join(format!("{name}.json")))
            .map_err(|e| StoreError::Io(e.to_string()))
    }

    /// True when the file already existed (vs. a fresh default project).
    pub fn file_exists(&self) -> bool {
        self.path.lock().unwrap_or_else(|p| p.into_inner()).exists()
    }

    pub fn get(&self) -> ProjectFile {
        // Poisoned mutex = a handler panicked mid-update; the last consistent
        // value is still inside — recover rather than take the server down.
        self.project
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Validate → persist (tmp+rename) → swap memory. Any failure leaves both
    /// untouched.
    pub fn replace(&self, incoming: ProjectFile) -> Result<(), StoreError> {
        incoming.validate().map_err(StoreError::Validation)?;
        let json =
            serde_json::to_string_pretty(&incoming).map_err(|e| StoreError::Io(e.to_string()))?;

        // Hold the path mutex across write+rename (review P1-3): PUT
        // handlers, drain_takes and drain_edits run concurrently by design,
        // and two writers sharing one X.json.tmp can rename a torn file
        // over the real project. Serializing replaces is cheap — they are
        // rare and this is the durability path.
        //
        // The memory swap stays INSIDE that lock too: releasing it first and
        // assigning after lets an interleaving lose the winner (T1 renames A,
        // T2 renames B and stores B, T1 then stores A -> GET serves a document
        // disk does not have, and the next restart reverts it). Harmless while
        // PUTs were one-per-Save-click; routine bursts from web mixer
        // auto-save make the window reachable. `project` is a leaf lock, so
        // taking it under `path` cannot deadlock.
        let path = self.path.lock().unwrap_or_else(|p| p.into_inner());
        let result = (|| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, &json).map_err(|e| StoreError::Io(e.to_string()))?;
            std::fs::rename(&tmp, &*path).map_err(|e| StoreError::Io(e.to_string()))
        })();
        if result.is_ok() {
            *self.project.lock().unwrap_or_else(|p| p.into_inner()) = incoming;
        }
        drop(path);
        result
    }

    pub fn path(&self) -> PathBuf {
        self.path.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

// -------------------------------------------------------------- live status

/// Engine *levels* shared with the HTTP/WS layer: written by the audio thread
/// (plain atomic stores — no locks, no allocation on the audio path), read by
/// the WS pusher. Transport state is a level, not an event stream: readers
/// always see "now", never miss or replay a transition.
#[derive(Debug, Default)]
pub struct LiveStatus {
    pub playing: AtomicBool,
    pub recording: AtomicBool,
    /// Playhead in beats × 1000 (updated every audio cycle while playing).
    pub pos_beats_x1000: AtomicU32,
    /// Active scene, stored 1-based; 0 = none.
    pub active_scene: AtomicU8,
    /// Finalized take count this boot.
    pub takes: AtomicU8,
    /// Unit front-panel mode tracked from DAW/PRESET button pulses (D11/T6).
    pub daw_mode: AtomicBool,
    /// Software overlay inside PRESET (D21): 0 = none, 1 = MIX, 2 = SEQ.
    /// Written by the audio thread on pad 9/10 toggles; read by WS/stats.
    pub overlay: AtomicU8,
    /// D25 single active voice: GM program 0..=127, [`VOICE_DRUMS`], or
    /// [`VOICE_PATCH`] (a catalog patch — details live in the project).
    /// Written by the audio thread (boot / swap / SelectVoice); read by the
    /// MIDI feeder for routing and by WS for display.
    pub active_voice: AtomicU32,
    /// D27 phase C fallback badges: human-readable notes from the most
    /// recent patch-resolution pass ("'X' 8:33 not loaded — using …").
    /// Written ONLY at user-paced points (boot, project swap, voice select,
    /// scene launch) — never in the render cycle itself; read by the WS
    /// pusher each tick. Capped by the writer.
    pub patch_notes: Mutex<Vec<String>>,
}

/// Sentinel value of [`LiveStatus::active_voice`] meaning "the kit".
/// Real programs are 0..=127, so this can never alias.
pub const VOICE_DRUMS: u32 = u32::MAX;

/// Sentinel of [`LiveStatus::active_voice`] meaning "a catalog patch"
/// (D27 phase C). The feeder treats anything non-`VOICE_DRUMS` as keys→
/// lead, so this flows through its routing untouched; only the display
/// distinguishes it. The concrete `{sfont,bank,program}` lives in the
/// project's `active_voice`, which is the single source of truth.
pub const VOICE_PATCH: u32 = u32::MAX - 1;

/// One preset of a loaded soundfont, as served by `GET /api/presets`.
/// Names come from the font itself — the web dropdown must never ship a
/// hardcoded instrument table (session-13: a hand-typed GM list was wrong
/// for most indices and mislabelled every pick).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PresetInfo {
    pub bank: i32,
    pub program: i32,
    pub name: String,
}

// ----------------------------------------------------------------- routes

/// Shared handler state: the store, live engine levels, and — when embedded —
/// the command lane out to / feed in from the audio thread.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<ProjectStore>,
    /// `None` = standalone (file CRUD only, no live engine to notify).
    pub engine: Option<Sender<EngineCommand>>,
    pub live: Arc<LiveStatus>,
    /// Preset catalog of the soundfont(s) the engine loaded — built once at
    /// boot on the main thread (FluidSynth iteration is not thread-safe),
    /// immutable afterwards. Served by `GET /api/presets` so the web UI's
    /// voice dropdown is labelled by the FONT, not a hardcoded table.
    pub presets: Arc<Vec<PresetInfo>>,
    /// Full patch catalog across ALL loaded soundfonts (D27 phase B): built
    /// at boot on the main thread from a FluidSynth FFI walk, immutable
    /// afterwards (phase E replaces the whole Arc on rescan). Served by
    /// `GET /api/patches`; names are font-provided, never hand-typed.
    pub patches: Arc<pocketstage_model::Catalog>,
    /// Finalized takes streamed out of the audio thread (engine → server).
    /// Drained by [`AppState::drain_takes`] on a timer; each one is appended
    /// to the stored project, so recordings survive reboot.
    take_feed: Option<Arc<Mutex<Receiver<Take>>>>,
    /// Engine-originated project snapshots (D24 write-through lane): MIX/
    /// SEQ edits mutate the engine's live project first, then a debounced
    /// full snapshot flows OUT here for persistence — nothing ever travels
    /// back in, so the single-writer rule (R5) holds. Drained by
    /// [`AppState::drain_edits`]; races with a web PUT resolve
    /// last-writer-wins on the store (accepted in R12 for mixer values).
    edit_feed: Option<Arc<Mutex<Receiver<ProjectFile>>>>,
    /// DAW keymap holder (D26 phase 2): `Some` when embedded — the HTTP
    /// layer validates + persists into it, the engine's MIDI feeder reads
    /// it. Never touched by the audio thread.
    keymap: Option<Arc<KeymapSlot>>,
    /// Device-layer control maps (music + HUI sections, D29 phase 5). The
    /// slot is shared with the engine's feeder: HTTP validates + persists +
    /// publishes, the feeder hot-reloads by Arc pointer. `None` in
    /// standalone mode (no feeder to reconfigure).
    mapping: Option<Arc<MappingSlot>>,
}

impl AppState {
    /// Standalone state (no engine channels).
    pub fn standalone(store: Arc<ProjectStore>) -> Self {
        Self {
            store,
            engine: None,
            live: Arc::new(LiveStatus::default()),
            presets: Default::default(),
            patches: Default::default(),
            take_feed: None,
            edit_feed: None,
            keymap: None,
            mapping: None,
        }
    }

    /// Attach the shared DAW keymap holder (engine calls this once at boot,
    /// before the server thread starts).
    pub fn with_keymap(mut self, slot: Arc<KeymapSlot>) -> Self {
        self.keymap = Some(slot);
        self
    }

    /// Attach the shared device-mapping slot (engine calls this once at
    /// boot; the same Arc is handed to the MIDI feeder).
    pub fn with_mapping_slot(mut self, slot: Arc<MappingSlot>) -> Self {
        self.mapping = Some(slot);
        self
    }

    /// Attach the boot-time preset catalog (engine calls this once, before
    /// the server thread starts).
    pub fn with_presets(mut self, presets: Vec<PresetInfo>) -> Self {
        self.presets = Arc::new(presets);
        self
    }

    /// Attach the full multi-font patch catalog (D27; engine calls this
    /// once at boot after loading every soundfont, sharing the SAME Arc the
    /// engine core resolves against).
    pub fn with_patches(mut self, catalog: Arc<pocketstage_model::Catalog>) -> Self {
        self.patches = catalog;
        self
    }

    /// Embedded state: full duplex with the audio thread (commands out on
    /// `engine`, finalized takes in on `take_feed`, engine-side project edits
    /// in on `edit_feed`; `live` levels are written by the audio thread and
    /// pushed to WS clients).
    pub fn embedded(
        store: Arc<ProjectStore>,
        engine: Sender<EngineCommand>,
        live: Arc<LiveStatus>,
        take_feed: Receiver<Take>,
        edit_feed: Receiver<ProjectFile>,
    ) -> Self {
        Self {
            store,
            engine: Some(engine),
            live,
            presets: Default::default(),
            patches: Default::default(),
            take_feed: Some(Arc::new(Mutex::new(take_feed))),
            edit_feed: Some(Arc::new(Mutex::new(edit_feed))),
            keymap: None,
            mapping: None,
        }
    }

    /// Pull finalized takes from the audio thread and append them to the
    /// stored project (one file rewrite per take — rare by construction).
    /// Returns how many were consumed.
    pub fn drain_takes(&self) -> usize {
        let Some(rx_slot) = &self.take_feed else {
            return 0;
        };
        let mut n = 0;
        // Lock is only held around non-blocking try_recv pulls.
        if let Ok(rx) = rx_slot.lock() {
            while let Ok(take) = rx.try_recv() {
                let mut pf = self.store.get();
                pf.project.takes.push(take);
                if let Err(e) = self.store.replace(pf) {
                    eprintln!("[server] take save failed: {e}");
                }
                n += 1;
            }
        }
        n
    }

    /// Pull engine-originated snapshots from the write-through lane (D24)
    /// and persist them. Snapshots are whole-project and supersede each
    /// other, so only the LAST one drained is written — a fader ride that
    /// produced several debounced batches costs one file write. Returns how
    /// many snapshots were consumed.
    pub fn drain_edits(&self) -> usize {
        let Some(rx_slot) = &self.edit_feed else {
            return 0;
        };
        let mut latest: Option<ProjectFile> = None;
        let mut n = 0;
        if let Ok(rx) = rx_slot.lock() {
            while let Ok(pf) = rx.try_recv() {
                latest = Some(pf);
                n += 1;
            }
        }
        if let Some(pf) = latest
            && let Err(e) = self.store.replace(pf)
        {
            eprintln!("[server] write-through persist failed: {e}");
        }
        n
    }

    /// Snapshot for the WS tick / tests.
    pub fn status(&self) -> (Status, Vec<String>) {
        let pf = self.store.get();
        let live = &*self.live;
        let beats = live.pos_beats_x1000.load(Ordering::Relaxed) as f64 / 1000.0;
        (
            Status {
                bpm: pf.project.bpm,
                transport: if live.recording.load(Ordering::Relaxed) {
                    "recording"
                } else if live.playing.load(Ordering::Relaxed) {
                    "playing"
                } else {
                    "stopped"
                },
                bar: (beats / 4.0).floor() as u32 + 1,
                beat: (beats % 4.0).floor() as u32 + 1,
                scene: match live.active_scene.load(Ordering::Relaxed) {
                    0 => None,
                    n => Some(n),
                },
                mode: if live.daw_mode.load(Ordering::Relaxed) {
                    "daw"
                } else {
                    "preset"
                },
                overlay: match live.overlay.load(Ordering::Relaxed) {
                    1 => "mix",
                    2 => "seq",
                    _ => "none",
                },
            },
            pf.project.takes.iter().map(|t| t.name.clone()).collect(),
        )
    }
}

#[derive(serde::Serialize, Debug, PartialEq)]
pub struct Status {
    pub bpm: f64,
    pub transport: &'static str,
    pub bar: u32,
    pub beat: u32,
    pub scene: Option<u8>,
    /// "preset" | "daw" (D11).
    pub mode: &'static str,
    /// "none" | "mix" | "seq" — software overlay inside PRESET (D21).
    pub overlay: &'static str,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/project", get(get_project).put(put_project))
        .route("/ws", get(ws_upgrade))
        .route("/api/transport/play", post(play))
        .route("/api/transport/stop", post(stop))
        .route("/api/transport/record", post(record))
        .route("/api/scene/{n}", post(launch_scene))
        .route("/api/voice", post(set_voice))
        .route("/api/presets", get(list_presets))
        .route("/api/patches", get(list_patches))
        // Browsers that skip the <link> tag still probe /favicon.ico.
        .route("/favicon.ico", get(favicon))
        .route("/api/projects", get(list_projects))
        .route("/api/projects/save_as", post(save_as_project))
        .route("/api/projects/load", post(load_project))
        .route("/api/projects/delete", post(delete_project))
        .route("/api/projects/raw", get(raw_project))
        .route("/api/takes/rename", post(rename_take))
        .route("/api/takes/delete", post(delete_take))
        .route("/api/keymap", get(get_keymap).put(put_keymap))
        .route("/api/keymap/reset", post(reset_keymap))
        .route("/api/mapping", get(get_mapping).put(put_mapping))
        .route("/api/mapping/reset", post(reset_mapping))
        .route("/api/overlay/{target}", post(set_overlay))
        .with_state(state)
}

/// Run the server on `addr` in a dedicated thread with its own tokio runtime
/// (the engine stays async-free). Returns once the socket is bound; bind
/// errors surface to the caller, so a port clash can't take audio down.
pub fn spawn_engine_server(state: AppState, addr: std::net::SocketAddr) -> Result<(), String> {
    let (hx, hr) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("ps-http".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("tokio runtime for ps-http");
            rt.block_on(async move {
                // Periodic take-feed drain (audio thread → project file).
                if state.take_feed.is_some() {
                    let s2 = state.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            s2.drain_takes();
                        }
                    });
                }
                // Periodic write-through drain (D24): the engine already
                // debounced (~500 ms of quiet per snapshot); this tick only
                // bounds how long a ride lags before hitting disk.
                if state.edit_feed.is_some() {
                    let s3 = state.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            s3.drain_edits();
                        }
                    });
                }
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => {
                        let _ = hx.send(Ok(()));
                        if let Err(e) = axum::serve(listener, router(state)).await {
                            eprintln!("[server] serve error: {e}");
                        }
                    }
                    Err(e) => {
                        let _ = hx.send(Err(e.to_string()));
                    }
                }
            });
        })
        .map_err(|e| e.to_string())?;
    hr.recv().map_err(|e| e.to_string())?
}

/// Minimal control-plane UI — single file, embedded at compile time, no
/// asset pipeline (vanilla JS; talks to /api/project + /ws like any client).
const INDEX_HTML: &str = include_str!("../web/index.html");

async fn index() -> axum::response::Html<&'static str> {
    axum::response::Html(INDEX_HTML)
}

/// The official brand icon from `assets/brand/icons/` — single source of
/// truth, embedded at compile time (same zero-asset-pipeline policy as
/// index.html). Do not hand-draw UI icons; the brand kit owns them.
const FAVICON_ICO: &[u8] = include_bytes!("../../../assets/brand/icons/favicon.ico");

async fn favicon() -> Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/x-icon"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        FAVICON_ICO,
    )
        .into_response()
}

/// Liveness + the model's hard limits. The UI reads `max_tracks` instead of
/// hardcoding a cap (D31 raised it 4 → 8; a second copy of that number in JS
/// is exactly how stale-limit bugs happen). Server-side validation stays the
/// authority — this is UX only.
async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let name = state.store.get().project.name;
    Json(serde_json::json!({
        "ok": true,
        "project": name,
        "max_tracks": pocketstage_model::MAX_TRACKS,
        "max_scenes": pocketstage_model::MAX_SCENES,
    }))
}

async fn get_project(State(state): State<AppState>) -> Json<ProjectFile> {
    Json(state.store.get())
}

const WS_TICK: Duration = Duration::from_millis(250);

async fn ws_upgrade(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| ws_pump(socket, state))
}

/// One client: push a status frame every tick until it disconnects.
async fn ws_pump(mut socket: WebSocket, state: AppState) {
    let mut tick = tokio::time::interval(WS_TICK);
    loop {
        let (status, takes) = state.status();
        let voice = state.live.active_voice.load(Ordering::Relaxed);
        let frame = serde_json::json!({
            "type": "status",
            "bpm": status.bpm,
            "transport": status.transport,
            "bar": status.bar,
            "beat": status.beat,
            "scene": status.scene,
            "mode": status.mode,
            "overlay": status.overlay,
            // D25: current performance voice — GM program number, "drums",
            // or "patch" (D27 catalog ref; exact patch lives in the project).
            "voice": if voice == VOICE_DRUMS {
                serde_json::json!("drums")
            } else if voice == VOICE_PATCH {
                serde_json::json!("patch")
            } else {
                serde_json::json!(voice)
            },
            // D27: visible patch-fallback badges (empty = all resolved).
            "patch_notes": state
                .live
                .patch_notes
                .lock()
                .map(|n| n.clone())
                .unwrap_or_default(),
            "takes": takes,
        });
        let text = match serde_json::to_string(&frame) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[server] ws serialize failed: {e}");
                break;
            }
        };
        if socket.send(Message::Text(text.into())).await.is_err() {
            break; // client gone
        }
        // Wait for the next tick, but notice disconnects promptly.
        tokio::select! {
            _ = tick.tick() => {}
            msg = socket.recv() => {
                match msg {
                    None | Some(Err(_)) => break,
                    Some(Ok(Message::Close(_))) => break,
                    _ => {} // ignore pings/text from the client
                }
            }
        }
    }
}

// Remote transport / scene triggers: the web page drives the same event
// path the Oxygen keys drive. One-way lane; engine applies at its safe point.

async fn play(State(state): State<AppState>) -> Result<StatusCode, (StatusCode, String)> {
    send_ctrl(&state, EngineCommand::Play)
}
async fn stop(State(state): State<AppState>) -> Result<StatusCode, (StatusCode, String)> {
    send_ctrl(&state, EngineCommand::Stop)
}
async fn record(State(state): State<AppState>) -> Result<StatusCode, (StatusCode, String)> {
    send_ctrl(&state, EngineCommand::Record)
}
async fn launch_scene(
    State(state): State<AppState>,
    PathParam(n): PathParam<u8>,
) -> Result<StatusCode, (StatusCode, String)> {
    if !(1..=8).contains(&n) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("scene {n} out of range 1..8"),
        ));
    }
    send_ctrl(&state, EngineCommand::LaunchScene(n))
}
/// Body is the model's `ActiveVoice` serde form: `"drums"` or
/// `{"program": 42}` — no new schema, and it matches what a project PUT
/// carries in `active_voice`.
/// Boot-time catalog of the engine's soundfont (may be empty in standalone
/// mode — the UI then falls back to bare `gm N` labels).
async fn list_presets(State(state): State<AppState>) -> Json<Vec<PresetInfo>> {
    Json((*state.presets).clone())
}

/// Full multi-font catalog (D27 phase B): fonts (incl. failed/skipped loads
/// with their reasons) + every patch `{sfont,bank,program,name}`. Pure
/// read of boot-time data — no engine round-trip.
async fn list_patches(State(state): State<AppState>) -> Json<pocketstage_model::Catalog> {
    Json((*state.patches).clone())
}
async fn set_voice(
    State(state): State<AppState>,
    Json(voice): Json<ActiveVoice>,
) -> Result<StatusCode, (StatusCode, String)> {
    // u8 deserializes up to 255; GM programs top out at 127. The store's
    // validation never sees this body, so gate it here.
    if matches!(voice, ActiveVoice::Program(p) if p > 127) {
        return Err((StatusCode::BAD_REQUEST, "program must be 0..=127".into()));
    }
    // Same range gate for catalog refs (the model validates on load; this
    // body bypasses the store, so mirror it here).
    if let ActiveVoice::Preset(p) = &voice
        && (p.bank > 128 || p.program > 127)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "patch bank must be 0..=128, program 0..=127".into(),
        ));
    }
    send_ctrl(&state, EngineCommand::SelectVoice(voice))
}

fn send_ctrl(state: &AppState, cmd: EngineCommand) -> Result<StatusCode, (StatusCode, String)> {
    let tx = state
        .engine
        .as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "no live engine".into()))?;
    tx.send(cmd)
        .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "engine lane closed".into()))?;
    Ok(StatusCode::ACCEPTED)
}

// ---------------------------------------------------------- project library

#[derive(Debug, serde::Deserialize)]
struct NameBody {
    name: String,
}

async fn list_projects(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "projects": state.store.list(),
        "current": state.store.current_name(),
        "library": state.store.is_library(),
    }))
}

async fn save_as_project(
    State(state): State<AppState>,
    Json(body): Json<NameBody>,
) -> Result<StatusCode, StoreError> {
    state.store.save_as(&body.name)?;
    Ok(StatusCode::CREATED)
}

async fn load_project(
    State(state): State<AppState>,
    Json(body): Json<NameBody>,
) -> Result<StatusCode, StoreError> {
    let pf = state.store.load(&body.name)?;
    if let Some(tx) = &state.engine {
        tx.send(EngineCommand::SetProject(pf))
            .map_err(|_| StoreError::Io("engine channel closed".into()))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_project(
    State(state): State<AppState>,
    Json(body): Json<NameBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    state.store.delete(&body.name).map_err(|e| {
        let m = e.to_string();
        if m.contains("current project") {
            (StatusCode::CONFLICT, m)
        } else {
            (StatusCode::BAD_REQUEST, m)
        }
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn raw_project(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Response, StoreError> {
    let name = q
        .get("name")
        .cloned()
        .ok_or_else(|| StoreError::Io("missing ?name".into()))?;
    let text = state.store.raw(&name)?;
    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}.json\"").as_str(),
            ),
        ],
        text,
    )
        .into_response())
}

// ------------------------------------------------------------- take rename

#[derive(Debug, serde::Deserialize)]
struct DeleteBody {
    name: String,
}

/// Delete a recorded take by name. Store first (durable), then the engine's
/// live copy — same order as [`rename_take`].
///
/// This endpoint is the reason takes are not deleted by *omission from a PUT*
/// body: a page holding a document from before someone recorded a take used to
/// be able to erase it just by saving (swap adoption only protects takes
/// finalized in the last 30 s). A named, explicit operation cannot do that by
/// accident, which is what lets the web UI treat `takes` as not its property.
async fn delete_take(
    State(state): State<AppState>,
    Json(body): Json<DeleteBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut pf = state.store.get();
    let before = pf.project.takes.len();
    pf.project.takes.retain(|t| t.name != body.name);
    if pf.project.takes.len() == before {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no take named '{}'", body.name),
        ));
    }
    state
        .store
        .replace(pf)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(tx) = &state.engine {
        let _ = tx.send(EngineCommand::DeleteTake { name: body.name });
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

/// Rename a recorded take. Store first (durable), then the engine's live
/// copy — same order as PUT /api/project. Names are the adoption key for
/// takes, so duplicates are refused with 409.
async fn rename_take(
    State(state): State<AppState>,
    Json(body): Json<RenameBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let to = body.to.trim().to_string();
    if to.is_empty() || to.len() > 64 {
        return Err((
            StatusCode::BAD_REQUEST,
            "take name must be 1..=64 chars".into(),
        ));
    }
    let mut pf = state.store.get();
    if !pf.project.takes.iter().any(|t| t.name == body.from) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no take named '{}'", body.from),
        ));
    }
    if pf.project.takes.iter().any(|t| t.name == to) {
        return Err((
            StatusCode::CONFLICT,
            format!("a take named '{to}' already exists"),
        ));
    }
    for t in &mut pf.project.takes {
        if t.name == body.from {
            t.name = to.clone();
        }
    }
    state
        .store
        .replace(pf)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(tx) = &state.engine {
        let _ = tx.send(EngineCommand::RenameTake {
            from: body.from,
            to,
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn put_project(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<StatusCode, StoreError> {
    let text = String::from_utf8_lossy(&body).into_owned();
    let incoming = ProjectFile::from_json_str(&text).map_err(StoreError::Model)?;
    // Parse gate first, then persist; only a durable save is pushed to the
    // engine. (Engine applies at its own safe point — no reply path, R5.)
    state.store.replace(incoming.clone())?;
    if let Some(tx) = &state.engine {
        tx.send(EngineCommand::SetProject(incoming))
            .map_err(|_| StoreError::Io("engine channel closed".into()))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------- daw keymap

/// Thread-safe holder for the live [`DawKeymap`] plus its file location and
/// a change counter (D26 phase 2). Writers: HTTP handlers only (validate →
/// persist atomically → publish). Reader: the engine's main-thread MIDI
/// feeder, which takes an `Arc` clone per event — the audio thread never
/// touches this type (R5; same lane discipline as [`LiveStatus`]).
pub struct KeymapSlot {
    path: PathBuf,
    map: Mutex<Arc<DawKeymap>>,
    version: AtomicU64,
}

#[derive(Debug, thiserror::Error)]
pub enum KeymapError {
    #[error("{0}")]
    Validation(String),
    #[error("io: {0}")]
    Io(String),
}

impl IntoResponse for KeymapError {
    fn into_response(self) -> Response {
        let status = match self {
            KeymapError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            KeymapError::Validation(_) => StatusCode::BAD_REQUEST,
        };
        (status, self.to_string()).into_response()
    }
}

/// Keymap file precedence: `POCKETSTAGE_KEYMAP` (tests/CI) >
/// `$HOME/.pocketstage/keymap.json` — global device config, not per-project
/// (Q1: bindings belong to the keyboard, not the song).
pub fn keymap_path() -> PathBuf {
    match std::env::var("POCKETSTAGE_KEYMAP") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".pocketstage").join("keymap.json")
        }
    }
}

impl KeymapSlot {
    /// Load `path`; NEVER fails: a missing file is the normal defaults case,
    /// a corrupt/invalid one falls back to defaults with a loud log line
    /// ("bad keymap must never fail boot", specs/daw-keymap.md).
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let (map, warn) = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (DawKeymap::default(), None),
            Err(e) => (
                DawKeymap::default(),
                Some(format!("cannot read {}: {e}", path.display())),
            ),
            Ok(text) => match DawKeymap::from_json_str(&text) {
                Err(e) => (
                    DawKeymap::default(),
                    Some(format!(
                        "corrupt keymap {}: invalid JSON: {e} — using defaults",
                        path.display()
                    )),
                ),
                Ok(m) => match m.validate() {
                    Ok(()) => (m, None),
                    Err(e) => (
                        DawKeymap::default(),
                        Some(format!(
                            "corrupt keymap {}: {e} — using defaults",
                            path.display()
                        )),
                    ),
                },
            },
        };
        if let Some(w) = warn {
            eprintln!("[keymap] WARNING: {w}");
        }
        Self {
            path,
            map: Mutex::new(Arc::new(map)),
            version: AtomicU64::new(0),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// Current live map (feeder calls this per MIDI event — Arc clone).
    pub fn get(&self) -> Arc<DawKeymap> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// Validate → persist (tmp+rename, same durability pattern as
    /// [`ProjectStore::replace`]) → publish. Any failure leaves memory and
    /// disk untouched.
    pub fn replace(&self, incoming: DawKeymap) -> Result<(), KeymapError> {
        incoming
            .validate()
            .map_err(|e| KeymapError::Validation(e.clone()))?;
        let json = incoming.to_json_string();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| KeymapError::Io(e.to_string()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| KeymapError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| KeymapError::Io(e.to_string()))?;
        *self.map.lock().unwrap_or_else(|p| p.into_inner()) = Arc::new(incoming);
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Restore the built-in defaults (writes them to disk like any PUT).
    pub fn reset(&self) -> Result<(), KeymapError> {
        self.replace(DawKeymap::default())
    }
}

fn keymap_slot(state: &AppState) -> Result<&Arc<KeymapSlot>, (StatusCode, String)> {
    state.keymap.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "no keymap configured".into(),
    ))
}

/// Current map + built-in defaults for diffing, plus the change counter.
async fn get_keymap(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let slot = keymap_slot(&state)?;
    Ok(Json(serde_json::json!({
        "schema": KEYMAP_SCHEMA,
        "version": slot.version(),
        "path": slot.path().display().to_string(),
        "keymap": &*slot.get(),
        "defaults": DawKeymap::default(),
    })))
}

async fn put_keymap(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<StatusCode, KeymapError> {
    let slot = keymap_slot(&state).map_err(|(s, m)| match s {
        StatusCode::SERVICE_UNAVAILABLE => KeymapError::Io(m),
        _ => KeymapError::Validation(m),
    })?;
    let text = String::from_utf8_lossy(&body).into_owned();
    let incoming = DawKeymap::from_json_str(&text)
        .map_err(|e| KeymapError::Validation(format!("invalid keymap JSON: {e}")))?;
    slot.replace(incoming)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reset_keymap(State(state): State<AppState>) -> Result<StatusCode, KeymapError> {
    let slot = keymap_slot(&state).map_err(|(s, m)| match s {
        StatusCode::SERVICE_UNAVAILABLE => KeymapError::Io(m),
        _ => KeymapError::Validation(m),
    })?;
    slot.reset()?;
    Ok(StatusCode::NO_CONTENT)
}

/// Thread-safe holder for the live [`MappingPair`] plus its file location
/// and a change counter (phase 5, D29/D37). Exact mirror of [`KeymapSlot`]:
/// writers are HTTP handlers only (validate → persist atomically →
/// publish); the reader is the engine's main-thread MIDI feeder, one Arc
/// clone per message — the audio thread never touches this (R5).
pub struct MappingSlot {
    path: PathBuf,
    maps: Mutex<Arc<MappingPair>>,
    version: AtomicU64,
}

/// Mapping file precedence: `POCKETSTAGE_MAPPING` (tests/CI) >
/// `$HOME/.pocketstage/mapping.json` — global device config, like the
/// keymap. One file, two sections (music + HUI boot together).
pub fn mapping_path() -> PathBuf {
    match std::env::var("POCKETSTAGE_MAPPING") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home)
                .join(".pocketstage")
                .join("mapping.json")
        }
    }
}

impl MappingSlot {
    /// Load `path`; NEVER fails: missing file = measured factory defaults;
    /// corrupt/invalid = defaults + loud warn (the keymap loader discipline
    /// — a bad device file must never fail boot, and never silently do
    /// something half-edited either).
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let (pair, warn) = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (MappingPair::default(), None),
            Err(e) => (
                MappingPair::default(),
                Some(format!("cannot read {}: {e}", path.display())),
            ),
            Ok(text) => match MappingPair::from_json_str(&text) {
                Err(e) => (
                    MappingPair::default(),
                    Some(format!(
                        "corrupt mapping {}: invalid JSON: {e} — using factory defaults",
                        path.display()
                    )),
                ),
                Ok(p) => match p.validate() {
                    Ok(()) => (p, None),
                    Err(e) => (
                        MappingPair::default(),
                        Some(format!(
                            "invalid mapping {}: {e} — using factory defaults",
                            path.display()
                        )),
                    ),
                },
            },
        };
        if let Some(w) = warn {
            eprintln!("[mapping] WARNING: {w}");
        }
        Self {
            path,
            maps: Mutex::new(Arc::new(pair)),
            version: AtomicU64::new(0),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// Current live pair (feeder calls this per MIDI event — Arc clone).
    pub fn get(&self) -> Arc<MappingPair> {
        self.maps.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// Validate → persist (tmp+rename) → publish. The whole sequence runs
    /// under the slot lock so memory and disk can never interleave (the
    /// D32 lesson from `ProjectStore::replace`, applied from day one);
    /// any failure leaves both untouched.
    pub fn replace(&self, incoming: MappingPair) -> Result<(), KeymapError> {
        let mut guard = self.maps.lock().unwrap_or_else(|p| p.into_inner());
        incoming
            .validate()
            .map_err(|e| KeymapError::Validation(e.clone()))?;
        let json = incoming.to_json_string();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| KeymapError::Io(e.to_string()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| KeymapError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| KeymapError::Io(e.to_string()))?;
        *guard = Arc::new(incoming);
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Restore the measured factory defaults (written like any PUT).
    pub fn reset(&self) -> Result<(), KeymapError> {
        self.replace(MappingPair::default())
    }
}

fn mapping_slot(state: &AppState) -> Result<&Arc<MappingSlot>, (StatusCode, String)> {
    state.mapping.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "no device mapping slot (embedded mode only)".into(),
    ))
}

/// Same shape as the keymap handlers: GET answers 503 without a slot,
/// PUT/reset fold that into an Io error like `put_keymap` does.
fn mapping_slot_err(state: &AppState) -> Result<&Arc<MappingSlot>, KeymapError> {
    mapping_slot(state).map_err(|(s, m)| match s {
        StatusCode::SERVICE_UNAVAILABLE => KeymapError::Io(m),
        _ => KeymapError::Validation(m),
    })
}

/// Current pair + factory defaults for diffing, plus the change counter.
async fn get_mapping(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let slot = mapping_slot(&state)?;
    Ok(Json(serde_json::json!({
        "version": slot.version(),
        "path": slot.path().display().to_string(),
        "music": &slot.get().music,
        "hui": &slot.get().hui,
        "defaults": MappingPair::default(),
        "editable": true,
        "note": "device-layer map = measured wire truth for THIS unit (see specs/device-maps/); the semantic DAW bindings are at /api/keymap. Edits hot-reload into the MIDI feeder immediately.",
    })))
}

async fn put_mapping(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<StatusCode, KeymapError> {
    let slot = mapping_slot_err(&state)?;
    let text = String::from_utf8_lossy(&body).into_owned();
    let incoming = MappingPair::from_json_str(&text)
        .map_err(|e| KeymapError::Validation(format!("invalid mapping JSON: {e}")))?;
    slot.replace(incoming)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reset_mapping(State(state): State<AppState>) -> Result<StatusCode, KeymapError> {
    let slot = mapping_slot_err(&state)?;
    slot.reset()?;
    Ok(StatusCode::NO_CONTENT)
}

/// Addition B: `POST /api/overlay/{mix|seq|none}` — exact overlay control
/// over the same EngineCommand lane as `/api/voice`, so a keymap that binds
/// nothing to mix_toggle can never trap the user in an overlay.
async fn set_overlay(
    State(state): State<AppState>,
    PathParam(target): PathParam<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let t = match target.as_str() {
        "mix" => OverlayTarget::Mix,
        "seq" => OverlayTarget::Seq,
        "none" => OverlayTarget::None,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown overlay target '{other}' (mix|seq|none)"),
            ));
        }
    };
    send_ctrl(&state, EngineCommand::SetOverlay(t))
}

impl IntoResponse for StoreError {
    fn into_response(self) -> Response {
        // Version-gate + validation failures are client errors; io is ours.
        let status = match self {
            StoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            StoreError::Model(_) | StoreError::Validation(_) => StatusCode::BAD_REQUEST,
        };
        (status, self.to_string()).into_response()
    }
}

#[cfg(test)]
mod tests;
