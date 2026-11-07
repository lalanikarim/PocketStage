# PocketStage — Handoff Document

> Created: 2026 – session handoff for continuing work after folder rename.
> Read this first, then read `specs/brainstorming/scratchpad.md` (the full design brainstorm).

---

## 1. What PocketStage is (30-second summary)

A portable lite DAW running headless on Linux (Raspberry Pi or laptop). A MIDI
keyboard (**M-Audio Oxygen 49 MKV**) is the primary live controller; a web UI
(phone/tablet browser) is secondary for setup/management. All audio stays local
(the browser is a remote control, never an audio path). Pattern/clip-based
(Ableton-style clip matrix), FluidSynth + soundfonts, single Rust binary.

Full design: `specs/brainstorming/scratchpad.md`

---

## 2. Immediate task: rename the folder

The repo folder is still named with the old working title. From a shell (NOT from
inside this session's cwd):

```bash
shopt -s nullglob; rmdir ~/Projects/PocketStage 2>/dev/null
mv ~/Projects/PiDaw ~/Projects/PocketStage
cd ~/Projects/PocketStage
```

Then verify:

```bash
cargo build          # should print no errors
ls specs/brainstorming/scratchpad.md
```

Everything else in the repo already uses the `pocketstage` name (workspace, crate).

---

## 3. Current state of the code

```
PocketStage/            (folder currently named PiDaw)
├── Cargo.toml                       # workspace, member: crates/pocketstage-engine
├── .gitignore                       # ignores .pi/
├── specs/brainstorming/scratchpad.md   # full design brainstorm + decision log D1–D10
└── crates/pocketstage-engine/
    ├── Cargo.toml                   # deps: alsa 0.9, libc 0.2
    └── src/main.rs                  # PLACEHOLDER stub — prints one line
```

- `cargo build` verified working (BUILD_OK).
- **Spike DONE** (see `SPIKE_RESULTS.md`): `src/main.rs` + `src/ffi.rs` implement the
  full ALSA + FluidSynth-FFI spike with metrics; runs glitch-free, ~1% CPU, 0 xruns.
  Next task: project file format (`pocketstage-model`) — backlog item 1 below.

---

## 4. Environment facts (verified this session)

| Item | Status |
|------|--------|
| Dev machine | System76 Pangolin laptop, Ubuntu-based (Pop!_OS), PipeWire audio |
| Rust | 1.99.1 via cargo ✓ |
| ALSA dev headers | `libasound2-dev` 1.2.11 ✓ |
| FluidSynth | runtime `libfluidsynth3` 2.3.4 + **`libfluidsynth-dev` installed this session** ✓ |
| Soundfont | `/usr/share/sounds/sf2/FluidR3_GM.sf2` present ✓ (also `TimGM6mb.sf2`, `default-GM.sf2`) |
| Oxygen 49 MKV | **Connected to this laptop**, ALSA card 3, sequencer client 28 with ports: `28:0` USB MIDI (main), `28:1` Mackie/HUI, `28:2` Editor (SysEx) |
| Internet | works generally; m-audio.com unreachable from this box (possibly blocked) |

Useful device-check commands:

```bash
aconnect -l                                  # list ALSA sequencer clients/ports
amidi -p hw:3,0 -d                           # dump raw MIDI from Oxygen (Ctrl+C stop)
aplay -l                                     # playback devices
```

---

## 5. Audio engine spike — ✅ DONE (results in `SPIKE_RESULTS.md`)

Goal: falsify the riskiest part of the project — clean low-latency audio with
FluidSynth in Rust on Linux — before writing any UI/format/mapping code.

> **Outcome (2026-11-07):** implemented and validated. Verdict: architecture confirmed
> (hand-rolled ALSA loop + FluidSynth FFI). 0 xruns, ~1.0–1.4% CPU, event→render ≤ 1
> period (5.3 ms), SCHED_FIFO denied on desktop yet still glitch-free. Remaining:
> human audible check (`cargo run --release`); raw `plughw:` + Pi measurements later.
> Details, numbers, and quirks log: `SPIKE_RESULTS.md`.

### Plan

1. In `crates/pocketstage-engine`, replace the stub with a spike binary:
   - Open ALSA playback via `alsa` crate (`Pcm::open(None, "default", Stream::Playback, None)`
     works under PipeWire; period ~256 frames × 3 periods @ 48 kHz S16LE stereo).
     Handle underruns with `recover()`.
   - FluidSynth via **hand-written FFI** (`#[link(name = "fluidsynth")]`, extern "C" decls —
     the C API is stable across 2.x; crates.io bindings are stale/abandoned, avoid).
     Needed fns: `new_fluid_settings`, `new_fluid_synth`, `delete_fluid_synth(_settings)`,
     `fluid_synth_sfload`, `fluid_synth_noteon/noteoff`, `fluid_synth_program_change`,
     `fluid_synth_write_float`.
   - Audio thread: render float L/R with `fluid_synth_write_float` → interleave+clip → i16 → ALSA write.
   - Scripted note feeder (channel + std::sync::mpsc to audio thread) — **no MIDI hardware needed**.
   - Load `/usr/share/sounds/sf2/FluidR3_GM.sf2`, play a ~10 s arpeggio loop at 120 BPM.
2. Instrument & report:
   - Scheduling latency: timestamp between event injection and the render call that consumed it (avg/max).
   - ALSA queue delay via `snd_pcm_delay`/avail → estimate output-side latency.
   - CPU%: audio-thread busy time / wall time.
   - xrun/recover count.
3. Optional if trivial: attempt `SCHED_FIFO` on the audio thread (log pass/fail; needs privileges).

### Success criteria

- Clean, glitch-free audio at 256-frame period (audible check — headphones on laptop)
- Event→sound path comfortably < ~15 ms perceived
- Clear verdict: hand-rolled ALSA callback + FluidSynth FFI is the architecture (or isn't)
- CPU datapoint (foreshadows Raspberry Pi viability; one instance should be a few % here)

### Notes / gotchas

- `fluid_synth_write_float` writes into caller-provided L/R buffers (offsets+strides).
- FluidSynth default master gain is 0.2 — raise via settings if too quiet for test.
- Keep the synth instance single-threaded (only touched from the audio thread); feed events through a lock-free/queue boundary. This mirrors the "single writer" state flow decided in scratchpad R5.
- If PipeWire's ALSA layer misbehaves at small periods, test `hw:` device directly and note the difference — Pi will run plain ALSA.

---

## 6. After the spike (ordered backlog)

1. ✅ **DONE** — Project file format (`crates/pocketstage-model`): clip-matrix schema
   (R2), serde JSON with explicit `version` gate (`SCHEMA_VERSION = 1`, future versions
   rejected not guessed), full validation invariants, `Project::default_project()` with
   R8 starter patches, 9 unit tests. CI added (`.github/workflows/ci.yml`:
   build + clippy -D warnings + fmt check + tests on push/PR).
2. MIDI I/O + hotplug (`pocketstage-midi`) — ✅ **CORE DONE**: port discovery,
   streaming parser, T1 `midi_probe` tool (verified live on the MKV), hotplug
   tracking (`ActivePorts` + pure `diff_ports`), engine event pump
   (`spawn_pump` → mpsc `PumpMessage`s; `midi_probe pump` smoke-tests it).
   15 tests, hardware tests mutex-serialized (rawmidi excludes concurrent openers).
   Deferred to when needed: rtrb lock-free ring for the audio-thread consumer,
   ALSA inotify instead of 1 s poll-scan.
   **T1 session ready to run whenever you are:**
   `cargo run -p pocketstage-midi --bin midi_probe -- watch | tee probe-$(date +%F).log`
   (put the MKV in a known mode first — HUI/template modes remap CCs)
3. Oxygen mapper + pad LED feedback (`pocketstage-oxygen`) — investigate Editor port SysEx for LEDs (task T2)
4. Web server (`pocketstage-server`): axum REST + WS, single-writer state snapshots
5. Minimal web UI: project load/save, take list, mixer

---

## 7. Decision log snapshot (full detail in scratchpad R7 + D10)

| # | Decision | Status |
|---|----------|--------|
| D1 | Controller = M-Audio Oxygen 49 MKV (verified connected, card 3 / client 28) | ✅ |
| D2 | Clip matrix model (track × scene); no separate LIVE/STUDIO modes | Proposed |
| D3 | Single FluidSynth instance; tracks = internal MIDI channels | Proposed |
| D4 | Record MIDI only in v0.1; offline WAV bounce | Proposed |
| D5 | Scene launch quantized to next bar (configurable) | Proposed |
| D6 | Rust workspace, crates per R5 layout | Proposed (scaffolded) |
| D7 | No footswitch; transport = Oxygen onboard buttons | ✅ |
| D8 | Curated defaults: FluidR3 GM base + GeneralUser GS upgrade + patch layer (R8) | ✅ |
| D9 | mDNS + no auth in v0.1 (LAN-trusted) | Proposed |
| D10 | Name = **PocketStage** | ✅ |

---

## 8. Open risks / watch-list

- **Pad LED control on MKV** — Editor port exists (good sign) but protocol unverified (task T2). Fallback: blink-free UX via transport buttons + audio cues.
- **MKV CC map unknown** — published tables unreliable; empirical probe required (T1, deferred by user).
- **FluidSynth crate bindings on crates.io are stale** — use hand-written FFI (spike will prove the approach).
- **Pi performance never measured** — spike CPU% is first datapoint; Pi hardware not yet acquired? (not discussed)

---

## 9. How to resume (suggested prompt for next session)

> Read HANDOFF.md and specs/brainstorming/scratchpad.md, then implement the audio
> engine spike described in section 5 of HANDOFF.md.
