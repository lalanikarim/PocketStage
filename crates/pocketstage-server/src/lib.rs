//! PocketStage HTTP control plane (scratchpad R5: "browser is a remote
//! control, never an audio path").
//!
//! Slice 2: the router runs *inside* the engine process ([`spawn_engine_server`])
//! and talks to the audio thread exclusively through [`EngineCommand`]s on an
//! mpsc channel — the single-writer rule holds: the audio thread still owns
//! all mutable state; the server only ever *requests* changes.
//!
//! Routes:
//! - `GET  /api/health`  — liveness probe
//! - `GET  /api/project` — current project as a versioned [`ProjectFile`] envelope
//! - `PUT  /api/project` — replace the whole project: parsed through the model's
//!   version gate, validated, persisted atomically (tmp+rename), then handed to
//!   the engine. Bad payloads are rejected with the reason and leave disk,
//!   memory, and the live engine untouched.

use axum::extract::Path as PathParam;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pocketstage_model::{Project, ProjectFile, SCHEMA_VERSION, Take};

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
    /// Rename a live take so the audio thread's copy matches the store
    /// (take adoption after later swaps keys on names — renaming only the
    /// store side would duplicate the take on the next swap).
    RenameTake { from: String, to: String },
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
    pub fn open_or_default(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let project = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let pf = ProjectFile::from_json_str(&text).map_err(StoreError::Model)?;
                pf.validate().map_err(StoreError::Validation)?;
                pf
            }
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
        let mut names: Vec<(std::time::SystemTime, String)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json")
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
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
                        Some(p.file_stem()?.to_str()?.to_string())
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
        let pf = ProjectFile::from_json_str(&text).map_err(StoreError::Model)?;
        pf.validate().map_err(StoreError::Validation)?;
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

        let path = self.path.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| StoreError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| StoreError::Io(e.to_string()))?;

        *self.project.lock().unwrap_or_else(|p| p.into_inner()) = incoming;
        Ok(())
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
    /// D25 single active voice: GM program 0..=127, or [`VOICE_DRUMS`].
    /// Written by the audio thread (boot / swap / SelectVoice); read by the
    /// MIDI feeder for routing and by WS for display.
    pub active_voice: AtomicU32,
}

/// Sentinel value of [`LiveStatus::active_voice`] meaning "the kit".
/// Real programs are 0..=127, so this can never alias.
pub const VOICE_DRUMS: u32 = u32::MAX;

// ----------------------------------------------------------------- routes

/// Shared handler state: the store, live engine levels, and — when embedded —
/// the command lane out to / feed in from the audio thread.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<ProjectStore>,
    /// `None` = standalone (file CRUD only, no live engine to notify).
    pub engine: Option<Sender<EngineCommand>>,
    pub live: Arc<LiveStatus>,
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
}

impl AppState {
    /// Standalone state (no engine channels).
    pub fn standalone(store: Arc<ProjectStore>) -> Self {
        Self {
            store,
            engine: None,
            live: Arc::new(LiveStatus::default()),
            take_feed: None,
            edit_feed: None,
        }
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
            take_feed: Some(Arc::new(Mutex::new(take_feed))),
            edit_feed: Some(Arc::new(Mutex::new(edit_feed))),
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
        .route("/api/projects", get(list_projects))
        .route("/api/projects/save_as", post(save_as_project))
        .route("/api/projects/load", post(load_project))
        .route("/api/projects/delete", post(delete_project))
        .route("/api/projects/raw", get(raw_project))
        .route("/api/takes/rename", post(rename_take))
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

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let name = state.store.get().project.name;
    Json(serde_json::json!({ "ok": true, "project": name }))
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
            // D25: current performance voice — GM program number or "drums".
            "voice": if voice == VOICE_DRUMS {
                serde_json::json!("drums")
            } else {
                serde_json::json!(voice)
            },
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
