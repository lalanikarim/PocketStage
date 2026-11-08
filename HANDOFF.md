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

## 3. Current state of the code

```
PocketStage/
├── Cargo.toml                          # workspace, 4 member crates
├── AGENTS.md                           # AI-agent guide — read before editing code
├── SPIKE_RESULTS.md                    # spike numbers + platform quirks log
├── specs/brainstorming/scratchpad.md   # design brainstorm, decisions D1–D10
├── specs/device-maps/oxygen-49-mkv.md  # GROUND TRUTH controller map (T1 + probes A/B/C)
├── .github/workflows/ci.yml            # build + clippy -D warnings + fmt + tests
└── crates/
    ├── pocketstage-engine/   ALSA + hand-rolled FluidSynth FFI; live MIDI (pump →
    │                         oxygen mappers → synth), --demo mode, metrics
    ├── pocketstage-model/    versioned clip-matrix project format (serde), 9 tests
    ├── pocketstage-midi/     port discovery, streaming parser, hotplug, spawn_pump → mpsc;
    │                         bin: midi_probe (list/watch/pump/send)
    └── pocketstage-oxygen/   controller input mapper — pure, JSON-reassignable, 24 tests;
                              bin: oxygen_demo (per-port mappers: music + HUI)
```

Key architectural facts proven so far:
- Audio: single-writer synth owned by the audio thread; mpsc event boundary; 0 xruns.
- MIDI: pump delivers decoded `Event`s + provenance (`MidiInputMessage`), hotplug-aware;
  device clock spam (~45 msg/s) discarded at the mapper boundary.
- Oxygen mapping: **per-port mappers** — port :0 music (default_mapping), port :1 HUI
  (hui_mapping, transport notes 86/93/94/95); fader-9 button tap-detector for its
  factory CC collision. Never open the unit's DAW mode expectations — preset mode only.

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
7. Web UI follow-ups (deferred): per-scene/clip editing widgets, FX sends

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

---

## 8. Open risks / watch-list

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

> Read HANDOFF.md, AGENTS.md, specs/device-maps/oxygen-49-mkv.md, and scratchpad
> R11 (mode system v2 + recording design, decisions D11–D16). First: run the T6
> probe (~5 min attended — user toggles the unit's DAW/PRESET button while
> `midi_probe watch` runs) to verify cc113 mode detection; record results in the
> device-map file. Then build the recording slice per R11: model additions first
> (per-track `channel` override, project `loop_bars`), then engine record-state
> machine + take capture/playback on the master clock + metronome gating, then
> mode-gated Actions (PRESET = keys/drum-kit pads/record; DAW = note selectors
> with audition via pitch % 12, bank-1 pads = scenes). Keep mpsc boundaries; no
> locks in the render loop.

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
