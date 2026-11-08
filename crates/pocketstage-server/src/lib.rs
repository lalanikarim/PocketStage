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

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
    path: PathBuf,
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
            path,
            project: Mutex::new(project),
        })
    }

    /// True when the file already existed (vs. a fresh default project).
    pub fn file_exists(&self) -> bool {
        self.path.exists()
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

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| StoreError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| StoreError::Io(e.to_string()))?;

        *self.project.lock().unwrap_or_else(|p| p.into_inner()) = incoming;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
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
    /// Finalized takes streamed out of the audio thread (engine → server).
    /// Drained by [`AppState::drain_takes`] on a timer; each one is appended
    /// to the stored project, so recordings survive reboot.
    take_feed: Option<Arc<Mutex<Receiver<Take>>>>,
}

impl AppState {
    /// Standalone state (no engine channels).
    pub fn standalone(store: Arc<ProjectStore>) -> Self {
        Self {
            store,
            engine: None,
            live: Arc::new(LiveStatus::default()),
            take_feed: None,
        }
    }

    /// Embedded state: full duplex with the audio thread (commands out on
    /// `engine`, finalized takes in on `take_feed`; `live` levels are written
    /// by the audio thread and pushed to WS clients).
    pub fn embedded(
        store: Arc<ProjectStore>,
        engine: Sender<EngineCommand>,
        live: Arc<LiveStatus>,
        take_feed: Receiver<Take>,
    ) -> Self {
        Self {
            store,
            engine: Some(engine),
            live,
            take_feed: Some(Arc::new(Mutex::new(take_feed))),
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
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/project", get(get_project).put(put_project))
        .route("/ws", get(ws_upgrade))
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
        let frame = serde_json::json!({
            "type": "status",
            "bpm": status.bpm,
            "transport": status.transport,
            "bar": status.bar,
            "beat": status.beat,
            "scene": status.scene,
            "mode": status.mode,
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
