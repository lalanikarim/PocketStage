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

#[tokio::test]
async fn remote_transport_routes() {
    let path = temp_path("remote");
    cleanup(&path);
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let app = router(AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
    ));

    for (uri, expect) in [
        ("/api/transport/play", EngineCommand::Play),
        ("/api/transport/stop", EngineCommand::Stop),
        ("/api/transport/record", EngineCommand::Record),
        ("/api/scene/3", EngineCommand::LaunchScene(3)),
    ] {
        let (status, _) = call(
            app.clone(),
            Request::builder()
                .method("POST")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{uri}");
        let got = rx.try_recv().unwrap();
        assert_eq!(format!("{got:?}"), format!("{expect:?}"), "{uri}");
    }

    // Scene out of range → 400, engine untouched.
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/scene/9")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(rx.try_recv().is_err());

    // Standalone (no engine) → 503.
    let solo = router(test_state(&path));
    let (status, _) = call(
        solo,
        Request::builder()
            .method("POST")
            .uri("/api/transport/play")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// ------------------------------------------------------------ library mode

#[test]
fn library_migrates_saves_loads_deletes() {
    let root = temp_path("libroot");
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("projects");
    std::fs::create_dir_all(&root).unwrap();
    // legacy single file next to the library dir
    let mut legacy = ProjectFile {
        version: SCHEMA_VERSION,
        project: pocketstage_model::Project::default_project(),
    };
    legacy.project.name = "Legacy".into();
    std::fs::write(
        root.join("project.json"),
        serde_json::to_string(&legacy).unwrap(),
    )
    .unwrap();

    let store = ProjectStore::open_library(&lib).unwrap();
    assert!(store.is_library());
    assert_eq!(store.current_name(), "Default", "migrated from legacy");
    assert_eq!(store.get().project.name, "Legacy");

    store.save_as("Gig").unwrap();
    assert!(lib.join("Gig.json").exists());
    assert_eq!(store.list(), vec!["Default".to_string(), "Gig".to_string()]);

    store.load("Gig").unwrap();
    assert_eq!(store.current_name(), "Gig");
    // replace() now targets the new current file
    let mut pf = store.get();
    pf.project.name = "Edited".into();
    store.replace(pf).unwrap();
    let on_disk = std::fs::read_to_string(lib.join("Gig.json")).unwrap();
    assert!(on_disk.contains("Edited"));

    // current cannot be deleted; after switching away it can
    assert!(store.delete("Gig").is_err());
    store.load("Default").unwrap();
    store.delete("Gig").unwrap();
    assert!(!lib.join("Gig.json").exists());

    // path-traversal-ish names rejected
    assert!(store.save_as("../evil").is_err());
    assert!(store.load("").is_err());
}

#[tokio::test]
async fn projects_routes_end_to_end() {
    let root = temp_path("libroutes");
    let _ = std::fs::remove_dir_all(&root); // cleanup() is file-only
    let store = std::sync::Arc::new(ProjectStore::open_library(root.join("projects")).unwrap());
    let app = router(AppState::standalone(store.clone()));

    // save_as then list
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/projects/save_as")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Copy A"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/projects")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(v["projects"].as_array().unwrap().len(), 2); // Default + Copy A

    // load switches
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/projects/load")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Copy A"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(store.current_name(), "Copy A");

    // export raw
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/projects/raw?name=Copy%20A")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"version\""));

    // delete current -> 409
    let (status, _) = call(
        app,
        Request::builder()
            .method("POST")
            .uri("/api/projects/delete")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Copy A"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// ------------------------------------------------------------ take rename

async fn rename(app: &axum::Router, body: &str) -> StatusCode {
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/takes/rename")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    status
}

#[tokio::test]
async fn rename_take_route_validates_and_persists() {
    let path = temp_path("rename");
    cleanup(&path);
    let store = std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap());
    let mut pf = store.get();
    pf.project.takes = vec![
        pocketstage_model::Take {
            name: "Take 1".into(),
            events: vec![],
            muted: false,
        },
        pocketstage_model::Take {
            name: "Keep".into(),
            events: vec![],
            muted: false,
        },
    ];
    store.replace(pf).unwrap();
    let app = router(AppState::standalone(store.clone()));

    // Valid rename → 204 and persisted to the store (and thus disk).
    assert_eq!(
        rename(&app, r#"{"from":"Take 1","to":"Main Riff"}"#).await,
        StatusCode::NO_CONTENT
    );
    let names: Vec<String> = store
        .get()
        .project
        .takes
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(names.contains(&"Main Riff".to_string()));
    assert!(names.contains(&"Keep".to_string()));

    // Target name already taken → 409.
    assert_eq!(
        rename(&app, r#"{"from":"Main Riff","to":"Keep"}"#).await,
        StatusCode::CONFLICT
    );
    // Unknown source take → 404.
    assert_eq!(
        rename(&app, r#"{"from":"Nope","to":"X"}"#).await,
        StatusCode::NOT_FOUND
    );
    // Blank target name → 400.
    assert_eq!(
        rename(&app, r#"{"from":"Main Riff","to":"   "}"#).await,
        StatusCode::BAD_REQUEST
    );
    cleanup(&path);
}

#[tokio::test]
async fn rename_take_forwards_command_to_engine() {
    let path = temp_path("rename-cmd");
    cleanup(&path);
    let store = std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap());
    let mut pf = store.get();
    pf.project.takes = vec![pocketstage_model::Take {
        name: "Take 1".into(),
        events: vec![],
        muted: false,
    }];
    store.replace(pf).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let app = router(AppState::embedded(
        store.clone(),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
    ));

    assert_eq!(
        rename(&app, r#"{"from":"Take 1","to":"Groove A"}"#).await,
        StatusCode::NO_CONTENT
    );
    match rx.try_recv() {
        Ok(EngineCommand::RenameTake { from, to }) => {
            assert_eq!(from, "Take 1");
            assert_eq!(to, "Groove A");
        }
        other => panic!("expected RenameTake, got {other:?}"),
    }
    cleanup(&path);
}
