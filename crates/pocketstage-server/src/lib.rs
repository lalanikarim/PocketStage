//! PocketStage HTTP control plane (scratchpad R5: "browser is a remote
//! control, never an audio path").
//!
//! Commit-1 scope: pure REST project CRUD with disk persistence. No engine
//! coupling yet — embedding (command channel + WS snapshots) builds on the
//! [`ProjectStore`] boundary defined here.
//!
//! Routes:
//! - `GET  /api/health`  — liveness probe
//! - `GET  /api/project` — current project as a versioned [`ProjectFile`] envelope
//! - `PUT  /api/project` — replace the whole project: parsed through the model's
//!   version gate, validated, persisted atomically (tmp+rename), only then
//!   swapped into memory. Bad payloads are rejected with the reason and leave
//!   both disk and memory untouched.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pocketstage_model::{Project, ProjectFile, SCHEMA_VERSION};

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

/// Thread-safe project holder with atomic-ish disk persistence. The engine
/// will later own the in-memory truth; this store is its seam for save/load.
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

pub fn router(store: Arc<ProjectStore>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/project", get(get_project).put(put_project))
        .with_state(store)
}

async fn health(State(store): State<Arc<ProjectStore>>) -> Json<serde_json::Value> {
    let name = store.get().project.name;
    Json(serde_json::json!({ "ok": true, "project": name }))
}

async fn get_project(State(store): State<Arc<ProjectStore>>) -> Json<ProjectFile> {
    Json(store.get())
}

async fn put_project(
    State(store): State<Arc<ProjectStore>>,
    body: axum::body::Bytes,
) -> Result<StatusCode, StoreError> {
    let text = String::from_utf8_lossy(&body).into_owned();
    let incoming = ProjectFile::from_json_str(&text).map_err(StoreError::Model)?;
    store.replace(incoming)?;
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
