//! Raw MIDI capture for hardware diagnostics (engine must be stopped —
//! rawmidi is single-reader). Prints every event with a ms timestamp:
//! `cargo run -p pocketstage-midi --example midi_capture 30`

use pocketstage_midi::{PumpMessage, spawn_pump};
use std::time::{Duration, Instant};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let (tx, rx) = std::sync::mpsc::channel();
    let _h = spawn_pump(tx, None).expect("spawn_pump (is the engine stopped?)");
    println!("capturing {secs}s — play now");
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        if let Ok(msg) = rx.recv_timeout(Duration::from_millis(200)) {
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            match msg {
                PumpMessage::Midi(m) => println!("{:>8.1} {} {:?}", ms, m.port, m.event),
                PumpMessage::PortConnected(p) => println!("{ms:>8.1} +port {p:?}"),
                PumpMessage::PortDisconnected(p) => println!("{ms:>8.1} -port {p:?}"),
            }
        }
    }
    println!("done");
}
