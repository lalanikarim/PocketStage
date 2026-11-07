//! PocketStage — audio engine spike (HANDOFF §5).
//!
//! Falsifiable question: can we get clean, low-latency audio on Linux from a
//! hand-rolled ALSA blocking-write loop driven by FluidSynth via hand-written
//! FFI, with sane CPU headroom?
//!
//! Architecture preview (mirrors the final design, scratchpad R5):
//!   * One FluidSynth instance, owned exclusively by the audio thread.
//!   * Events arrive over an mpsc channel (scripted feeder here later; real
//!     MIDI in a later crate). No locks on the hot path.
//!   * ALSA S16LE stereo, 256-frame periods x 3 @ 48 kHz, underrun recovery.
//!
//! Run:  cargo run --release --bin pocketstage-engine   (Ctrl+C to stop)

mod ffi;

use alsa::pcm::{Access, Format, Frames, PCM};
use alsa::{Direction, ValueOr};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- constants

const RATE: u32 = 48_000;
const PERIOD_FRAMES: usize = 256; // frames per period (buffer size knob)
const N_PERIODS: u32 = 3;
const SF2_PATH: &str = "/usr/share/sounds/sf2/FluidR3_GM.sf2";

/// Internal FluidSynth MIDI channel for the lead voice (track 1 in the real model).
const LEAD_CH: i32 = 0;
/// GM drum channel (channel 10 in MIDI land, index 9) — proves multi-timbral D3.
const DRUM_CH: i32 = 9;
/// GM program for the lead: 81 = "Lead 2 (sawtooth)".
const LEAD_PROGRAM: i32 = 81;

// ------------------------------------------------------------------- events

enum Event {
    NoteOn { chan: i32, key: i32, vel: i32 },
    NoteOff { chan: i32, key: i32 },
    ProgramChange { chan: i32, program: i32 },
}

/// An event plus the wall-clock time it was *intended* to fire. The audio
/// thread records (consume_time - intended) as scheduling latency.
struct TimedEvent {
    intended: Instant, // when the feeder meant to emit it (feeder jitter = send - intended)
    sent: Instant,     // when it hit the channel (engine latency = consume - sent)
    event: Event,
}

// -------------------------------------------------------------------- stats

#[derive(Default)]
struct Stats {
    cycles: AtomicU64,        // render+write iterations
    busy_ns: AtomicU64,       // time spent in synth render + interleave (not blocking writes)
    xruns: AtomicU64,         // ALSA underruns recovered
    events: AtomicU64,        // MIDI events consumed
    q_sum_us: AtomicU64,      // engine-internal queue wait: consumed - sent
    q_max_us: AtomicU64,      // worst queue wait in window
    feed_max_us: AtomicU64,   // worst feeder jitter (sent - intended) in window
    delay_frames_max: AtomicU64, // max snd_pcm_delay seen in window (output-side queue)
}

impl Stats {
    fn snapshot_and_reset(&self) -> [u64; 8] {
        [
            self.cycles.swap(0, Ordering::Relaxed),
            self.busy_ns.swap(0, Ordering::Relaxed),
            self.xruns.swap(0, Ordering::Relaxed),
            self.events.swap(0, Ordering::Relaxed),
            self.q_sum_us.swap(0, Ordering::Relaxed),
            self.q_max_us.swap(0, Ordering::Relaxed),
            self.feed_max_us.swap(0, Ordering::Relaxed),
            self.delay_frames_max.swap(0, Ordering::Relaxed),
        ]
    }
}

// --------------------------------------------------------------------- main

