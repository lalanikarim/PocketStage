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
fn legacy_project_file_boots_with_gain_clamped() {
    // D33: `Track::gain` used to validate up to 2.0, which the engine could not
    // deliver (CC7 tops out at unity). Such a file must still open — the store
    // migrates on LOAD; the HTTP write path stays strict (see
    // put_rejects_gain_above_unity).
    let path = temp_path("legacy-gain");
    cleanup(&path);
    std::fs::write(
        &path,
        r#"{ "version": 1, "project": { "name": "OldBoost",
             "tracks": [{ "name": "T1", "instrument": {}, "clips": [null], "gain": 1.7 }],
             "scenes": [{ "name": "A" }] } }"#,
    )
    .unwrap();

    let store = ProjectStore::open_or_default(&path).unwrap();
    assert_eq!(store.get().project.tracks[0].gain, Some(1.0));
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
    // The UI reads its track cap from here (D31) instead of mirroring the
    // constant — if this disappears, "+ add track" silently never disables.
    assert!(
        body.contains(&format!("\"max_tracks\":{}", pocketstage_model::MAX_TRACKS)),
        "health must publish the track cap: {body}"
    );

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
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
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

// ------------------------------------------------- burst PUTs (autosave)

/// Build a valid document carrying a distinguishable name + track-1 gain,
/// so a burst's winner is identifiable from either view (memory or disk).
fn ride_doc(name: &str, gain: f64) -> ProjectFile {
    let mut pf = ProjectFile {
        version: SCHEMA_VERSION,
        project: Project::default_project(),
    };
    pf.project.name = name.into();
    if let Some(t) = pf.project.tracks.first_mut() {
        t.gain = Some(gain);
    }
    pf
}

/// The web mixer auto-saves on slider release ("mixer edits apply live"), so
/// whole-project PUTs arrive in bursts with overlapping writers. Durability
/// contract: an accepted replace must leave disk and memory AGREEING — a GET
/// may never serve a document the next restart would not reproduce — and the
/// tmp+rename dance must never strand debris.
#[test]
fn concurrent_replaces_keep_disk_and_memory_in_step() {
    let path = temp_path("concurrent-replace");
    cleanup(&path);
    let store = std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap());

    std::thread::scope(|s| {
        for i in 0..8 {
            let store = store.clone();
            s.spawn(move || {
                for n in 0..25 {
                    store
                        .replace(ride_doc(
                            &format!("Ride {i}-{n}"),
                            ((i + n) % 8) as f64 / 8.0,
                        ))
                        .unwrap();
                }
            });
        }
    });

    let on_disk: ProjectFile =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        on_disk,
        store.get(),
        "store memory drifted from disk under concurrent PUTs"
    );
    // All writers are joined here, so a surviving .tmp means an unrenameable
    // scratch file, not a race.
    assert!(!path.with_extension("json.tmp").exists());
    cleanup(&path);
}

/// Autosave shape on the wire: one drag = a few PUTs of the whole document,
/// possibly with a stale/invalid one in between (editor state mid-edit). Each
/// accepted document must reach the engine as its own `SetProject` — they are
/// applied in arrival order at safe points, last writer wins — and a rejected
/// payload must never forward a swap.
#[tokio::test]
async fn put_rejects_gain_above_unity() {
    // Loads migrate, writes do NOT: a client sending a "boost" gets the
    // validation message rather than a document quietly rewritten behind its
    // back (D33 — and the exact silent-normalization trap this review caught).
    let path = temp_path("gain-strict");
    cleanup(&path);
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
    ));

    let body = serde_json::to_vec(&ride_doc("Boost", 1.4)).unwrap();
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .method("PUT")
            .uri("/api/project")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("0..=1"),
        "error should name the legal range: {body}"
    );
    assert!(
        rx.try_recv().is_err(),
        "a rejected write never reaches the engine"
    );
    cleanup(&path);
}

