//! Standalone control-plane server (dev convenience; the engine will embed
//! the same router once it grows the command channel).
//!
//! Env: `POCKETSTAGE_PROJECT` project file path (default `project.pocketstage.json`)
//!      `PORT`                   listen port (default `7777`)

use std::sync::Arc;

use pocketstage_server::{ProjectStore, router};

#[tokio::main]
async fn main() {
    let path = std::env::var("POCKETSTAGE_PROJECT")
        .unwrap_or_else(|_| "project.pocketstage.json".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7777);

    let store = Arc::new(
        ProjectStore::open_or_default(&path)
            .unwrap_or_else(|e| panic!("cannot open project store '{path}': {e}")),
    );
    println!("[server] project file: {}", store.path().display());

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("cannot bind :{port}: {e}"));
    println!("[server] listening on http://0.0.0.0:{port} (LAN-trusted, no auth in v0.1)");
    axum::serve(listener, router(store))
        .await
        .expect("axum serve");
}
