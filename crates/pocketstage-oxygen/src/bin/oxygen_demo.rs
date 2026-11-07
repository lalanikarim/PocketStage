//! Live mapping demo: pump → oxygen mapper → stdout.
//!
//! Put the unit in **preset mode** (DAW LED unlit), then play/turn/slide —
//! every physical control prints as a semantic [`Action`]. TimingClock spam
//! and other noise stay silent, proving the mapper's discipline.
//!
//! ```text
//! cargo run -p pocketstage-oxygen --bin oxygen_demo            # all inputs
//! cargo run -p pocketstage-oxygen --bin oxygen_demo -- hw:3,0,0  # one port
//! ```

use std::sync::mpsc;
use std::time::Duration;

use pocketstage_midi::{PumpMessage, spawn_pump};
use pocketstage_oxygen::{Mapper, default_mapping};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let filter = if args.is_empty() { None } else { Some(args) };

    let mapping = default_mapping();
    if let Err(e) = mapping.validate() {
        eprintln!("internal default mapping invalid: {e}");
        std::process::exit(1);
    }
    let mapper = Mapper::new(mapping);

    let (tx, rx) = mpsc::channel();
    let _pump = match spawn_pump(tx, filter) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot start MIDI pump: {e}");
            std::process::exit(1);
        }
    };

    println!("oxygen_demo — move controls (preset mode!). Ctrl-C to stop.\n");
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(PumpMessage::Midi(m)) => {
                if let Some(action) = mapper.map(&m.event) {
                    println!("{:<10} {action:?}", m.port);
                }
            }
            Ok(PumpMessage::PortConnected(p)) => println!("  [+] {p}"),
            Ok(PumpMessage::PortDisconnected(p)) => println!("  [-] {p}"),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