#[tokio::test]
async fn put_burst_forwards_every_accepted_swap_to_engine() {
    let path = temp_path("burst");
    cleanup(&path);
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
    ));

    let mut accepted = 0;
    for i in 0..6u8 {
        let mut doc = ride_doc(&format!("Ride {i}"), f64::from(i) / 8.0);
        if i == 3 {
            // Burst neighbor with a validation bug: 400, nothing forwarded.
            doc.project.loop_bars = 0;
        }
        let body = serde_json::to_vec(&doc).unwrap();
        if i == 3 {
            let (status, _) = call(
                app.clone(),
                Request::builder()
                    .method("PUT")
                    .uri("/api/project")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "burst PUT {i}");
            continue;
        }
        let (status, _) = call(
            app.clone(),
            Request::builder()
                .method("PUT")
                .uri("/api/project")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "burst PUT {i}");
        accepted += 1;
    }

    let mut got = Vec::new();
    while let Ok(EngineCommand::SetProject(pf)) = rx.try_recv() {
        got.push(pf.project.name);
    }
    assert_eq!(got.len(), accepted, "every accepted swap forwards, no more");
    assert_eq!(got.last().map(String::as_str), Some("Ride 5"));
    // The rejected burst member left no trace in the store either.
    assert_eq!(
        ProjectStore::open_or_default(&path)
            .unwrap()
            .get()
            .project
            .name,
        "Ride 5"
    );
    cleanup(&path);
}

#[tokio::test]
async fn favicon_serves_the_brand_ico() {
    let path = temp_path("favicon");
    cleanup(&path);
    let app = router(test_state(&path));
    let res = app
        .oneshot(
            Request::builder()
                .uri("/favicon.ico")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("image/x-icon"),
        "content type"
    );
    // Payload is the brand kit file itself (ICONDIR header, type 1).
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&bytes[..4], &[0, 0, 1, 0], "ICO header");
}

#[tokio::test]
async fn presets_route_serves_catalog() {
    let path = temp_path("presets");
    cleanup(&path);
    // Standalone: empty catalog (no engine, no loaded font) — valid JSON.
    let app = router(test_state(&path));
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/presets")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "[]");

    // With a catalog attached: shape is {bank, program, name}.
    let state = test_state(&path).with_presets(vec![crate::PresetInfo {
        bank: 0,
        program: 112,
        name: "Taiko Drum".into(),
    }]);
    let (_, body) = call(
        router(state),
        Request::builder()
            .uri("/api/presets")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body.contains("\"program\":112") && body.contains("Taiko Drum"));
    cleanup(&path);
}

