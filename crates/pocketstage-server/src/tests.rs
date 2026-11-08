//! Store + route tests. Disk tests use a unique temp file per test; the HTTP
//! tests run the router in-process (oneshot) — no ports, CI-safe.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // oneshot

use pocketstage_model::{Project, ProjectFile, SCHEMA_VERSION};

use crate::{AppState, EngineCommand, ProjectStore, router};

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ps-server-test-{name}.json"))
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("json.tmp"));
}

#[test]
fn open_or_default_starts_from_default_project() {
    let path = temp_path("default");
    cleanup(&path);
    let store = ProjectStore::open_or_default(&path).unwrap();
    assert_eq!(store.get().project, Project::default_project());
    // Not written until first PUT.
    assert!(!path.exists());
    cleanup(&path);
}

#[test]
fn replace_persists_and_survives_reload() {
    let path = temp_path("roundtrip");
    cleanup(&path);
    let store = ProjectStore::open_or_default(&path).unwrap();

    let mut pf = store.get();
    pf.project.name = "Roadset".into();
    store.replace(pf).unwrap();
    assert!(path.exists());

    let reloaded = ProjectStore::open_or_default(&path).unwrap();
    assert_eq!(reloaded.get().project.name, "Roadset");
    cleanup(&path);
}

#[test]
fn rejects_bad_payloads_and_changes_nothing() {
    let path = temp_path("reject");
    cleanup(&path);
    let store = ProjectStore::open_or_default(&path).unwrap();
    let before = store.get();

    // Future schema version — rejected by the model's version gate.
    let future = r#"{"version": 999, "project": {"name":"x","tracks":[],"scenes":[{"name":"A"}]}}"#;
    assert!(matches!(
        ProjectFile::from_json_str(future),
        Err(pocketstage_model::ModelError::UnsupportedVersion(999))
    ));

    // Validation failure (loop_bars = 0) via the store: memory + disk untouched.
    let mut bad = before.clone();
    bad.project.loop_bars = 0;
    assert!(store.replace(bad).is_err());
    assert_eq!(store.get(), before);
    assert!(!path.exists()); // never written

    // Garbage JSON via from_json_str path.
    assert!(ProjectFile::from_json_str("not json").is_err());
    cleanup(&path);
}

#[test]
fn replace_creates_missing_parent_dirs() {
    // Regression: first save to a fresh ~/.pocketstage-style location.
    let mut path =
        std::env::temp_dir().join(format!("ps-server-test-nested-{}", std::process::id()));
    path.push("deeper");
    path.push("project.json");
    cleanup(&path);
    let store = ProjectStore::open_or_default(&path).unwrap();
    let mut pf = store.get();
    pf.project.name = "Fresh".into();
    store.replace(pf).unwrap();
    assert!(path.exists());
    let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap().parent().unwrap());
}

// ------------------------------------------------------------------ HTTP

fn test_state(path: &std::path::Path) -> AppState {
    AppState::standalone(std::sync::Arc::new(
        ProjectStore::open_or_default(path).unwrap(),
    ))
}

async fn call(app: axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn index_serves_the_ui() {
    let path = temp_path("index");
    cleanup(&path);
    let app = router(test_state(&path));
    let (status, body) = call(
        app,
        Request::builder().uri("/").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("PocketStage"));
    // The UI's API contract — keep these endpoints in sync with the page.
    assert!(body.contains("/api/project") && body.contains("/ws"));
}

#[tokio::test]
async fn http_round_trip() {
    let path = temp_path("http");
    cleanup(&path);
    let app = router(test_state(&path));

    // health
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"ok\":true"));

    // GET project — full versioned envelope
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/project")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pf: ProjectFile = serde_json::from_str(&body).unwrap();
    assert_eq!(pf.version, SCHEMA_VERSION);

    // PUT a renamed project → 204, then GET reflects it.
    let mut pf = pf;
    pf.project.name = "Setlist".into();
    let payload = serde_json::to_vec(&pf).unwrap();
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("PUT")
            .uri("/api/project")
            .body(Body::from(payload))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/project")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body.contains("Setlist"));

    // PUT garbage → 400 with reason.
    let (status, body) = call(
        app,
        Request::builder()
            .method("PUT")
            .uri("/api/project")
            .body(Body::from("{{not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!body.is_empty());
    cleanup(&path);
}

#[tokio::test]
async fn put_forwards_valid_project_to_engine_channel() {
    let path = temp_path("engine-cmd");
    cleanup(&path);
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let app = router(AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
    ));

    // Invalid payload → 400 and NOTHING reaches the engine.
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("PUT")
            .uri("/api/project")
            .body(Body::from(r#"{"version":1,"project":{"name":"x","loop_bars":0,"tracks":[],"scenes":[{"name":"A"}]}}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(rx.try_recv().is_err());

    // Valid payload → 204 and the engine receives SetProject.
    let mut pf = ProjectFile {
        version: SCHEMA_VERSION,
        project: Project::default_project(),
    };
    pf.project.name = "Live".into();
    let (status, _) = call(
        app,
        Request::builder()
            .method("PUT")
            .uri("/api/project")
            .body(Body::from(serde_json::to_vec(&pf).unwrap()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    match rx.try_recv() {
        Ok(EngineCommand::SetProject(got)) => assert_eq!(got.project.name, "Live"),
        other => panic!("expected SetProject, got {other:?}"),
    }
    cleanup(&path);
}

#[test]
fn drain_takes_appends_to_stored_project() {
    let path = temp_path("drain");
    cleanup(&path);
    let (ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let state = AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        std::sync::mpsc::channel::<EngineCommand>().0,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
    );

    let take = pocketstage_model::Take {
        name: "Take 1".into(),
        muted: false,
        events: vec![pocketstage_model::RecordedEvent::NoteOn {
            tick: 0,
            channel: 0,
            pitch: 60,
            velocity: 90,
        }],
    };
    ptx.send(take).unwrap();
    assert_eq!(state.drain_takes(), 1);
    assert_eq!(state.drain_takes(), 0);

    // Persisted into the project file (survives restart) …
    let reloaded = ProjectStore::open_or_default(&path).unwrap();
    assert_eq!(reloaded.get().project.takes.len(), 1);
    // … and surfaced through the WS status snapshot.
    let (_status, names) = state.status();
    assert_eq!(names, vec!["Take 1".to_string()]);
    cleanup(&path);
}
