# PocketStage — Handoff Document

> Created: 2026 – session handoff for continuing work after folder rename.
> Read this first, then read `specs/brainstorming/scratchpad.md` (the full design brainstorm).

**Status (session 8):** spike validated (0 xruns, ~1 % CPU) · model crate + CI ·
MIDI core (parser/hotplug/pump) · **oxygen input mapper DONE incl. hardware probes
A/B/C** — 47 tests green. Device ground truth: `specs/device-maps/oxygen-49-mkv.md`.
**T2 SysEx expedition DONE — inconclusive:** pad LEDs are local momentary state,
not host-controllable (no reply to any inquiry; notes on ports 0/2 don't move
them, both modes) → **LED-less UX fallback adopted**.
**Engine wiring DONE:** pump → per-port mappers → synth. Keys play the lead
voice live (verified with headphones), pads fire a kick placeholder,
`--demo` flag keeps the hardware-free arpeggio, `POCKETSTAGE_DEVICE` env var
overrides the ALSA device.
**Transport/clock DONE:** Play/Stop drive a real master clock in the engine —
sample-accurate playhead at project BPM (120 default), accented beat ticks
while playing, `transport=… pos=bar.beat` in stats. Live-verified.
**Scene launching DONE:** pads 1–8 launch scenes (R2) — from stopped it starts
playback immediately; while playing the switch lands on the scene's quantized
boundary (D5, per-scene override honored). Active scene's clip column plays as
looping 16th-gated step patterns (track N → channel N, D3); `scene=S#` in stats.
Boot project = `Project::default_project()` + demo patterns in scenes 1–2
(placeholder until project loading). Live-verified: all 8 pads, keys layering,
0 xruns. Pure clock math in `engine/src/clock.rs` (4 tests).
**Design pass (session 8):** mode system v2 + recording workflow decided —
D11–D16, full spec in scratchpad R11.
**Recording core DONE:** ⏺ captures keys into multi-channel takes on the
master clock (metronome fires ONLY while recording — D16); Stop finalizes
(wait for Play); Play loops takes at max(loop_bars, rounded-up take length).
Live-verified: record → stop → play back loop. `transport=… takes=N` in stats.
**T6 VERIFIED ✅ (session 8, attended):** DAW button = `cc113` pulse, PRESET
button = `cc112` pulse, both on the HUI port — two distinct edge signals, mode
is fully trackable (see device map §T6). **The R11 mode-gated layer is now
unblocked** (PRESET drums-pads / DAW selectors / record-stop-on-entry).
**Mode system v2 IMPLEMENTED (session 8):** PRESET = keys + fixed 8-hit kit on
pads (D12); DAW = pads→scenes, keys→patch browser with audition (D13), B-key
cycles loop length; entering DAW finalizes any recording (D15). Mode ships in
the WS frame (`"mode"`). **Same-key retrigger (D17):** striking a pad/key now
resets its previous voice instead of stacking — one channel per pad (8..=15)
+ All Sound Off; see lab findings in §8. Live-verified with headphones.
Remaining hardware work: pedal CC check (needs a pedal).

---

## 1. What PocketStage is (30-second summary)

A portable lite DAW running headless on Linux (Raspberry Pi or laptop). A MIDI
keyboard (**M-Audio Oxygen 49 MKV**) is the primary live controller; a web UI
(phone/tablet browser) is secondary for setup/management. All audio stays local
(the browser is a remote control, never an audio path). Pattern/clip-based
(Ableton-style clip matrix), FluidSynth + soundfonts, single Rust binary.

Full design: `specs/brainstorming/scratchpad.md`

---

## 2. Repo identity

Folder is `~/Projects/PocketStage`; remote **https://github.com/lalanikarim/PocketStage**
(public, MIT). Branch protection on main: PR required + linear history, admins
(currently: me) bypass to push directly. Always run the CI gates locally before pushing:
`cargo build && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`.

---

## 3. Current state of the code (session 11 wrap)

**R12 IMPLEMENTED (session 11):** D24 write-through lane → MIX overlay →
SEQ overlay, each test-first on the harness. Pads 9/10 inside PRESET toggle
MIX/SEQ (voice-confirmed kick/rim/tom — no LEDs), any mode switch exits.
MIX maps F1–F4→gain, K1–K4→pan, K5–K8→reverb of tracks 1–4; fader buttons
select a per-track detail page (K1–K4 = gain/pan/reverb/chorus). SEQ paints
steps of the active scene with bank-1 pads (pad 11 next bar, pad 12 flip
half-window), creating 1-bar clips on demand. Engine-originated edits
persist via a debounced (~500 ms) outbound snapshot lane — nothing flows
back in; `Track.root_note` added (additive, v1-safe). Overlay state rides
`LiveStatus.overlay` → `/ws` frame + stats line + UI live card. Follow-up:
`Project.master_gain` (0..=2, additive) → `fluid_synth_set_gain` at boot +
swap — Master slider in the mixer card turns the output up live, no
recompile; unset keeps the 0.3 boot default. See §10 "Session
11" and scratchpad R12.

**D25 IMPLEMENTED (session 11b): one voice at a time.** PRESET sounds
exactly ONE instrument. DAW mode is a white-key voice browser anchored at
C2: slot 0 (lowest white key) = Drums, the next 11 white keys = curated GM
patches; black keys are inert. A melodic voice plays across all 49 keys —
pad presses become melody notes too (pads transmit plain notes 36–51). The
Drums voice flips that: pads 1–16 hit the kit across both banks
(`drum_kit_2`, new additive field; bank-2 channels mirror bank 1), keybed
silent. **Unified note router (user rule: pads are NOT different from
notes):** the feeder reconstructs raw notes from Key AND Pad actions and
routes one stream — no pad special-casing anywhere. Hardware scene launching
is gone with it (scenes live in the web UI / HTTP); overlay enter/exit moved
to black-key command notes above the browser span: G#3 = MIX, A#3 = SEQ,
C#4 = next bar, D#4 = flip window (buttons only while an overlay lives or in
DAW mode; ordinary voice notes elsewhere). The
voice persists via `Project.active_voice` (write-through lane) and rides
`LiveStatus.active_voice` → `/ws` + stats line + UI "voice" cell.
Dead events removed: SetLeadProgram, CycleLoopBars (B-key loop-length
cycling is gone — loop bars are web-editable). 152 tests. See D25, §6 item
16, §10 Session 11b.