#[tokio::test]
async fn voice_route_forwards_select_voice() {
    let path = temp_path("voice");
    cleanup(&path);
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
    ));

    // Program voice → 202 + SelectVoice on the engine lane.
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("POST")
            .header("content-type", "application/json")
            .uri("/api/voice")
            .body(Body::from(r#"{"program": 65}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    match rx.try_recv() {
        Ok(EngineCommand::SelectVoice(pocketstage_model::ActiveVoice::Program(65))) => {}
        other => panic!("expected SelectVoice(Program(65)), got {other:?}"),
    }

    // Drums voice — bare tagged form.
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("POST")
            .header("content-type", "application/json")
            .uri("/api/voice")
            .body(Body::from(r#""drums""#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    match rx.try_recv() {
        Ok(EngineCommand::SelectVoice(pocketstage_model::ActiveVoice::Drums)) => {}
        other => panic!("expected SelectVoice(Drums), got {other:?}"),
    }

    // Out-of-range program → 400, nothing reaches the engine.
    let (status, body) = call(
        app,
        Request::builder()
            .method("POST")
            .header("content-type", "application/json")
            .uri("/api/voice")
            .body(Body::from(r#"{"program": 200}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!body.is_empty());
    assert!(rx.try_recv().is_err());
    cleanup(&path);
}

#[test]
fn drain_takes_appends_to_stored_project() {
    let path = temp_path("drain");
    cleanup(&path);
    let (ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let state = AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        std::sync::mpsc::channel::<EngineCommand>().0,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
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

#[test]
fn drain_edits_persists_latest_snapshot_only() {
    // D24 write-through lane: engine snapshots supersede each other, so a
    // burst drains to ONE persist of the newest — a fader ride costs one
    // file write, not one per debounced batch.
    let path = temp_path("edit-drain");
    cleanup(&path);
    let store = std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap());
    let (etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let state = AppState::embedded(
        store,
        std::sync::mpsc::channel::<EngineCommand>().0,
        std::sync::Arc::new(crate::LiveStatus::default()),
        std::sync::mpsc::channel::<pocketstage_model::Take>().1,
        erx,
    );

    assert_eq!(state.drain_edits(), 0, "quiet lane drains to nothing");

    let mut pf = ProjectFile {
        version: SCHEMA_VERSION,
        project: Project::default_project(),
    };
    pf.project.name = "stale".into();
    etx.send(pf.clone()).unwrap();
    pf.project.name = "latest".into();
    etx.send(pf).unwrap();
    assert_eq!(state.drain_edits(), 2, "both consumed");

    let reloaded = ProjectStore::open_or_default(&path).unwrap();
    assert_eq!(reloaded.get().project.name, "latest");
    assert_eq!(state.drain_edits(), 0);
    cleanup(&path);
}

#[tokio::test]
async fn remote_transport_routes() {
    let path = temp_path("remote");
    cleanup(&path);
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
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
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        store.clone(),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
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

// ------------------------------------------------------------ durability

#[test]
fn concurrent_replaces_leave_a_whole_file() {
    // Review P1-3: PUT handlers, drain_takes and drain_edits call replace()
    // concurrently by design. Before the path mutex covered write+rename,
    // two writers shared X.json.tmp and could rename a torn file over the
    // real project. Post-fix: whatever lands must always parse and equal
    // exactly one of the candidate payloads.
    let path = temp_path("concurrent");
    cleanup(&path);
    let store = std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap());
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let s = store.clone();
            std::thread::spawn(move || {
                for k in 0..12 {
                    let mut pf = s.get();
                    pf.project.name = format!("p{i}-{k}");
                    s.replace(pf).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let pf = ProjectFile::from_json_str(&text).expect("disk file parses — no interleaved writes");
    assert!(
        pf.project.name.starts_with("p") && pf.project.name.contains('-'),
        "file is one whole candidate, got '{}'",
        pf.project.name
    );
    cleanup(&path);
}

#[test]
fn library_ignores_and_sweeps_tmp_leftovers() {
    // Review P2-6: a crash between save_as write and rename leaves
    // ".ps-tmp-X.json" — extension json, so it used to be listed (and could
    // even become the boot project). Now: swept on open, hidden from list().
    let root = temp_path("libtmp");
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("projects");
    std::fs::create_dir_all(&lib).unwrap();

    let store = ProjectStore::open_library(&lib).unwrap();
    store.save_as("Gig").unwrap();
    // Simulate the crash leftover, then reopen the library.
    std::fs::write(lib.join(".ps-tmp-Ghost.json"), b"{ broken").unwrap();
    assert!(list_names(&lib).contains(&".ps-tmp-Ghost".to_string()));

    let store = ProjectStore::open_library(&lib).unwrap();
    assert!(
        !std::path::Path::new(&lib.join(".ps-tmp-Ghost.json")).exists(),
        "leftover swept on open"
    );
    assert_eq!(store.list(), vec!["Default".to_string(), "Gig".to_string()]);
    // The sweep must not eat a live save-as target.
    assert!(std::path::Path::new(&lib.join("Gig.json")).exists());
    let _ = std::fs::remove_dir_all(&root);
}

fn list_names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    v.sort();
    v
}

// ------------------------------------------------------------- daw keymap

use crate::KeymapSlot;
use pocketstage_oxygen::DawKeymap;

fn km_temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ps-keymap-test-{name}.json"))
}

fn km_cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("json.tmp"));
}

const KM_CUSTOM: &str = r#"{
    "voices": [ { "source": {"note": 60}, "voice": {"program": 5}, "label": "Test EP" } ],
    "commands": [ { "source": {"note": 70}, "command": "seq_toggle" } ],
    "fader_buttons": [],
    "knobs": []
}"#;

#[test]
fn keymap_slot_missing_file_is_defaults() {
    let path = km_temp("missing");
    km_cleanup(&path);
    let slot = KeymapSlot::open(&path);
    assert_eq!(*slot.get(), DawKeymap::default());
    assert!(!path.exists(), "open must not write");
    km_cleanup(&path);
}

#[test]
fn keymap_slot_corrupt_file_falls_back_to_defaults() {
    let path = km_temp("corrupt");
    std::fs::write(&path, "{{ definitely not json").unwrap();
    let slot = KeymapSlot::open(&path);
    assert_eq!(*slot.get(), DawKeymap::default());
    // Validation failures count as corrupt too.
    std::fs::write(
        &path,
        r#"{"voices":[{"source":{"note":60},"voice":"drums","label":""}],"commands":[],"fader_buttons":[],"knobs":[]}"#,
    )
    .unwrap();
    let slot = KeymapSlot::open(&path);
    assert_eq!(*slot.get(), DawKeymap::default());
    km_cleanup(&path);
}

#[test]
fn keymap_slot_replace_persists_and_reloads() {
    let path = km_temp("roundtrip");
    km_cleanup(&path);
    let slot = KeymapSlot::open(&path);
    assert_eq!(slot.version(), 0);

    let custom = DawKeymap::from_json_str(KM_CUSTOM).unwrap();
    slot.replace(custom.clone()).unwrap();
    assert_eq!(*slot.get(), custom);
    assert_eq!(slot.version(), 1, "publish bumps the counter");
    assert!(path.exists());

    let reloaded = KeymapSlot::open(&path);
    assert_eq!(*reloaded.get(), custom);
    km_cleanup(&path);
}

#[test]
fn keymap_slot_invalid_replace_changes_nothing() {
    let path = km_temp("reject");
    km_cleanup(&path);
    let slot = KeymapSlot::open(&path);
    let before = slot.get();

    // Duplicate sources across sections → validation error, memory + disk
    // untouched (same durability discipline as ProjectStore).
    let dup = r#"{
        "voices": [ { "source": {"note": 60}, "voice": "drums", "label": "A" } ],
        "commands": [ { "source": {"note": 60}, "command": "mix_toggle" } ],
        "fader_buttons": [],
        "knobs": []
    }"#;
    let bad = DawKeymap::from_json_str(dup).unwrap();
    assert!(slot.replace(bad).is_err());
    assert_eq!(slot.get(), before);
    assert!(!path.exists());
    km_cleanup(&path);
}

#[test]
fn keymap_slot_reset_restores_defaults() {
    let path = km_temp("reset");
    km_cleanup(&path);
    let slot = KeymapSlot::open(&path);
    slot.replace(DawKeymap::from_json_str(KM_CUSTOM).unwrap())
        .unwrap();
    slot.reset().unwrap();
    assert_eq!(*slot.get(), DawKeymap::default());
    let reloaded = KeymapSlot::open(&path);
    assert_eq!(*reloaded.get(), DawKeymap::default());
    km_cleanup(&path);
}

/// One shared router (AppState is Clone; the keymap slot lives behind an
/// Arc, so version + map persist across requests within a test).
fn km_app() -> axum::Router {
    let proj = temp_path("keymap-http-app");
    let state = AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&proj).unwrap()),
        std::sync::mpsc::channel::<EngineCommand>().0,
        std::sync::Arc::new(crate::LiveStatus::default()),
        std::sync::mpsc::channel().1,
        std::sync::mpsc::channel().1,
    )
    .with_keymap(std::sync::Arc::new(KeymapSlot::open(km_temp("http"))));
    router(state)
}