fn main() {
    // ALSA device selectable: `default` (PipeWire plugin here) or e.g. `plughw:1,0` for raw hardware.
    let device = std::env::args().nth(1).unwrap_or_else(|| "default".to_string());

    // ---- FluidSynth setup (main thread; handles move to the audio thread) ----
    let settings = ffi::Settings::new().expect("new_fluid_settings returned null");
    settings.set_num("synth.sample-rate", RATE as f64);
    settings.set_num("synth.gain", 0.7); // default master gain is 0.2 — too quiet for a test
    let synth = ffi::Synth::new(&settings).expect("new_fluid_synth returned null");

    match synth.load_soundfont(SF2_PATH) {
        Ok(id) => println!("[init] soundfont loaded (id {id}): {SF2_PATH}"),
        Err(e) => {
            eprintln!("[init] {e}");
            std::process::exit(1);
        }
    }
    synth.program_change(LEAD_CH, LEAD_PROGRAM);

    let (tx, rx) = mpsc::channel::<TimedEvent>();
    let stats = Arc::new(Stats::default());

    // ---- audio thread -------------------------------------------------------
    let a_stats = Arc::clone(&stats);
    let _audio_thread = thread::Builder::new()
        .name("ps-audio".into())
        .spawn(move || {
            try_realtime_priority();

            let pcm = open_playback(&device)
                .unwrap_or_else(|e| panic!("could not open ALSA playback device '{device}': {e}"));
            let io = pcm.io_checked::<i16>().expect("could not create PCM IO handle");

            let mut left = vec![0f32; PERIOD_FRAMES];
            let mut right = vec![0f32; PERIOD_FRAMES];
            let mut out = vec![0i16; PERIOD_FRAMES * 2]; // interleaved

            let mut cycle: u64 = 0;
            loop {
                // 1) Drain pending MIDI events (bounded per cycle — writes stay timely).
                for _ in 0..64 {
                    match rx.try_recv() {
                        Ok(te) => {
                            let q_us = te.sent.elapsed().as_micros() as u64;
                            let feed_us = te.intended.elapsed().as_micros() as u64 - q_us;
                            a_stats.events.fetch_add(1, Ordering::Relaxed);
                            a_stats.q_sum_us.fetch_add(q_us, Ordering::Relaxed);
                            a_stats.q_max_us.fetch_max(q_us, Ordering::Relaxed);
                            a_stats.feed_max_us.fetch_max(feed_us, Ordering::Relaxed);
                            match te.event {
                                Event::NoteOn { chan, key, vel } => synth.note_on(chan, key, vel),
                                Event::NoteOff { chan, key } => synth.note_off(chan, key),
                                Event::ProgramChange { chan, program } => {
                                    synth.program_change(chan, program)
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }

                // 2) Render float L/R -> interleave + soft-clip to i16.
                let t0 = Instant::now();
                synth.render(PERIOD_FRAMES, &mut left, &mut right);
                for i in 0..PERIOD_FRAMES {
                    let l = (left[i] * i32::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32);
                    let r = (right[i] * i32::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32);
                    out[2 * i] = l as i16;
                    out[2 * i + 1] = r as i16;
                }
                let busy = t0.elapsed().as_nanos() as u64;

                // 3) Blocking write; recover from underruns.
                if let Err(e) = io.writei(&out) {
                    a_stats.xruns.fetch_add(1, Ordering::Relaxed);
                    if let Err(re) = pcm.recover(e.errno(), true) {
                        eprintln!("[audio] unrecoverable ALSA error: {re} (alsa errno {})", e.errno());
                        return;
                    }
                }

                cycle += 1;
                a_stats.cycles.fetch_add(1, Ordering::Relaxed);
                a_stats.busy_ns.fetch_add(busy, Ordering::Relaxed);

                // 4) Sample the output queue depth occasionally (output-side latency).
                if cycle.is_multiple_of(64) && let Ok(delay) = pcm.delay() {
                    a_stats.delay_frames_max.fetch_max(delay as u64, Ordering::Relaxed);
                }
            }
            // Note: `synth` then `settings` drop here (reverse declaration order), which is the correct teardown order.
        })
        .expect("failed to spawn audio thread");

    // ---- stats printer ------------------------------------------------------
    let s_stats = Arc::clone(&stats);
    thread::Builder::new()
        .name("ps-stats".into())
        .spawn(move || {
            let window = Duration::from_millis(2000);
            loop {
                thread::sleep(window);
                let [cycles, busy_ns, xruns, events, q_sum, q_max, feed_max, delay_max] =
                    s_stats.snapshot_and_reset();
                let busy_pct = busy_ns as f64 / (window.as_nanos() as f64) * 100.0;
                let q_avg = if events > 0 { q_sum as f64 / events as f64 } else { 0.0 };
                // Period cadence: how much of the ideal cycle budget (busy vs period duration).
                let period_us = PERIOD_FRAMES as f64 * 1e6 / RATE as f64;
                println!(
                    "[stats] cycles={cycles:5} busy={busy_pct:4.2}% ({:.1} us/{} us period) \
                     events={events:3} queue avg/max={q_avg:6.1}/{q_max:6} us \
                     feeder_jitter_max={feed_max:5} us xruns={xruns} out_queue~{:.1} ms",
                    if cycles > 0 { busy_ns as f64 / cycles as f64 / 1000.0 } else { 0.0 },
                    period_us,
                    delay_max as f64 * 1000.0 / RATE as f64,
                );
            }
        })
        .expect("failed to spawn stats thread");

    // ---- scripted note feeder (no MIDI hardware required) -------------------
    // ~2-bar arpeggio loop at 120 BPM (eighth notes) + kick on every beat.
    println!("[feeder] arpeggio @ 120 BPM — Ctrl+C to stop");
    let pattern: [i32; 8] = [60, 64, 67, 71, 72, 71, 67, 64]; // C maj arpeggio, eighth notes
    let step = Duration::from_millis(250);
    let start = Instant::now();
    // Stamp `sent` at enqueue time so engine-side queue latency is separable from feeder wake-up jitter.
    let send = |intended: Instant, event: Event| {
        let _ = tx.send(TimedEvent { intended, sent: Instant::now(), event });
    };

    let mut i: u32 = 0;
    loop {
        let t = start + step * i;
        if let Some(remaining) = t.checked_duration_since(Instant::now()) {
            thread::sleep(remaining);
        }

        // Lead: note on now, note off for the *previous* step's note.
        let key = pattern[(i % pattern.len() as u32) as usize];
        send(t, Event::NoteOn { chan: LEAD_CH, key, vel: 96 });
        if i > 0 {
            let prev = pattern[((i - 1) % pattern.len() as u32) as usize];
            send(t, Event::NoteOff { chan: LEAD_CH, key: prev });
        }

        // Drums: kick (GM 36) on every beat (every other eighth).
        if i.is_multiple_of(2) {
            send(t, Event::NoteOn { chan: DRUM_CH, key: 36, vel: 100 });
            send(t, Event::NoteOff { chan: DRUM_CH, key: 36 }); // kick is self-decaying
        }

        // Swap lead program every ~8 s to prove program_change works mid-stream.
        if i > 0 && i.is_multiple_of(32) {
            let prog = if (i / 32).is_multiple_of(2) { LEAD_PROGRAM } else { 4 }; // saw lead <-> EPiano
            send(t, Event::ProgramChange { chan: LEAD_CH, program: prog });
        }

        i += 1;
    }
}

// ------------------------------------------------------------------ helpers

/// Opens ALSA playback (`default` = PipeWire ALSA plugin on the dev box; plain
/// ALSA/dmix or `plughw:` elsewhere) with our fixed geometry.
fn open_playback(device: &str) -> alsa::Result<PCM> {
    let pcm = PCM::new(device, Direction::Playback, false)?;

    let (actual_period, buf, actual_rate);
    {
        // NB: HwParams::any (fresh params), NOT hw_params_current — the latter
        // fails with EBADMSG(77) on PipeWire's ALSA plugin.
        let h = alsa::pcm::HwParams::any(&pcm)?;
        h.set_access(Access::RWInterleaved)?;
        h.set_format(Format::S16LE)?;
        h.set_rate(RATE, ValueOr::Nearest)?;
        h.set_channels(2)?;
        h.set_periods(N_PERIODS, ValueOr::Nearest)?;
        h.set_period_size(PERIOD_FRAMES as Frames, ValueOr::Nearest)?;
        pcm.hw_params(&h)?;

        actual_period = h.get_period_size().unwrap_or(PERIOD_FRAMES as Frames);
        buf = h.get_buffer_size()?;
        actual_rate = h.get_rate()?;

        // Start as soon as one period is queued (lowest startup latency).
        // Best-effort: default (start when buffer full) still works, just starts ~16 ms later.
        if let Ok(sw) = pcm.sw_params_current()
            && sw.set_start_threshold(actual_period).is_ok()
        {
            let _ = pcm.sw_params(&sw);
        }
    } // h/sw guards dropped here so `pcm` can be moved out

    println!("[alsa] opened '{device}': rate={actual_rate} period={actual_period} frames buffer={buf} frames");
    Ok(pcm)
}

/// Attempts SCHED_FIFO on the calling (audio) thread. Logs pass/fail — on a
/// stock desktop this needs privileges; the Pi setup will grant it via
/// limits/systemd. Not fatal either way for the spike.
fn try_realtime_priority() {
    #[repr(C)]
    struct SchedParam {
        sched_priority: i32,
    }
    unsafe extern "C" {
        fn sched_setscheduler(pid: i32, policy: i32, param: *const SchedParam) -> i32;
    }
    const SCHED_FIFO: i32 = 1; // libc::SCHED_FIFO
    let param = SchedParam { sched_priority: 80 };
    let rc = unsafe { sched_setscheduler(0, SCHED_FIFO, &param) };
    if rc == 0 {
        println!("[rt] SCHED_FIFO priority 80 acquired");
    } else {
        let err = std::io::Error::last_os_error();
        println!("[rt] SCHED_FIFO unavailable ({err}) — running with default scheduler (fine for spike)");
    }
}
