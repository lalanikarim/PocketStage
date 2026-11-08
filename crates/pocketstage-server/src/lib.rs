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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use pocketstage_model::{Project, ProjectFile, SCHEMA_VERSION};

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

// ----------------------------------------------------------------- routes

/// Shared handler state: the store plus, when embedded, the engine channel.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<ProjectStore>,
    /// `None` = standalone (file CRUD only, no live engine to notify).
    pub engine: Option<Sender<EngineCommand>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/project", get(get_project).put(put_project))
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