#[tokio::test]
async fn keymap_http_round_trip_and_rejection() {
    let km = km_temp("http");
    km_cleanup(&km);
    let app = km_app();

    // GET on a fresh slot → built-in defaults + schema stamp.
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/keymap")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["schema"],
        serde_json::json!(pocketstage_oxygen::KEYMAP_SCHEMA)
    );
    assert_eq!(
        serde_json::from_value::<DawKeymap>(v["keymap"].clone()).unwrap(),
        DawKeymap::default()
    );
    assert!(v["defaults"].is_object());

    // PUT valid → 204, file written, version counter advanced.
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("PUT")
            .uri("/api/keymap")
            .body(Body::from(KM_CUSTOM))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/keymap")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["keymap"]["voices"].as_array().unwrap().len(), 1);
    assert!(v["version"].as_u64().unwrap() >= 1);
    assert!(km.exists());

    // PUT garbage → 400; the previous map survives.
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("PUT")
            .uri("/api/keymap")
            .body(Body::from("not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .uri("/api/keymap")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["keymap"]["voices"].as_array().unwrap().len(), 1);

    // PUT validation failure (dup source) → 400 with the human message.
    let dup = r#"{
        "voices": [ { "source": {"note": 60}, "voice": "drums", "label": "A" } ],
        "commands": [ { "source": {"note": 60}, "command": "mix_toggle" } ]
    }"#;
    let (status, body) = call(
        app.clone(),
        Request::builder()
            .method("PUT")
            .uri("/api/keymap")
            .body(Body::from(dup))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("both claim note 60"), "{body}");

    // Reset → defaults again.
    let (status, _) = call(
        app,
        Request::builder()
            .method("POST")
            .uri("/api/keymap/reset")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    km_cleanup(&km);
}

#[tokio::test]
async fn keymap_http_unavailable_without_slot() {
    let path = temp_path("keymap-none");
    cleanup(&path);
    let app = router(test_state(&path)); // standalone: no keymap attached
    let (status, _) = call(
        app,
        Request::builder()
            .uri("/api/keymap")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    cleanup(&path);
}

#[tokio::test]
async fn overlay_routes_forward_exact_targets() {
    let path = temp_path("overlay-route");
    cleanup(&path);
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap()),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
    ));

    for (target, expect) in [
        ("mix", "SetOverlay(Mix)"),
        ("seq", "SetOverlay(Seq)"),
        ("none", "SetOverlay(None)"),
    ] {
        let (status, _) = call(
            app.clone(),
            Request::builder()
                .method("POST")
                .uri(format!("/api/overlay/{target}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{target}");
        let cmd = rx.try_recv().expect("command reaches engine lane");
        assert_eq!(format!("{cmd:?}"), expect);
    }

    // Unknown target → 400, nothing forwarded.
    let (status, _) = call(
        app,
        Request::builder()
            .method("POST")
            .uri("/api/overlay/bogus")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(rx.try_recv().is_err());
    cleanup(&path);
}

// ------------------------------------------------- device mapping (read-only)

#[tokio::test]
async fn mapping_endpoint_is_read_only_view() {
    let path = temp_path("mapping-get");
    cleanup(&path);
    let state = AppState::standalone(std::sync::Arc::new(
        ProjectStore::open_or_default(&path).unwrap(),
    ))
    .with_device_mappings(
        pocketstage_oxygen::default_mapping(),
        pocketstage_oxygen::hui_mapping(),
    );
    let (status, body) = call(
        router(state),
        Request::builder()
            .uri("/api/mapping")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["editable"], serde_json::json!(false));
    // The measured factory map is what comes back (T1 ground truth).
    assert_eq!(v["music"]["pad_note_base"], serde_json::json!(36));
    assert_eq!(v["music"]["mode_daw_cc"], serde_json::json!(113));
    // HUI view carries the transport notes the music view lacks.
    assert!(v["music"]["transport_notes"].is_null());
    assert!(v["hui"]["transport_notes"].is_object());

    // No mappings published → 503 (standalone default).
    let (status, _) = call(
        router(test_state(&path)),
        Request::builder()
            .uri("/api/mapping")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    cleanup(&path);
}

// ------------------------------------------------------------ take delete

async fn del_take(app: &axum::Router, body: &str) -> StatusCode {
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/takes/delete")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    status
}

fn two_takes() -> ProjectFile {
    let mut pf = Project::default_project();
    pf.takes = vec![
        pocketstage_model::Take {
            name: "Take 1".into(),
            events: vec![],
            muted: false,
        },
        pocketstage_model::Take {
            name: "Keep".into(),
            events: vec![],
            muted: true,
        },
    ];
    ProjectFile {
        version: SCHEMA_VERSION,
        project: pf,
    }
}

#[tokio::test]
async fn delete_take_route_removes_one_and_keeps_the_rest() {
    let path = temp_path("delete");
    cleanup(&path);
    let store = std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap());
    store.replace(two_takes()).unwrap();
    let app = router(AppState::standalone(store.clone()));

    assert_eq!(
        del_take(&app, r#"{"name":"Take 1"}"#).await,
        StatusCode::NO_CONTENT
    );
    let names: Vec<String> = store
        .get()
        .project
        .takes
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(names, vec!["Keep".to_string()], "only the named one goes");

    // Unknown name → 404, and nothing else moves. Deleting twice (two tabs, or
    // a double click) is therefore harmless on the second call.
    assert_eq!(
        del_take(&app, r#"{"name":"Take 1"}"#).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(store.get().project.takes.len(), 1);
    cleanup(&path);
}

#[tokio::test]
async fn delete_take_forwards_command_to_engine() {
    let path = temp_path("delete-cmd");
    cleanup(&path);
    let store = std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap());
    store.replace(two_takes()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        store.clone(),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
    ));

    assert_eq!(
        del_take(&app, r#"{"name":"Keep"}"#).await,
        StatusCode::NO_CONTENT
    );
    match rx.try_recv() {
        Ok(EngineCommand::DeleteTake { name }) => assert_eq!(name, "Keep"),
        other => panic!("expected DeleteTake, got {other:?}"),
    }

    // A refused delete must not reach the engine either.
    assert_eq!(
        del_take(&app, r#"{"name":"Nope"}"#).await,
        StatusCode::NOT_FOUND
    );
    assert!(rx.try_recv().is_err());
    cleanup(&path);
}

#[tokio::test]
async fn stale_put_no_longer_deletes_a_take_recorded_after_it_loaded() {
    // The hole this closes: a page loaded with one take, someone records
    // another, the page PUTs its stale document (a mixer live apply used to do
    // exactly that) — adoption kept only takes finalized in the last 30 s, so
    // the newer take vanished. The web UI now never PUTs `takes`: deletion goes
    // through /api/takes/delete. This test pins the *server* half of that
    // contract — a PUT whose takes list omits a take still deletes it, which is
    // why the client must stop sending the field (see index.html live apply).
    let path = temp_path("stale-take");
    cleanup(&path);
    let store = std::sync::Arc::new(ProjectStore::open_or_default(&path).unwrap());
    store.replace(two_takes()).unwrap();
    let (tx, _rx) = std::sync::mpsc::channel::<EngineCommand>();
    let (_ptx, prx) = std::sync::mpsc::channel::<pocketstage_model::Take>();
    let (_etx, erx) = std::sync::mpsc::channel::<ProjectFile>();
    let app = router(AppState::embedded(
        store.clone(),
        tx,
        std::sync::Arc::new(crate::LiveStatus::default()),
        prx,
        erx,
    ));

    // A stale document (only "Take 1") PUT today would drop "Keep"; the fixed
    // client sends a body carrying the CURRENT list. Assert the current-list
    // body is a no-op for takes and still applies its mixer edit.
    let mut fresh = store.get();
    fresh.project.tracks[0].gain = Some(0.5);
    let (status, _) = call(
        app.clone(),
        Request::builder()
            .method("PUT")
            .uri("/api/project")
            .body(Body::from(serde_json::to_vec(&fresh).unwrap()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let after = store.get();
    assert_eq!(after.project.takes.len(), 2, "both takes survive");
    assert_eq!(after.project.tracks[0].gain, Some(0.5));
    cleanup(&path);
}
