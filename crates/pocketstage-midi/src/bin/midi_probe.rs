//! T1 — `midi_probe`: ground-truth tool for the Oxygen 49 MKV (scratchpad R9).
//!
//! Usage:
//!   midi_probe                 list all rawmidi input ports
//!   midi_probe watch [dev...]  timestamped decode of everything received;
//!                              defaults to ALL input ports (the MKV exposes
//!                              USB / Mackie / Editor subdevices)
//!   midi_probe pump            run the production pump (hotplug + mpsc) and
//!                              print PumpMessages — smoke test for `spawn_pump`
//!   midi_probe send <dev> <hex..>  write raw bytes to an OUTPUT subdevice
//!                              (T2: poke SysEx at the Editor port, e.g.
//!                              `send hw:3,0,2 F0 00 51 3F 01 F7`)
//!   midi_probe t7              scripted T7 capture: one labelled segment per
//!                              factory DAW personality (ENTER advances, q +
//!                              ENTER finishes and prints a per-segment CC /
//!                              note summary — diffable across personalities)
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
        Some("send") => send(&args[1..]),
        Some("t7") => t7(&args[1..]),
        Some(other) => {
            eprintln!(
                "unknown command: {other}\nusage: midi_probe [watch|pump <dev> ... | send <dev> <hex..> | t7]"
            );
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

/// Write raw hex bytes to an output subdevice. Rawmidi opens duplex, so
/// `hw:3,0,2` captured elsewhere still works while we send.
fn send(args: &[String]) -> ! {
    if args.len() < 2 {
        eprintln!("usage: midi_probe send <dev> <hex byte> ...  (e.g. F0 00 51 3F 01 F7)");
        std::process::exit(2);
    }
    let bytes: Vec<u8> = args[1..]
        .iter()
        .map(|h| {
            u8::from_str_radix(h.trim_start_matches("0x"), 16)
                .unwrap_or_else(|_| panic!("invalid hex byte: {h}"))
        })
        .collect();
    let rm = Rawmidi::new(&args[0], Direction::Playback, false)
        .unwrap_or_else(|e| panic!("cannot open {} for output: {e}", args[0]));
    let mut io = rm.io();
    use std::io::Write;
    io.write_all(&bytes)
        .unwrap_or_else(|e| panic!("write failed: {e}"));
    // Give the host class driver a beat to chew on before we close the port.
    std::thread::sleep(std::time::Duration::from_millis(50));
    println!(
        "[send] wrote {} bytes to {}: {:02X?}",
        bytes.len(),
        args[0],
        bytes
    );
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

// ----------------------------------------------------------------------- T7

/// One segment's observed traffic, aggregated for cross-personality diffs.
#[derive(Default)]
struct Agg {
    /// (channel, cc) -> (min, max, count)
    cc: std::collections::BTreeMap<(i32, i32), (i32, i32, u32)>,
    notes: Option<(i32, i32)>,
    note_on: u32,
    sysex: u32,
    other: u32,
}

impl Agg {
    fn record(&mut self, ev: &Event) {
        match ev {
            Event::ControlChange {
                channel,
                control,
                value,
            } => {
                let e = self
                    .cc
                    .entry((*channel as i32, *control as i32))
                    .or_insert((*value as i32, *value as i32, 0));
                e.0 = e.0.min(*value as i32);
                e.1 = e.1.max(*value as i32);
                e.2 += 1;
            }
            Event::NoteOn { key, .. } => {
                self.note_on += 1;
                self.notes = Some(match self.notes {
                    Some((lo, hi)) => (lo.min(*key as i32), hi.max(*key as i32)),
                    None => (*key as i32, *key as i32),
                });
            }
            Event::NoteOff { .. } => {}
            Event::SystemExclusive { .. } => self.sysex += 1,
            _ => self.other += 1,
        }
    }
}

/// Scripted T7 capture (D26 addendum): walk the six factory DAW
/// personalities + a PRESET baseline, one labelled segment each. The log
/// stays greppable (`grep 'DAW-3'`), and the closing summary answers the
/// probe's actual questions: do knob CC *numbers* change per personality
/// (vs only meaning)? what exactly do `<`/`>` emit? does SHIFT+pad
/// submode switching put anything on the wire at all?
fn t7(filter: &[String]) -> ! {
    let segments: Vec<String> = [
        "BASE-PRESET — PRESET mode: keys both ends, pads bank1 + SHIFT bank2, every fader, knob, fader button, wheels".to_string(),
        "PRESET-SHIFT — PRESET: every SHIFT modifier combo you can reach".to_string(),
        "DAW-1 — SHIFT+DAW/PRESET + <> -> factory personality 1 (NC1): EVERY control incl. <>, knobs, SHIFT+pads (all submodes)".to_string(),
        "DAW-2 — next personality (NC2): same full sweep".to_string(),
        "DAW-3 — next personality (Mackie variant 1): same full sweep".to_string(),
        "DAW-4 — next personality (Mackie variant 2 / HUI): same full sweep".to_string(),
        "DAW-5 — next personality (N1 plain MIDI): same full sweep".to_string(),
        "DAW-6 — next personality (N2 plain MIDI): same full sweep + N3 if present".to_string(),
    ]
    .into();

    let ports: Vec<InputPort> = list_input_ports();
    let selected: Vec<&InputPort> = if filter.is_empty() {
        ports.iter().collect()
    } else {
        ports
            .iter()
            .filter(|p| filter.contains(&p.device_string()))
            .collect()
    };
    let mut handles: Vec<(String, Rawmidi, Parser)> = Vec::new();
    for p in &selected {
        match Rawmidi::new(&p.device_string(), Direction::Capture, true) {
            Ok(rm) => handles.push((p.device_string(), rm, Parser::new())),
            Err(e) => eprintln!("[t7] skip {} ({e})", p.device_string()),
        }
    }
    if handles.is_empty() {
        eprintln!("[t7] no ports open — nothing to capture");
        std::process::exit(1);
    }

    let nseg = segments.len();
    let shared = std::sync::Arc::new(std::sync::Mutex::new((0usize, false)));
    {
        let shared = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                let mut s = shared.lock().unwrap();
                if line.trim().eq_ignore_ascii_case("q") || s.0 + 1 >= nseg {
                    s.1 = true;
                    break;
                }
                s.0 += 1;
            }
        });
    }

    println!("[t7] ENTER advances to the next labelled segment, q+ENTER finishes.");
    let mut current_seg = usize::MAX; // force first banner
    let mut agg: std::collections::BTreeMap<(usize, String), Agg> = Default::default();
    let t0 = Instant::now();
    let mut buf = [0u8; 1024];
    loop {
        let (seg, quit) = {
            let s = shared.lock().unwrap();
            (s.0, s.1)
        };
        if quit {
            println!("\n[t7] done.\n");
            break;
        }
        if seg != current_seg {
            current_seg = seg;
            println!(
                "\n=== SEGMENT {seg}: {} ===\n",
                segments[seg.min(segments.len() - 1)]
            );
        }
        let mut got_data = false;
        for (name, rm, parser) in handles.iter_mut() {
            let mut io = rm.io();
            loop {
                match io.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        got_data = true;
                        let mut events = Vec::with_capacity(8);
                        parser.feed(&buf[..n], &mut events);
                        for ev in events {
                            println!(
                                "[{:7.3}s] seg{seg:>1} {name}  {}",
                                t0.elapsed().as_secs_f64(),
                                format_event(&ev)
                            );
                            agg.entry((seg, name.clone())).or_default().record(&ev);
                        }
                    }
                    Err(e) if e.raw_os_error().is_some_and(|c| c == -libc::EAGAIN) => break,
                    Err(e) => {
                        eprintln!("[t7] read error on {name}: {e}");
                        break;
                    }
                }
            }
        }
        if !got_data {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    // Summary: per segment + port, the CC inventory (numbers AND ranges) —
    // diff two segments' lists to see number-vs-meaning changes.
    let mut last_seg = usize::MAX;
    for ((seg, port), a) in &agg {
        if *seg != last_seg {
            last_seg = *seg;
            println!("\n--- summary segment {seg} ---");
        }
        println!(
            "  {port}: note_on x{} {}",
            a.note_on,
            match a.notes {
                Some((lo, hi)) => format!("(notes {lo}..{hi})"),
                None => String::new(),
            }
        );
        for ((ch, cc), (lo, hi, n)) in &a.cc {
            println!("    cc ch{ch} cc{cc:>3} val {lo}..{hi} x{n}");
        }
        if a.sysex > 0 {
            println!("    sysex x{}", a.sysex);
        }
        if a.other > 0 {
            println!("    other events x{}", a.other);
        }
    }
    std::process::exit(0);
}
