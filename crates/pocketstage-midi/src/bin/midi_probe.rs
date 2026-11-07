//! T1 — `midi_probe`: ground-truth tool for the Oxygen 49 MKV (scratchpad R9).
//!
//! Usage:
//!   midi_probe                 list all rawmidi input ports
//!   midi_probe watch [dev...]  timestamped decode of everything received;
//!                              defaults to ALL input ports (the MKV exposes
//!                              USB / Mackie / Editor subdevices)
//!   midi_probe pump            run the production pump (hotplug + mpsc) and
//!                              print PumpMessages — smoke test for `spawn_pump`
//!
//! Probe session checklist (T1):
//!   1. Put the unit in a known mode first (MIDI/Advanced — HUI/template modes
//!      remap CCs; scratchpad R10 warns about this).
//!   2. Move EVERY control slowly: each pad, knob, fader, key range ends,
//!      transport buttons, octave/transpose, wheels, sustain taps.
//!   3. Save the output (`midi_probe watch | tee probe-$(date +%F).log`).

use pocketstage_midi::{self as midi, Event, InputPort, Parser, list_input_ports};

use alsa::{Direction, Rawmidi};
use std::collections::HashMap;
use std::io::Read;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => list_ports(),
        Some("watch") => watch(&args[1..]),
        Some("pump") => pump(&args[1..]),
        Some(other) => {
            eprintln!("unknown command: {other}\nusage: midi_probe [watch|pump <dev> ...]");
            std::process::exit(2);
        }
    }
}

fn list_ports() {
    let ports = list_input_ports();
    if ports.is_empty() {
        println!("no rawmidi input ports found (is a MIDI device connected?)");
        return;
    }
    println!("{} input port(s):", ports.len());
    for p in &ports {
        println!("  {p}");
    }
    println!("\nwatch everything:  midi_probe watch");
    println!("watch one device :  midi_probe watch hw:3,0,0");
}

fn watch(filter: &[String]) -> ! {
    let ports: Vec<InputPort> = list_input_ports();
    let selected: Vec<&InputPort> = if filter.is_empty() {
        ports.iter().collect()
    } else {
        ports
            .iter()
            .filter(|p| filter.contains(&p.device_string()))
            .collect()
    };
    if selected.is_empty() {
        eprintln!("no matching input ports; run `midi_probe` to list them");
        std::process::exit(1);
    }

    // Open non-blocking so we can poll several subdevices from one thread.
    let mut handles: Vec<(String, Rawmidi, Parser)> = Vec::new();
    for p in &selected {
        match Rawmidi::new(&p.device_string(), Direction::Capture, true) {
            Ok(rm) => {
                println!("[watch] opened {}", p.device_string());
                handles.push((p.device_string(), rm, Parser::new()));
            }
            Err(e) => eprintln!("[watch] skip {} ({e})", p.device_string()),
        }
    }
    if handles.is_empty() {
        eprintln!("could not open any port (busy? try as user in `audio` group)");
        std::process::exit(1);
    }

    println!("[watch] move controls now — Ctrl+C to stop\n");
    let t0 = Instant::now();
    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut buf = [0u8; 1024];
    loop {
        let mut got_data = false;
        for (name, rm, parser) in handles.iter_mut() {
            let mut io = rm.io();
            loop {
                match io.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        got_data = true;
                        *counts.entry(name.clone()).or_default() += n as u64;
                        let mut events = Vec::with_capacity(8);
                        parser.feed(&buf[..n], &mut events);
                        for ev in events {
                            println!(
                                "[{:7.3}s] {name}  {}",
                                t0.elapsed().as_secs_f64(),
                                format_event(&ev)
                            );
                        }
                    }
                    // ALSA nonblocking reads surface as raw -EAGAIN.
                    Err(e) if e.raw_os_error().is_some_and(|c| c == -libc::EAGAIN) => break,
                    Err(e) => {
                        eprintln!("[watch] read error on {name}: {e}");
                        break;
                    }
                }
            }
        }
        // 5 ms poll: fine for a probe tool (this is not the audio path).
        if !got_data {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Drive the real pump (`spawn_pump`: polling reads + hotplug rescans) and
/// print what the engine would receive.
fn pump(filter: &[String]) -> ! {
    let filter = (!filter.is_empty()).then(|| filter.to_vec());
    let (tx, rx) = std::sync::mpsc::channel();
    let _handle = midi::spawn_pump(tx, filter).expect("failed to spawn pump");
    println!("[pump] running — Ctrl+C to stop\n");
    let t0 = Instant::now();
    for msg in rx {
        match msg {
            midi::PumpMessage::Midi(m) => {
                println!(
                    "[{:7.3}s] {}  {}",
                    t0.elapsed().as_secs_f64(),
                    m.port,
                    format_event(&m.event)
                );
            }
            midi::PumpMessage::PortConnected(p) => {
                println!(
                    "[{:7.3}s] HOTPLUG  +connected {}",
                    t0.elapsed().as_secs_f64(),
                    p.device_string()
                );
            }
            midi::PumpMessage::PortDisconnected(p) => {
                println!(
                    "[{:7.3}s] HOTPLUG  -disconnected {}",
                    t0.elapsed().as_secs_f64(),
                    p.device_string()
                );
            }
        }
    }
    std::process::exit(0)
}

fn format_event(ev: &Event) -> String {
    match ev {
        Event::NoteOn {
            channel,
            key,
            velocity,
        } if *velocity > 0 => {
            format!(
                "note_on     ch{channel} key{key:>3} vel{velocity:>3}  {}",
                note_name(*key)
            )
        }
        Event::NoteOff { channel, key, .. } => format!("note_off    ch{channel} key{key:>3}"),
        Event::ControlChange {
            channel,
            control,
            value,
        } => {
            format!("cc          ch{channel} cc{control:>3} val{value:>3}")
        }
        Event::ProgramChange { channel, program } => {
            format!("prog_change ch{channel} prog{program:>3}")
        }
        Event::PitchBend { channel, value } => {
            format!("pitch_bend  ch{channel} {value:#06x} (center 0x2000)")
        }
        Event::ChannelPressure { channel, value } => {
            format!("chan_press  ch{channel} val{value:>3}")
        }
        Event::PolyPressure {
            channel,
            key,
            value,
        } => {
            format!("poly_press  ch{channel} key{key:>3} val{value:>3}")
        }
        Event::SystemExclusive { data } => {
            let hex: Vec<String> = data.iter().take(24).map(|b| format!("{b:02X}")).collect();
            let more = if data.len() > 24 { " …" } else { "" };
            format!("sysex ({}B) {}{more}", data.len(), hex.join(" "))
        }
        other => format!("{other:?}"),
    }
}

fn note_name(key: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("({}{})", NAMES[key as usize % 12], key as i32 / 12 - 1)
}