**A usable end-to-end loop exists**: boot → play keys/drum pads (PRESET) →
⏺ record takes → Stop → looping playback → overdub layers on top; DAW mode
launches scenes from pads and browses patches; a full web control plane at
`:7777` mirrors and drives everything live. **118 tests, all CI gates green.**
Session 10 closed the software-only backlog: device-free engine harness
(`engine/src/core.rs` — `SynthBackend` trait + call-recording fake driving
`EngineCore::period()` in a loop; main.rs keeps only ALSA/FFI/feeder),
take rename, global FX amount knobs, JSON drum kits, and the SEQ/MIX
design pass (scratchpad R12, D21–D24 proposed).

**Session 10c hotfix — output was hard-clipped into a square wave.** The
render path scaled FluidSynth's normalized float output by `i32::MAX` before
clamping to `i16`, so every sample above ≈−96 dBFS pinned to ±32767 (95–99% of
the waveform at normal velocities). Soft taps were sparse/partially clipped
("crisp"); harder hits became a dense square wave ("stacked/distorted"). Fixed
with `to_i16()` (`i16::MAX` scale + regression test); master gain is now 0.3
(the old 0.05 was an accidental side-effect of the saturation). See §10
"Session 10c" for measurements.

```
PocketStage/
├── Cargo.toml                          # workspace, 5 member crates
├── AGENTS.md                           # AI-agent guide — read before editing code
├── SPIKE_RESULTS.md                    # spike numbers + platform quirks log
├── specs/brainstorming/scratchpad.md   # design brainstorm D1–D10 + R-sections
├── specs/device-maps/oxygen-49-mkv.md  # GROUND TRUTH controller map (+ T6 evidence)
├── .github/workflows/ci.yml            # build + clippy -D warnings + fmt + tests
└── crates/
    ├── pocketstage-engine/   ALSA + hand-rolled FluidSynth FFI; live MIDI; embedded
    │                         server; core.rs = SynthBackend trait + EngineCore
    │                         (device-free integration harness); transport.rs =
    │                         PURE tested state machine; --demo mode; labs in
    │                         examples/ (retrigger, drum_chan);
    │                         ps-stats thread watchdogs ps-audio
    ├── pocketstage-model/    versioned project format SCHEMA_VERSION=1 (additive-
    │                         only policy: gain/pan/mute/reverb/chorus/take.muted)
    ├── pocketstage-midi/     port discovery, streaming parser, hotplug, spawn_pump
    ├── pocketstage-oxygen/   input mapper incl. Mode detection (cc113/cc112 HUI)
    └── pocketstage-server/   axum control plane: ProjectStore + project LIBRARY
                              (switch/save-as/delete/export), REST + /ws status,
                              EngineCommand lane, embedded single-file web UI
```

Key architectural facts proven so far:
- Audio: single-writer synth owned by the audio thread; mpsc event boundary; 0 xruns.
- **Render loop may NEVER panic** — all cross-lane data hostile by default (see 8f
  post-mortem); launch/scene logic extracted to `engine/src/transport.rs` (pure,
  total functions, 9 tests incl. the exact live-crash regressions).
- Web click == key press: HTTP transport/scene commands become ordinary Events
  through the same audio-thread drain.
- Project persistence: takes auto-persist via take-feed; library mode keeps
  `~/.pocketstage/projects/<name>.json`, engine hot-switches on load; legacy
  single file auto-migrated as "Default".
- Oxygen mapping: per-port mappers (music :0, HUI :1); DAW/PRESET buttons =
  cc113/cc112 pulses on HUI port only (T6 verified) → mode fully trackable.

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

Audio routing note (dev box): the engine's `default` ALSA device resolves to
the **CX1970** USB interface and PipeWire links it *directly* to its endpoints,
bypassing WirePlumber sink nodes — so the OS master volume slider does not
scale it (per-app stream volume via GNOME/`wpctl set-volume <stream> <v>`
does). To route through the default sink (system slider works), open the sink's
node name as the device: `POCKETSTAGE_DEVICE=alsa_output.pci-0000_04_00.6.HiFi__Speaker__sink`
(verified working, session 8). Sink node ids churn on WirePlumber restarts —
look them up with `wpctl status` / `wpctl inspect <id>`.

Useful device-check commands:

```bash
aconnect -l                                  # list ALSA sequencer clients/ports
amidi -p hw:3,0 -d                           # dump raw MIDI from Oxygen (Ctrl+C stop)
aplay -l                                     # playback devices
wpctl status                                 # sinks/streams; find the engine's stream id
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
   13 tests, hardware tests mutex-serialized (rawmidi excludes concurrent openers).
   Deferred to when needed: rtrb lock-free ring for the audio-thread consumer,
   ALSA inotify instead of 1 s poll-scan.
   **T1 capture DONE** (+ probes A/B/C; all distilled into the device-map file —
   raw logs lived in /tmp and are ephemeral). Headlines: controls on **ch13**;
   knobs CC[44,45,46,47,62,63,75,76] · faders CC33-41 · buttons CC49-56 + #9
   colliding with CC41; pads = notes 36–51 (two banks → logical 1–16); transport via
   HUI-port notes (play/record indistinguishable otherwise); octave/program buttons
   emit nothing; device streams MIDI clock ~45 msg/s nonstop — mapper discards it.
   Corrected inventory: **9 faders+buttons, 8 knobs, 8 physical pads** (2×4 rows).
3. Oxygen mapper (`pocketstage-oxygen`) — **part 1 DONE** (input mapping, probes
   A/B/C baked in): `Mapping` (serde, deny_unknown_fields, validated; pad window
   36–51 = logical pads 1–16 via banks; knob order [44,45,46,47,62,63,75,76];
   shared-button faders [9]) + stateful `Mapper::map(&Event, &mut Vec<Action>)`
   (tap-detector; one Mapper **per source port** — see oxygen_demo). 24 tests.
   **Engine wiring ✅ DONE (session 8)** — pump → per-port mappers → engine:
   keys→lead voice (live-verified), pads→kick placeholder. `--demo` = scripted
   arpeggio (no hardware); device selectable via arg or `POCKETSTAGE_DEVICE`.
   **Transport/clock ✅ DONE** — Play/Stop drive a sample-accurate master clock
   (`clock.rs`, pure + tested): accented beat ticks while playing, position in
   stats, live-verified.
   **Scene launching ✅ DONE** — pads 1–8 launch scenes: immediate from
   stopped, D5-quantized while playing (per-scene override honored); active
   column loops as 16th-gated step patterns (track N → channel N). Demo
   patterns in scenes 1–2 of the boot project. Live-verified all 8 pads +
   keys, 0 xruns. **Next:** recording slice (item 4).
   **T2 DONE (session 8), inconclusive**: LEDs are local momentary state;
   Editor port silent both ways; no host-controllable LED protocol found →
   LED-less UX fallback adopted. Part 2 (LED feedback) is therefore **dropped
   unless** M-Audio/community SysEx docs surface later.
   **Remaining hardware tasks**: pedal CC verify when a pedal exists.
4. **Recording slice** (scratchpad R11, D11–D16) — **core DONE (session 8)**:
   model prep ✅ (`Track::channel` override, `loop_bars`, take helpers — commit
   1398123); engine capture/playback ✅ (⏺ → tick-stamped multi-channel takes,
   metronome only while recording, Stop finalizes + waits for Play, loop =
   max(loop_bars, rounded-up take); live-verified). **Remaining:** the
   mode-gated layer — PRESET drum-kit pads (D12) + DAW note-selectors with
   audition (D13) + record-stop on entering DAW (D15). T6 ✅ VERIFIED —
   cc113/cc112 pulses on the HUI port detect mode switches unambiguously;
   implement next.
5. Web server (`pocketstage-server`) — **slices 1+2 DONE (session 8)**: axum
   REST crate + `ProjectStore`; **slice 2: router now embedded in the engine**
   (`spawn_engine_server`, own thread + tokio runtime, engine stays async-free).
   PUT → store-validated → `EngineCommand::SetProject` on an mpsc lane the
   audio-thread drain loop applies at its safe point (single writer intact, R5;
   swap+dealloc in audio thread is a documented, user-initiated exception).
   Engine boots its project from `POCKETSTAGE_PROJECT` (default
   `~/.pocketstage/project.json`) — boot-project placeholder killed; demo
   content only stamped on a brand-new project and persisted immediately.
   Stats show `swaps=N`. Verified live: PUT → 204, swaps=1, bpm change audible,
   0 xruns. **Slice 3 DONE:** `GET /ws` WebSocket pushes a status frame every
   250 ms (`{type,bpm,transport,bar,beat,scene,takes}`) from `LiveStatus`
   atomics shared with the audio thread (plain stores on the audio path — no
   locks/alloc); finalized takes stream back over a take-feed channel and are
   appended to the stored project → **recordings survive reboot** (engine seeds
   its live takes from the file at boot). Verified: demo session's Take 1 (64
   events) landed in the JSON; after restart WS immediately shows it.
   **Slice 4 DONE (session 8): minimal web UI** — `GET /` serves a single
   embedded file (`crates/pocketstage-server/web/index.html`, vanilla JS, no
   asset pipeline): live transport/bpm/bar:beat/scene/mode/takes from /ws
   (auto-reconnect), recorded-take list (re-fetched when the take count
   changes), project editor (name/bpm/loop bars + raw JSON escape hatch) via
   GET/PUT /api/project. Mixer view deferred until FX exist.
6. **Mixer slice DONE (session 8)**: `Track.gain/pan/mute` (additive optional
   fields — schema policy kept at v1), engine applies CC7/CC10 per track at
   boot + every project swap (`apply_track_voice`); web UI gains a Mixer card
   (per-track gain/pan sliders + mute, dirty-flag Save). No new endpoints —
   mixer edits ride PUT /api/project. Live-verified: PUT → 204, swaps=1,
   xruns=0.
7. **Take management DONE (session 8)**: `Take.muted` (additive, v1-safe) —
   engine skips muted takes on replay; project swaps adopt incoming take
   state (mute flags + deletions) while keeping this boot's un-persisted
   takes until the store absorbs them (swap-vs-drain race). Web takes list:
   mute checkbox (dirty-flag Save) + ✕ delete (confirm → immediate PUT).
8. **Remote transport DONE (session 8)**: POST /api/transport/play|stop|
   record + /api/scene/{1..8} push EngineCommands; the audio thread maps
   them onto ordinary Events and they run through the SAME drain as
   hardware (web click == key press, one code path). UI live card gained
   ▶ ⏹ ⏺ + S1–S8 chips.
8b. **Clip matrix DONE (session 8)**: tracks×scenes grid in the web UI;
    click to create/select clips, step editor (paint pitch, 1/2/4 bars,
    delete) edits patterns via the dirty-flag PUT path; refresh() now
    refuses to clobber unsaved edits.
8c. **FX sends DONE (session 8)**: Track.reverb/.chorus (0..=1, None =
    soundfont default) → CC91/CC93 in apply_track_voice; mixer sliders.
8d. **Structure CRUD DONE (session 8)**: rename/GM-program/delete/add
    tracks (≤4), rename the 8 scene columns; engine clamps active AND
    pending-quantized scene indices when a PUT shrinks the scene list
    (caught live: pending S5 used to fire after an 8→3 shrink).
8e. **Clear-all takes DONE (session 8)**: web button + time-bounded
    `mine` keep-rule (30 s) so cleared takes can't resurrect on swap.
    Remaining web items: overdub UX pass, take rename/audition.
    ⚠ Dev quirk: rebuilding while the engine runs can leave the OLD
    binary in place (ETXTBSY under cargo's copy) — pkill before
    `cargo build --release`, restart after.
8f. **Post-mortem (session 8)**: "no sound after DAW→PRESET" was NOT a
    mode bug — LaunchScene indexed scenes unguarded; my 3-scene shrink
    test + pad 5 panicked ps-audio while the process kept serving WS
    (silent-but-alive = nasty). Fixed: get()-guards + stats-thread
    watchdog that screams when ps-audio dies. Rule: the render loop
    may NEVER panic; treat all cross-lane data as hostile.
9. **Transport extraction DONE (session 9)**: launch/quantize/clamp/step-window
   logic moved from the render closure to `engine/src/transport.rs` — pure,
   total, 9 tests covering the live-crash cases; fixed a u64 k−1 underflow
   (launch starting the clock mid-period). apply_scene_voice() dedups.
10. **D18 Overdub DONE (session 9)**: takes keep looping while a NEW take
    records (capture only sees the input lane — no feedback). Tick base =
    press moment; start layers on the downbeat for alignment.
11. **Web UX DONE (session 9)**: active-scene column/chip highlight from
    /ws; beforeunload guard while dirty; hotkeys Space=play/stop, R=record;
    per-take SOLO button (mute-flag solo, immediate PUT).
12. **Project library DONE (session 9)**: multi-project store + switch/
    save-as/delete/export routes and UI card; engine boots the library
    (~/.pocketstage/projects, most recent = current), POCKETSTAGE_PROJECT
    still pins a single file for tests/CI. Live E2E incl. 409-on-current.
13. **Software-only sweep DONE (session 10)**: engine integration harness ✅
    (`core.rs` SynthBackend trait + fake; 21 harness tests covering launches,
    quantize timing at bpm 125, record/overdub lanes, D15/D17/D18 semantics,
    swap adoption, scene-shrink clamp, HTTP lane); take rename ✅
    (POST /api/takes/rename + `EngineCommand::RenameTake` keeps the live copy
    and the mine-rule in step — no duplicate-after-swap; ✎ in UI); global FX
    amount knobs ✅ (`Project.fx_reverb`/`fx_chorus` 0..=1 → fluid_synth_set_
    reverb/chorus at boot + swap, "Global FX" row in mixer card); JSON drum
    kits ✅ (`Project.drum_kit [u8;8]`, GM 35–81, lock-free AtomicU64 handoff
    to the pad feeder, kit editor card). SEQ/MIX design pass ✅ — scratchpad
    **R12**, decisions D21–D24 (proposed): pads 9/10 overlay toggles, MIX =
    write-through CC mapping, SEQ = bank-1 step window programmer, shared
    `ProjectEdit` debounce lane. Build order when picked up: D24 lane → MIX
    → SEQ, test-first on the harness.
14. **R12 IMPLEMENTED (session 11)** ✅: D24 write-through lane → MIX overlay
    (D22) → SEQ overlay (D23), in the prescribed order, all harness-tested.
    `/ws` + stats + UI surface `overlay` (none|mix|seq); live CC values not
    yet in the WS frame (open nicety). **Next software candidates:** fader
    *ramp* smoothing measurement, pitch-per-step editing in SEQ (deferred
    in R12), MIX detail-page layout revisit if the hardware feel disagrees
    with the K1–K4 = gain/pan/reverb/chorus reading. Hardware-gated: ear-
    checks of sessions 8/9/10/11 features (overlay toggles, mix rides, step
    painting), pedal probe, T2 round 2, Pi validation + deployment kit
    (user deferred).
15. **Web UX: mixer edits should apply immediately** (user report, session
    11): volume/gain sliders in the web UI only take effect after an
    explicit Save click — dragging should be audible live. Sketch: debounced
    auto-save on slider release (or a lightweight per-control PATCH-style
    lane); mind the dirty-flag guard (refresh() must not clobber unsaved
    edits) and don't PUT per mousemove event. Todo, software-only.
16. **One voice at a time + white-key browser — IMPLEMENTED (session 11b,
    D25)** ✅: exactly one active voice in PRESET; DAW white-key selection;
    kit spans both pad banks in Drums; melodic pads play the voice. Layering
    workflow under strict one-voice: record a drums take → switch voice to
    piano → overdub (capture is lane-based, cross-take multitracking works).
    **Hardware ear-checks owed:** white-key select/audition feel, melodic pad
    presses (must play melody notes), 16-hit kit bank 2, keybed silence in
    Drums, black-key commands (G#3/A#3 enter/exit overlays in both modes,
    C#4/D#4 SEQ bar/window while live), hardware scene launch REMOVED (web
    UI / HTTP only), voice persistence across reboot.

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
| D7 | Transport = Oxygen onboard buttons (a sustain pedal exists but was never on hand; probe deferred, expect CC64) | ✅ |
| D8 | Curated defaults: FluidR3 GM base + GeneralUser GS upgrade + patch layer (R8) | ✅ |
| D9 | mDNS + no auth in v0.1 (LAN-trusted) | Proposed |
| D10 | Name = **PocketStage** | ✅ |
| D11 | **Mode system v2**: unit's DAW/PRESET switch = manage/perform split (full spec scratchpad R11); supersedes PLAY/SEQ/MIX trim for v0.1 — SEQ/MIX deferred | ✅ |
| D12 | Pads bank 1 = fixed 8-hit GM kit (PRESET mode); low-key shadowing accepted — one performance voice at a time | ✅ |
| D13 | DAW mode: every note is a patch selector + audition; slot = pitch % 12 (octave ignored); full keyboard = browser; settings keys override specific letters (B = loop length) | ✅ |
| D14 | Fader buttons reserved as channel selectors (MIX era); never scenes/instrument select. Program ‹ › / octave buttons emit no MIDI | ✅ |
| D15 | Entering DAW mode stops recording (finalizes take), NEVER touches playback; transport owns playback | ✅ |
| D16 | Recording = multi-channel Take capture; `loop_bars` project setting; Stop → wait for Play; metronome only while armed/recording, never captured | ✅ |
| D17 | Hardware-drum retrigger feel: same key/pad struck again RESETS its previous voice (different keys stay polyphonic). Pads own private channels 8..=15 (metronome shares ch8 with kick); kill = per-channel All Sound Off — one-shot GM voices ignore note-off AND live GEN_ATTENUATION edits (lab: `engine/examples/retrigger_lab.rs`) | ✅ |
| D18 | **Overdub layering**: takes keep looping while a new take records; capture is input-lane-only so no feedback. Tick base = record-press moment (align by starting layers on the downbeat) | ✅ |
| D19 | **Project library**: `~/.pocketstage/projects/*.json`, current entry switchable at runtime (load → SetProject hot-swap); save-as = atomic snapshot (no switch); delete refuses current (409); POCKETSTAGE_PROJECT pins single-file mode for tests. Legacy project.json migrates in as "Default" | ✅ |
| D20 | **Render-loop law**: the audio thread may never panic and never trusts cross-lane indices — get()/is_some_and over indexing; state machines live in pure tested modules (transport.rs pattern); ps-stats watchdogs ps-audio | ✅ |
| D21 | **Overlay entry** (R12): SEQ/MIX are software overlays inside PRESET (the Play/Seq/Mix trims emit nothing). Logical pad 9 toggles MIX, pad 10 toggles SEQ; mutually exclusive; any mode switch exits both; voice-confirmed (no LEDs) | ✅ Implemented (s11) |
| D22 | **MIX overlay** (R12): F1–F4→track gain, K1–K4→pan, K5–K8→reverb send; fader buttons = per-track detail select (D14); write-through to project with ~500 ms debounced persist. Detail-page layout chosen at implementation: K1–K4 = gain/pan/reverb/chorus of the selected track | ✅ Implemented (s11) |
| D23 | **SEQ overlay** (R12): live step programmer — bank-1 pads 1–8 toggle steps of an 8-step window on the selected track; bank 2 keeps commands (pad 10 exit, pad 11 next bar, pad 12 flip half-window); edits quantize to the engine's 16th grid | ✅ Implemented (s11) |
| D24 | **Write-through lane** (R12): engine-originated project edits flow out on a dedicated mpsc to the store (debounced), never back in — single writer intact; engine applies edits locally first. Debounce is sample-counted in the engine; the server drain coalesces bursts to one persist. First rider: CycleLoopBars — B-key survives reboot now (event itself removed by D25) | ✅ Implemented (s11) |
| D25 | **One voice at a time** (session 11 feedback; supersedes D12's always-on kit layer + D13's pitch%12 browser): PRESET has exactly ONE active performance voice. Selection = DAW-mode white-key browser anchored at C2 — slot 0 is Drums, next 11 white keys are the curated GM patches; black keys carry only overlay commands. Melodic voice: all 49 keys play it and pad presses are melody notes too (pads transmit plain notes 36–51, nothing to disambiguate). Drums voice: notes 36–51 = kit across both banks (`drum_kit_2` additive field; bank-2 channels mirror bank 1 — same kill group, documented trade-off), other notes silent. Overlay enter/exit = black-key command notes (G#3 MIX, A#3 SEQ; +C#4 next bar, D#4 flip while SEQ lives), armed in either mode — a follow-up to the user's unified-note rule replaced the earlier "DAW pads 9/10" scheme and REMOVED hardware scene launching (web UI / HTTP only): with one note stream, notes 36–51 cannot be both instrument and buttons. Voice persists via `Project.active_voice` + write-through; mirrored on `LiveStatus.active_voice` (VOICE_DRUMS sentinel) → /ws + stats + UI | ✅ Implemented (s11b), ear-checks owed |

---

## 8. Open risks / watch-list

- **Float→i16 conversion is load-bearing.** `fluid_synth_write_float` emits
  normalized −1.0..=1.0; scale by `i16::MAX`, **never** `i32::MAX` — the latter
  puts almost every sample above the i16 clamp and turns the output into a
  square wave. Session 10c post-mortem below; regression test
  `float_to_i16_is_linear_until_true_full_scale`. Any future render-path change
  must include an end-to-end conversion test, not just a synth-float peak check.

- **FluidSynth one-shot voices are unkillable by note-off or live gen edits**
  (session 8 lab): FluidR3 percussion ignores note-off (plays to natural end),
  `fluid_voice_gen_set(GEN_ATTENUATION)` + `update_param` writes the value but
  never reaches the mixer, and `fluid_voice_get_id` returns 0 for all voices.
  **Only CC120 All Sound Off kills instantly — per CHANNEL**, hence D17's
  one-channel-per-pad layout. Old takes recorded before D17 sit on ch9 (now
  pad 2's channel): they still sound right (drum pitch follows the note), only
  their retrigger grouping is off; re-record to migrate.
- **`fluid_synth_set_channel_type` does not re-resolve presets** (session 8
  lab `engine/examples/drum_chan_lab.rs`): the channel keeps playing its
  melodic default until `bank_select(128)` + `program_change(0)` force it onto
  the GM kit. Engine boot does all three per pad channel.
- **rawmidi subdevices are single-reader**: while the engine runs, no other
  process can open the Oxygen ports (`active_ports` test asserts ⊆ present,
  not ==, because of this). Stop the engine before running device probes.

- ~~**Pad LED control on MKV**~~ — **T2 done (session 8), inconclusive**: LEDs are
  local momentary state (off-while-pressed), Editor port silent to all inquiries,
  incoming notes don't move them in either mode. **LED-less UX fallback adopted**
  (transport buttons + audio cues). Revisit only if protocol docs surface.
- ~~MKV CC map unknown~~ — **resolved** by T1 + probes A/B/C; ground truth in `specs/device-maps/oxygen-49-mkv.md`.
- ~~FluidSynth crate bindings stale~~ — resolved: hand-written FFI works (spike-proven).
- **Pi performance never measured** — spike CPU% is first datapoint; Pi hardware not yet acquired? (not discussed)
- **Unit state drifts**: preset mode + channel 13 + bank 1 are assumed defaults; the
  user can change them on the unit. `control_any_channel` and JSON mapping overrides
  exist for this — surface a "learn/mismatch" warning in the UI someday.
  **Concrete incident (session 8):** unit left at octave −2 shifted the keyboard
  into the pad window (36–51) → middle keys silently became pad presses; diagnosed
  via `midi_probe watch` capture of a full white-key sweep. Operational rule: if
  keys go missing, check the unit's octave/preset state first. Note the lowest 16
  keys (C2–G3 at oct 0) are *always* pad-shadowed by design; making the pad window
  configurable is the open fix if that ever matters.
- ~~**Mode detection unverified (T6, BLOCKING for record slice)**~~ —
  **RESOLVED session 8:** attended capture confirmed DAW button → `cc ch0
  cc113 val127→0` pulse, PRESET button → `cc ch0 cc112 val127→0` pulse, HUI
  port only, perfect lockstep over 4 toggles, no port-open bursts (earlier
  "burst" was echo of real presses). Initial mode still unknown at boot —
  assume PRESET; self-heals on first toggle. Full table in device map §T6.
- **Fader 9 button collision** — heuristic (bare 127→0) could misfire only if someone
  moves that fader top-to-bottom between two MIDI samples; effectively impossible at
  USB rates, but it's a known theoretical edge.

---

## 9. How to resume (suggested prompt for next session)

> Read HANDOFF.md (§3 state, §6 backlog items 14–16, §7 decisions D21–D25)
> and AGENTS.md. Session 11b implemented D25 (one voice at a time: DAW
> white-key browser with Drums on the lowest C, melodic pads play the voice,
> kit spans both pad banks in Drums mode, overlay entry moved to DAW pads
> 9/10, active_voice persisted) — 152 tests, all gates green. Next software
> candidate: §6 item 15 (web mixer edits should apply live without Save).
> Everything else is hardware-gated: ear-check lists in §6 items 14 + 16
> (sessions 8–11 features), pedal probe, Pi validation. Software niceties:
> live CC values in the /ws frame, fader ramp smoothing, pitch-per-step SEQ
> editing (deferred). Live env: engine runs detached on :7777, project
> library in ~/.pocketstage/projects (test artifacts: "Gig" snapshot,
> "UI Test Clip" in S1, reverb 0.5 on track 1 — user may want to clean).

---

## 10. Session log

- **Session 1**: brainstorm → scratchpad; workspace scaffolded.
- **Session 2**: ALSA + hand-rolled FluidSynth FFI spike; validated 0 xruns / ~1 % CPU;
  SPIKE_RESULTS.md; MIT license (SPDX text) + GitHub Actions CI green.
- **Session 3**: `pocketstage-model` (versioned clip-matrix format, 9 tests); repo made
  public; branch protection on main; edition 2024 migration.
- **Session 4**: AGENTS.md; `pocketstage-midi` core — port discovery, streaming parser
  (15 tests), `midi_probe list/watch/pump/send`, hotplug (`ActivePorts` + `diff_ports`),
  engine event pump (`spawn_pump` → mpsc) wired into demo main.
- **Session 5**: T1 hardware capture ×2 (first accidentally in DAW mode — became useful
  routing evidence). Inventory corrections (9 faders, 8 pads) + guide errata. Wrote
  `specs/device-maps/oxygen-49-mkv.md`. Inputs for the mapper: transport = system
  real-time messages; controls ch13; non-contiguous knob CCs; discard TimingClock spam.
- **Session 6**: `pocketstage-oxygen` **part 1 (input mapper)** — pure `Mapping`
  (serde + `deny_unknown_fields`, validated for ranges/duplicate CC claims),
  `Mapper::map(&Event) -> Option<Action>` built on the measured T1 map, 16 unit
  tests + doctest, `oxygen_demo` bin verified live against the unit (clock spam
  correctly silent). Pads = notes 36–43 shadow low keys by design; documented.
- **Session 7**: probes A/B/C on the unit (three recording runs; one wasted on
  accidental DAW mode — still produced routing evidence). Findings baked into
  `pocketstage-oxygen`: pad banks = notes 36–51 → logical pads 1–16 (mapper
  `pad_count: 16`); transport needs the HUI port (loop emits nothing else;
  record ≡ play on port 0) → `transport_notes`/`hui_mapping()` + per-port mappers
  in oxygen_demo; fader button 9 collides with fader 9's CC41 → buffering
  tap-detector (`FaderButtonTap`). 24 oxygen tests (47 workspace-wide), all gates
  green, demo verified live. Pedal probe deferred (no hardware).
- **Session 8**: T2 SysEx expedition (attended, one-at-a-time with user watching
  LEDs). Result: **inconclusive** — pad LEDs are local momentary state (all lit,
  off-while-pressed); Note On/Off via ports 0 and 2 don't move them in Preset or
  DAW mode; Editor port replies to nothing (identify, M-Audio 0x43 guesses,
  dump requests). LED-less UX fallback adopted; part 2 (LED feedback) dropped.
  Side confirmation: a top-row pad press emitted key 40, exactly matching the
  device map (bank 1: bottom row 36–39, top row 40–43) — no state drift.
  Findings in the device-map file.
  Then **engine wiring**: `pocketstage-engine` now consumes `spawn_pump` →
  per-port Oxygen mappers (music + HUI) → synth events. Keys drive the lead
  voice (live-verified with headphones through the CX1970), pads fire a kick
  placeholder, transport prints state. Fixed two real bugs found during live
  testing: `PumpHandle` was dropped immediately (its Drop shuts the pump down —
  process exited silently) and an audio-thread panic left the binary limping
  (now exits the process). Added `--demo` mode (scripted arpeggio, no hardware)
  and `POCKETSTAGE_DEVICE`. Master gain set to 0.05 for live levels. 47 tests,
  all gates green.
  Then **transport/clock**: Play/Stop now drive a real master clock in the
  engine — sample-accurate playhead (`engine/src/clock.rs`, pure math, 3 new
  tests), accented kick tick on each beat boundary while playing (downbeat
  louder), `transport=… pos=bar.beat` in the stats line. Demo mode starts the
  clock too (scripted kicks removed — the clock owns the drum now). Live-
  verified: ticks at 120 BPM on play, position counting bars, reset on stop,
  keys layering over ticks, 0 xruns. 50 tests, all gates green.
  Then **scene launching**: pads 1–8 now launch scenes — immediate from
  stopped (starts playback), D5-quantized while playing (per-scene `Quantize`
  override honored: immediate/next-beat/next-bar). The active scene's clip
  column loops as 16th-gated step patterns, track N → internal channel N
  (D3); scene switch silences old voices via `fluid_synth_all_notes_off` (new
  FFI decl) and re-arms programs. Boot project = `Project::default_project()`
  with demo patterns in scenes 1–2; keys now play track 1's instrument
  (Acoustic Grand, not the old saw lead). `--demo` auto-launches scene 1.
  Live incident: unit left at octave −2 put the keyboard inside the pad
  window — middle keys became pad presses (silence); diagnosed by capturing a
  full white-key sweep with `midi_probe watch`. All 8 pads + keys verified,
  0 xruns. 51 tests, all gates green.
  Then a **design pass** (no code): mode system v2 + recording workflow —
  decisions D11–D16, full spec scratchpad R11. Headline: the unit's DAW/PRESET
  switch becomes the manage/perform split (PRESET = keys + 8-hit drum-kit pads
  + record; DAW = every note a patch selector w/ audition via pitch % 12,
  bank-1 pads launch scenes, B sets loop length); takes for recording
  (multi-channel, `loop_bars` configurable, wait-for-play after stop);
  metronome only while recording. Fader buttons stay reserved (channel
  selectors). Key discovery: one T2 capture shows the DAW toggle emitting
  cc113 on the HUI port → mode is detectable; verification deferred as **T6,
  blocking** for the mode layer.
- **Web server slice 1**: new crate `pocketstage-server` (axum 0.8). Surface:
  `ProjectStore` (open-or-default load through the model's version gate,
  validated PUTs persisted atomically tmp+rename, poisoned-mutex recovery),
  routes `GET /api/health`, `GET/PUT /api/project`, standalone binary
  (`PORT`=7777, `POCKETSTAGE_PROJECT` file path). Deliberately *not* wired to
  the engine yet — single-writer R5 means embedding happens via a command
  channel in slice 2; this slice keeps the seam (the store) testable alone.
  Smoke-tested live: health OK, default project served, invalid `loop_bars=99`
  → 400 with reason, valid PUT → 204 + on-disk JSON. 4 new tests (60 total),
  all gates green. Bug found via the live server: first save into a fresh
  `~/.pocketstage` failed ENOENT → `create_dir_all(parent)` before the atomic
  write (+ regression test).
- **Web server slice 2 (engine embedding)**: `EngineCommand` enum +
  `spawn_engine_server` (dedicated thread, own tokio runtime — engine stays
  async-free; bind errors surface to caller so a port clash can't kill audio).
  PUT persists first, then commands the engine; the audio-thread drain loop
  applies `SetProject` at its safe point: swap project, re-read bpm, re-apply
  track programs (scene/pattern data is read at launch time → new content
  goes live on next scene launch). Single writer intact — server never touches
  engine state directly. Engine now boots from the store
  (`POCKETSTAGE_PROJECT` > `~/.pocketstage/project.json`); demo content only
  on fresh projects and persisted immediately so GET matches live from t=0.
  Stats: `swaps=N`. Live E2E: PUT rename+bpm → 204, swaps=1, disk updated,
  0 xruns. +1 test (engine-channel forwarding; invalid PUTs never reach the
  engine). 61 tests total.
- **Web server slice 3 (WS snapshots + take persistence)**: `LiveStatus`
  atomics moved behind the crate boundary — Stats now holds `Arc<LiveStatus>`
  shared with the server; `/ws` pushes `{type:"status",bpm,transport,bar,beat,
  scene,takes}` every 250 ms (disconnect-aware). Finalized takes: audio thread
  `send`s a clone on Stop → tokio-side drain task (250 ms tick) appends to the
  store's project → atomic file write. Engine seeds live takes from the saved
  project at boot, so recordings survive reboot (verified: WS shows Take 1 on a
  fresh start; 64 events persisted). +1 test (`drain_takes`). 62 total.
  Then slices A+B (unblocked parts of the recording design): **A — model prep**
  (`Track::channel` override, `loop_bars` floor + validation, `Take::loop_ticks`
  / `events_at` / sorted-invariant, `RecordedEvent::tick()`; +5 tests) and
  **B — engine take capture & playback**: ⏺ (HUI note 95 → `StartRecording`)
  captures key events into tick-stamped multi-channel takes inside the audio
  thread (clock + event stream share one owner — stamping needs no atomics);
  metronome now fires ONLY while recording (D16); Stop finalizes and waits for
  Play; Play loops takes at max(loop_bars, rounded-up take length) under live
  keys; stats show `transport=… takes=N`; `--demo` scripts a full
  record→stop→playback session. Live-verified with headphones (record → stop →
  loop). 56 tests, all gates green.
- **Housekeeping note (session 5)**: some earlier HANDOFF edits were found missing
  (prose edits did not persist; cause unknown). This file was reconstructed from git +
  notes. When prose and git disagree, trust git.
- **Session 8 (mega)**: T6 mode-detection verified (cc113/cc112 HUI pulses);
  mode system v2 live (PRESET perform / DAW manage, D11–D15); take capture &
  looping playback (D16); web server slices 1–4 (store → embedded router →
  WS status → single-file UI); per-track mixer + FX sends; take management
  (mute/delete/clear-all with time-bounded keep-rule); remote transport &
  scene launch over HTTP (web click == key press); clip matrix + step editor;
  structure CRUD; D17 retrigger via per-pad channels + CC120 (labs kept in
  engine/examples). Two live bug hunts: scenes-OOB panic killing ps-audio
  silently (→ get()-guards + watchdog, D20) and stale-binary rebuilds
  (ETXTBSY — pkill before release builds).
- **Session 9**: transport state machine extracted to pure `transport.rs`
  (9 tests incl. crash regressions; fixed mid-period-launch u64 underflow);
  D18 overdub layering; web polish (scene highlight, dirty guard, Space/R
  hotkeys); take SOLO button; D19 project library (multi-project switch/
  save-as/delete/export + legacy migration, live E2E). 84 tests, gates green.
- **Session 10**: software-only sweep of backlog 13. Engine integration
  harness: render-loop logic extracted to `engine/src/core.rs` behind a
  `SynthBackend` trait (FluidBackend = FFI passthrough; fake records calls),
  `EngineCore::period()` driven device-free in CI — 21 harness tests incl.
  exact quantize-boundary timing (bpm 125 ⇒ whole periods per beat) and the
  live-crash regressions through the full pipeline. Take rename end to end
  (store-first, `EngineCommand::RenameTake`, ✎ button; duplicate-after-swap
  regression covered — adoption keys on names so both copies must move).
  Global FX amounts (`fx_reverb`/`fx_chorus`, additive v1-safe) applied at
  boot + swap via `fluid_synth_set_reverb/chorus`; None = documented engine
  defaults restore the stock sound. JSON drum kits (`drum_kit [u8;8]`, GM
  35–81) with a lock-free packed-AtomicU64 handoff to the MIDI feeder (the
  harness caught a pre-swap publish-ordering bug mid-extraction). Web: FX
  row + kit editor card + ✎. SEQ/MIX design pass → scratchpad R12, D21–D24
  proposed. 113 tests, all gates green. Live ear-check still owed by human
  ears (§6 item 14).
- **Session 10b (hotfix): "held keys sound stacked" post-mortem.** User
  report: sustained notes uncrisp vs taps — a session-10 regression.
  `examples/fx_bench.rs` (headless FFI render lab; tail/sustain energy +
  sample-distance metrics) pinned TWO independent mistakes: (a) the hand-
  written `fluid_synth_set_chorus` FFI declared five args in the wrong order
  (real: `(synth, nr, level, speed, depth_ms, type)`) → boot ran chorus with
  clamped-to-max speed + garbage `type` — but FluidR3 piano/organ/strings
  have ZERO chorus send, so it was inaudible on the complained-about sound
  (Lead Synth 81 is the probe preset that hears chorus); (b) the real
  culprit: session-10 boot unconditionally overwrote reverb space params
  (`set_reverb(0.5, 0.4, 0.7, v)` vs this build's stock room=0.2/damp=0/
  width=0.5) — more than double the tail; held notes pile onto their own
  reverb. New policy: FX knobs are LEVEL-ONLY and OPT-IN — `fx_* = None`
  touches nothing at boot/swap (per-group *level* setters + `chorus_on/
  reverb_on` only); regression proof: fixed engine renders bit-identical to
  stock. Side discoveries: bool-typed settings have no public setter in
  2.3.4; `Settings` must outlive `Synth` (dangling-pointer segfault). The
  session-10 claim "None = documented engine defaults restore the stock
  sound" was itself wrong — retracted by this entry.
- **Session 10c (hotfix): velocity-dependent distortion post-mortem.** User
  report across sessions: soft key hits sounded crisp, harder hits sounded
  distorted / like notes were stacking, and it persisted at low system volume.
  Chatter guard, reverb/chorus and MIDI input had already been ruled out
  (events were 1–3 per hit; `fx_bench` proved FX-clean; the high-velocity
  FluidR3 piano layer is *bit-exactly* the low-velocity waveform scaled —
  normalized rel-diff 0.0000). Root cause was in `EngineCore::period`'s float
  → S16LE conversion:

  ```rust
  // WRONG — threshold ≈ 1/65536 ≈ −96.3 dBFS:
  let l = (left[i] * i32::MAX as f32)
      .clamp(i16::MIN as f32, i16::MAX as f32);
  ```

  FluidSynth floats are normalized −1..=1. Multiplying by `i32::MAX` before
  clamping to `i16` meant essentially every sample saturated: measured on the
  live Grand Piano, velocity 35 clipped **95.1%** of samples, velocity 64
  **98.5%**, velocity 90 **99.2%**, velocity 127 **99.5%** (velocity 5: 1.0%).
  At normal playing levels the engine was emitting a square wave, not the
  synth waveform. Very soft taps only partially crossed the threshold, hence
  "crisp"; harder hits crossed everywhere, hence the buzz/stacking illusion.

  Why it survived so long: prior checks inspected the *float* signal (peak
  only 1.8% FS at velocity 127) and assumed the clamp only engaged above 1.0;
  nobody tested the actual i16 output. The fix is a helper with the correct
  scale plus a boundary regression test:

  ```rust
  fn to_i16(x: f32) -> i16 {
      (x * i16::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32) as i16
  }
  ```

  `float_to_i16_is_linear_until_true_full_scale` asserts 0.5 → 16383,
  0.018 → 589 (typical live peak), and saturation only at genuine full scale.
  Consequence: the correct output is ~35 dB quieter than the old saturated
  one, so `synth.gain` was raised from 0.05 (which had been masking the bug)
  to 0.3; an 8-note max-velocity chord peaks ≈0.40 (≈−8 dBFS), leaving
  headroom. The PipeWire per-app stream volume (restored to 100%) is still the
  listener's loudness control.

  Lesson: the render boundary needs an **end-to-end sample-format test** —
  "the synth's float output is clean" is not the same as "the bytes handed to
  ALSA are clean".
- **Session 11 (R12 implemented):** D24 → MIX → SEQ in the prescribed order,
  test-first on the harness, four commits + docs. **D24 lane:** engine-originated
  edits mark the live project dirty; after 500 ms of quiet (sample-counted —
  no timers on the audio thread) one full snapshot (takes included) flows out
  on a dedicated mpsc; the ps-http drain task coalesces bursts and persists
  through ProjectStore::replace. Outbound-only, R5 intact; PUT races resolve
  last-writer-wins (accepted). CycleLoopBars rides it immediately — the B-key
  setting survived reboot for the first time. **Overlays (D21):** core-owned
  Overlay state mirrored to LiveStatus.overlay (→ /ws "overlay", stats line,
  UI live card); pad 9/10 toggles are mutually exclusive, mode switches exit,
  every switch voice-confirmed kick/rim/tom on private channels 8/9/10.
  **MIX (D22):** F1–F4 gain / K1–K4 pan / K5–K8 reverb of tracks 1–4; fader
  buttons select a detail page (press-again clears) where K1–K4 =
  gain/pan/reverb/chorus of that one track — the layout was left open by the
  design and this reading is documented in D22. CC math shared with
  apply_track_voice (rides and PUTs cannot diverge); mute keeps CC7=0 through
  a ride; controls inert outside the overlay; OOB indices resolve to nothing.
  **SEQ (D23):** bank-1 pads toggle steps of an 8-step window (bar cursor +
  half flip) on the selected track of the active scene (column 1 when
  stopped); empty-cell toggle creates a 1-bar clip, pitch = clip's own →
  Track.root_note (new additive field) → C4; never deletes clips; zero synth
  calls from the programmer — edits land audibly on the 16th grid via the
  pattern clock. Feeder reads the overlay atomic (drum-kit pattern) to route
  bank-1 pads away from the kit. 146 tests total (+28), clippy -D warnings +
  fmt green throughout. All of it device-free; hardware ear-checks still owed.
  Follow-up (same session): **master_gain** — optional project field
  (0..=2) applied via `fluid_synth_set_gain` at boot + every swap, so the
  web UI's new Master slider sets live output level without a recompile
  (unset = keep the 0.3 synth.gain boot default; engine clamps hostile
  values even past store validation; FFI symbol verified against the
  linked libfluidsynth3). Still one PUT away from `LiveStatus`, so no WS
  surfacing of the live value yet.
- **Session 11b**: **D25 — one voice at a time** (user feedback: piano and
  drums were layered always-on; wanted single-instrument performance with
  white-key selection in DAW mode). Semantics settled with the user: Q1 keep
  the 11-patch list + Drums; Q2 melodic voice plays notes 36–51 like any key
  (pad presses included — full 49-key instrument, pad-shadow trap gone); Q3
  kit extends across both pad banks in Drums mode (`drum_kit_2`, defaults
  [35,40,56,57,45,48,50,55], channels mirror bank 1). Model: `ActiveVoice`
  enum (externally tagged serde) + `Project.active_voice` (persisted via the
  D24 write-through lane; boot applies it over track 1's program; swaps
  re-mirror). Engine: `Event::SelectVoice` (program → prog-change on lead,
  Drums → kick confirm), `LiveStatus.active_voice` u32 atomic with
  `VOICE_DRUMS` sentinel (server crate, shared) → /ws `"voice"` + stats
  `voice=` + UI live-card cell. Feeder rewrite: DAW white-key browser
  (anchor C2, slot 0 = Drums, black keys inert, programs audition); PRESET
  routes by voice — melodic pads become lead notes, Drums silences keybed
  and arms 16-hit kit; overlay ENTRY moved to DAW pads 9/10, exits stay on
  PRESET bank 2 while an overlay lives (mode-switch-exits rule now applies
  to entering DAW only). Removed dead events SetLeadProgram + CycleLoopBars
  (B-key loop cycling gone; loop bars web-only). New core tests: select ×
  persist/atomic/kick-confirm, boot program override, swap re-mirror,
  bank-2 kit atomic; model serde/validation test. 152 tests (+3 net),
  clippy -D warnings + fmt green; engine restarted and smoke-tested live
  (PUT active_voice=drums → stats `voice=drums`, /ws `"voice":"drums"`,
  restore clean, xruns=0). Hardware ear-checks listed in §6 item 16.
- **Session 11c**: **Unified note router** (user rule: "don't treat pads
  different than notes"). First D25 ear-test failed — "only E, F, G work" in
  the browser: pad-shadow notes 36–51 were mapper-classified as Pad actions,
  so DAW-mode presses there went to scene launching / overlay entry instead
  of the browser (E3/F3/G3 above the shadow were the only real Keys). Fix
  is not a re-anchor: the feeder now reconstructs the raw note from Key AND
  Pad actions into ONE routing path and the artificial pad lane is gone.
  Consequences: browser back at C2 anchor (pad presses ARE browser events
  in DAW mode); hardware scene launching removed (notes can't be instrument
  and buttons at once — scenes live in web UI/HTTP); overlay commands moved
  to black keys above the span (G#3 MIX, A#3 SEQ, C#4 next bar, D#4 flip),
  armed in either mode, ordinary notes elsewhere; PRESET+SEQ step editing
  now triggers on notes 36–43. Drum kit unchanged (notes 36–51 = 16 slots).
  Browser unit tests added to the bin (anchor C2..G3 coverage + inert keys);
  154 tests, clippy -D warnings + fmt green; engine restarted on the new
  binary. Ear-checks owed: full white-key sweep incl. pad presses selecting
  voices, G#3/A#3 overlay entry in both modes, SEQ editing via low keys.
