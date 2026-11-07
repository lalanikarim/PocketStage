# PocketStage — Brainstorming / Scratchpad

> **NAME: PocketStage** (decided, see D10). Repo folder is still `PiDaw` (legacy working title).
> Former working title: PiDAW.

> Portable lite DAW for headless Linux boxes (Raspberry Pi, old laptop).
> Primary control: Oxygen 49 MIDI controller. Secondary: web browser (phone/tablet).
> Use case: plug in MIDI keyboard to a networked headless computer, play live music, record tracks, manage arrangements — all from the controller or a browser tab.

---

## Core Concept

A headless, network-accessible DAW that turns any Linux box into a live music workstation. You plug in your MIDI keyboard via USB, connect speakers/headphones locally, and control everything from the Oxygen 49 (primary) or a browser on your phone/tablet (secondary).

**Key principle:** The web interface is a *remote control*, not an audio path. All audio processing stays on the box. If wifi drops, music keeps playing.

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│              Headless Linux Box (Pi / Laptop)            │
│                                                          │
│  ┌────────────┐    ┌─────────────────────────────────┐   │
│  │ Oxygen 49  │    │           DAW Core              │   │
│  │ (USB MIDI) │───▶│                                 │   │
│  └────────────┘    │  ┌───────────┐  ┌───────────┐  │   │
│                    │  │ MIDI Mapper│  │  Synth /  │  │   │
│  ┌────────────┐    │  │ (mode-    │─▶│  Sampler  │  │   │
│  │ Footswitch │    │  │  aware)   │  │(FluidSynth│  │   │
│  │ (play/rec/ │───▶│  └───────────┘  │  etc.)    │  │   │
│  │ scene)     │    │                 └─────┬─────┘  │   │
│  └────────────┘    │  ┌───────────┐        │         │   │
│                    │  │ Sequencer │◀───────┘         │   │
│  ┌────────────┐    │  │ (patterns│                 │   │
│  │ Speakers /  │◀───│  │ + scenes)│                 │   │
│  │ Headphones  │    │  └───────────┘                 │   │
│  └────────────┘    │  ┌───────────┐  ┌───────────┐  │   │
│                    │  │ Mixer/FX  │  │ Recorder  │  │   │
│                    │  └───────────┘  └───────────┘  │   │
│                    └────────────┬────────────────────┘   │
│                                 │                        │
│                    ┌────────────▼────────────────────┐   │
│                    │       Web Server (REST+WS)      │   │
│                    │   (secondary: setup, arrange,   │   │
│                    │    file mgmt, project save/load)│   │
│                    └─────────────────────────────────┘   │
└──────────────────────────────────────────────────────────┘
```

---

## Key Components

### 1. DAW Core (engine)

- **Pattern/clip-based sequencing** (Ableton Live Lite style, not linear timeline)
  - Scenes → Steps → Clips
  - Each clip can be a MIDI pattern or audio sample
- **Synth/Sampler backends:**
  - FluidSynth (SF2 soundfonts — lightweight, great for Pi)
  - ZynAddSubFX (subtractive synth)
  - Optional: VST via `vst-host` on x86 laptops (skip on Pi)
- **Mixer:** per-channel gain, pan, mute/solo, simple EQ
- **Arrangement view:** arrange clips/scenes into a song structure (intro → verse → chorus → outro)
- **Recording:** capture MIDI and/or audio to WAV/FLAC, overdub support

### 2. MIDI Input Layer

- USB MIDI via `libusb` / ALSA rawmidi
- Primary controller: **Oxygen 49**
- Optional: footswitch for transport (play/stop/record/scene)
- Support for multiple MIDI devices simultaneously
- **MIDI Learn mode:** press a controller param → it binds to the selected DAW param

### 3. Audio Output

- ALSA directly (lowest latency on Pi)
- Optional JACK bridge if user wants to mix with other apps
- Sample rate: 44.1/48kHz, buffer size configurable (64–512 samples)
- **Critical:** audio path is entirely local — no network in the audio chain

### 4. Web Interface (secondary control plane)

- **Stack:** lightweight server (Rust `axum` / Go / Node.js + WebSocket)
  - REST for state queries, file management, settings
  - WebSocket for real-time: transport control, clip triggering, parameter changes, live metering
- **Frontend:** single-page app (vanilla JS, Svelte, or React — keep it light)
  - Live view: scene pads, transport, mixer faders, knobs
  - Arrange view: drag-and-drop clip grid
  - Record view: arm tracks, take management, playback
  - File manager: import/export MIDI, WAV, project files
- **Optional audio streaming to browser** (WebRTC / Opus) for monitoring — not required, speakers are local

### 5. Project/Storage

- Single-file project format (e.g., `.dawproj` — JSON or msgpack with embedded assets)
- Or folder-based: `project.json` + `samples/` + `midi/`
- Store on SD card / SSD — keep it simple, no database

---

## Oxygen 49 Control Mapping

### Inventory

| Control | Count | Notes |
|---------|-------|-------|
| Keys | 49 | Semi-weighted, aftertouch |
| RGB Pads | 16 | Velocity-sensitive, backlit |
| Rotary Knobs | 8 | Encoders, can be absolute or relative |
| Mini Faders | 4 | Linear |
| Mod Wheel | 1 | CC1 typically |
| Pitch Bend | 1 | CC0 / 14-bit |
| Octave Up/Down | 2 | Momentary buttons |
| Transpose +/− | 2 | Momentary (±1, ±12) |
| Sustain Pedal | 1 input | 6.3mm jack |
| Expression Pedal | 1 input | 6.3mm jack |
| USB MIDI | — | In/out |

### Mode System

The Oxygen 49 becomes the **primary controller**. DAW modes remap the same physical controls to different functions (borrowed from hardware synths / Ableton Push).

**Mode switching:** Hold Pad 15 + tap Pad 16 cycles modes. Or a dedicated MODE button on footswitch.

---

#### Mode: PLAY (default — live performance)

| Oxygen Control | DAW Function |
|---------------|--------------|
| **49 Keys** | Play active synth/sampler channel(s). Layered or split zones configurable. |
| **Octave Up/Down** | Shift register octave (±1), hold for ±2 |
| **Transpose +/−** | Quick transpose of active patch (±1 semitone, hold for octave) |
| **Pads 1–8** | Scene A–H trigger (one-shot or toggle) |
| **Pads 9–16** | Clip/song section triggers (intro, verse, chorus, bridge, etc.) |
| **Knob 1** | Active synth param 1 (e.g., filter cutoff) |
| **Knob 2** | Active synth param 2 (e.g., resonance / decay) |
| **Knob 3** | Active synth param 3 (e.g., attack / reverb send) |
| **Knob 4** | Active synth param 4 (e.g., drive / delay time) |
| **Knobs 5–8** | Mapped to active channel or global effects (user-configurable per patch) |
| **Faders 1–4** | Mixer: gain for channels 1–4 |
| **Mod Wheel** | Continuous param on active synth (vibrato depth, filter LFO, etc.) |
| **Pitch Bend** | Pitch bend / portamento |
| **Sustain Pedal** | Note sustain (standard) |
| **Expression Pedal** | Global volume swell, or mapped to a chosen param |

> During PLAY mode you never touch the browser. You play, switch scenes, tweak knobs in real time.

---

#### Mode: SEQ (step sequencer / pattern editing)

| Oxygen Control | DAW Function |
|---------------|--------------|
| **16 Pads** | 16-step sequencer grid. Tap to toggle step on/off for active channel. |
| **Keys** | Preview individual notes/MIDI events at cursor position |
| **Knob 1** | Step duration / swing amount |
| **Knob 2** | Velocity of selected step |
| **Knob 3** | Pattern length (4/8/16 steps) |
| **Knob 4** | Cursor position (scroll through steps) |
| **Faders 1–4** | Per-channel pattern velocity / gain |
| **Octave buttons** | Navigate between patterns (A/B/C/D) |
| **Transpose** | Shift note range in current pattern |

> Program patterns live without a screen — tap pads to build steps, knobs to shape them.

---

#### Mode: MIX (mixer / FX focus)

| Oxygen Control | DAW Function |
|---------------|--------------|
| **Faders 1–4** | Channel gain 1–4 |
| **Knob 1** | Pan for selected channel |
| **Knob 2** | Send A (reverb amount) for selected channel |
| **Knob 3** | Send B (delay amount) for selected channel |
| **Knob 4** | FX parameter (reverb size, delay time, etc.) |
| **Pads (bottom row)** | Select channel to edit (1–8), or toggle mute/solo |
| **Mod Wheel** | Fine-tune currently-selected param |

---

#### Mode: SETUP (rarely used — configuration)

| Oxygen Control | DAW Function |
|---------------|--------------|
| **Pads** | Menu navigation (up/down/confirm/back) |
| **Knobs** | Adjust selected parameter value |
| **Keys** | Quick jump / search |
| **Faders** | N/A or fine adjustment |

> Use for renaming tracks, changing MIDI mappings, importing soundfonts, etc. For anything complex, fall back to browser.

---

### Transport (always accessible, any mode)

| Function | Mapping |
|----------|---------|
| **Play / Stop** | Sustain pedal held = play, released = stop (guitar-pedal style). Pad 15 as backup toggle. |
| **Record** | Pad 16 (momentary arm, second tap confirms) |
| **Tap Tempo** | Double-tap any pad in a specific zone, or dedicated button combo |
| **Tempo fine** | Knob 5 (in PLAY mode) or expression pedal |
| **Next/Prev scene** | Octave up/down buttons (contextual in PLAY mode) |

---

### Pad Backlight as Status Display

No screen on the Pi → **the Oxygen's 16 RGB pads become the primary visual feedback.**

| State | Color | Meaning |
|-------|-------|---------|
| Scene active | Green | Currently playing scene |
| Clip playing | Blue | One-shot clip sounding |
| Record armed | Yellow blink | Ready to record |
| Recording | Red solid | Capturing |
| Step seq: step on | White | Step is active in pattern |
| Step seq: cursor | Cyan blink | Current playhead position |
| Mixer: channel selected | Purple | This fader/knob controls this channel |
| Mode change | All pads flash mode color | Brief flash to confirm mode switch |

Sent via MIDI LED/color commands back to the Oxygen over USB.

---

### MIDI Mapping Engine (critical piece)

Must be:

1. **Mode-aware** — same CC does different things in PLAY vs SEQ vs MIX
2. **User-reassignable** — mapping table (JSON), rebind any control to any DAW param
3. **Learn-mode capable** — "press the knob, it grabs what you touch"
4. **Per-patch overrides** — Knob 1 controls filter on Patch A but decay on Patch B

```jsonc
// mappings.json (simplified)
{
  "modes": {
    "play": {
      "keys":       { "function": "note_play", "channel": "active" },
      "pads_1_8":   { "function": "scene_trigger", "scenes": ["A","B","C","D","E","F","G","H"] },
      "pads_9_16":  { "function": "clip_trigger", "clips": ["intro","verse","chorus","bridge","outro","fill","sting","loop"] },
      "knob_1":     { "function": "param", "target": "active_synth.filter_cutoff" },
      "knob_2":     { "function": "param", "target": "active_synth.resonance" },
      "knob_3":     { "function": "param", "target": "active_synth.attack" },
      "knob_4":     { "function": "param", "target": "active_synth.reverb_send" },
      "fader_1":    { "function": "mixer.gain", "channel": 1 },
      "fader_2":    { "function": "mixer.gain", "channel": 2 },
      "fader_3":    { "function": "mixer.gain", "channel": 3 },
      "fader_4":    { "function": "mixer.gain", "channel": 4 },
      "mod_wheel":  { "function": "param", "target": "active_synth.vibrato_depth" },
      "pitch_bend": { "function": "note.pitch_bend" },
      "sustain":    { "function": "note.sustain" },
      "octave_up":  { "function": "register.octave_up" },
      "octave_dn":  { "function": "register.octave_down" }
    },
    "seq":   { /* ... */ },
    "mix":   { /* ... */ },
    "setup": { /* ... */ }
  },
  "patch_overrides": {
    "dream_pad":  { "knob_1": "cutoff", "knob_2": "release" },
    "saw_lead":   { "knob_1": "drive",  "knob_2": "filter_res" }
  }
}
```

---

## Web UI (secondary — management & setup)

Since Oxygen 49 handles live performance, the browser is for things you do before/after a session:

- **Project manager:** create/save/load projects, import/export
- **Arrangement editor:** drag-and-drop song structure (hard to do via pads)
- **Sound library browser:** browse/install SF2 soundfonts, sample packs
- **MIDI mapping editor:** visually rebind controls (or use pad-based SETUP mode for quick changes)
- **Recording review:** listen back to takes, label them, delete bad ones
- **Settings:** audio device config, buffer size, network, Wi-Fi
- **Remote monitoring:** optional low-quality audio stream (not for performance, just checking in)

---

## Latency & Performance Strategy

| Path | Latency target | Method |
|------|---------------|--------|
| MIDI in → synth → speaker | < 10 ms | Local ALSA, small buffer |
| Browser button → DAW action | < 50 ms | WebSocket, local network |
| Browser fader → audio param | < 20 ms | WS + interpolation on engine side |

**Key principle:** Web interface is a *remote control*, not an audio path. All audio processing stays on the box.

---

## Tech Stack (leaning)

| Layer | Pi-friendly options | Laptop options |
|-------|-------------------|----------------|
| Language | Rust, C, Go | Rust, C++, Go, Node |
| Audio I/O | ALSA (C API) / CPAL | JACK / PortAudio |
| MIDI | alsa-rawmidi / libusb | same + USB HID |
| Synth | FluidSynth (C), custom DSP in Rust | + VST host |
| Web server | axum (Rust) / Go net/http | same |
| Frontend | Static files served by the box | same |

**Lean: Rust** for the core (audio engine, MIDI, web server) — single binary, low memory footprint (~50–100 MB RAM), no GC pauses in audio thread. Serve a small static frontend from the same binary.

---

## MVP Scope (v0.1 — Oxygen-first)

1. ✅ 4 synth channels (FluidSynth, user picks SF2)
2. ✅ **Oxygen 49 mode system:** PLAY / SEQ / MIX (3 modes, mapped via JSON)
3. ✅ 8 scenes, triggered by pads in PLAY mode
4. ✅ 16-step sequencer per channel, editable in SEQ mode via pads
5. ✅ 4-channel mixer with faders
6. ✅ Transport: play/stop/record via pad combos + optional footswitch
7. ✅ Pad backlight feedback (scene state, record state, cursor position)
8. ✅ Record MIDI to a clip (arm via pad, play, stop)
9. ✅ Web UI: project save/load, basic mixer view, recording list
10. ✅ Single-file project export (JSON + embedded assets)

**Cut from MVP:** expression pedal mapping, per-patch knob overrides, FX send/return, browser audio streaming, VST support.

---

## Footswitch Companion (recommended)

A 3–4 button MIDI foot pedal for hands-free transport:

| Button | Function |
|--------|----------|
| 1 | Play / Stop |
| 2 | Record (momentary arm + confirm) |
| 3 | Next scene |
| 4 | Mode cycle (optional) |

Cheap options: Korg CP-7, or DIY 4-button USB MIDI foot pedal (~$20).

---

## Differentiators

- **Zero-install client.** No app on phone/tablet — just open a URL.
- **Headless & always-on.** The Pi is your "music server." Plug in keyboard, play.
- **Local audio, remote control.** Pro-quality low-latency audio locally, convenience of touch control remotely.
- **Resource-light.** Runs on a $35 Pi 4 with 2GB RAM. No GPU needed.
- **Open & hackable.** Single binary, JSON project files, no proprietary format.
- **Controller-first UX.** Designed around the Oxygen 49, not a mouse/keyboard.

---

## Potential Challenges

1. **MIDI device hotplug** — handle USB disconnect/reconnect gracefully without crashing audio thread
2. **Pi audio crackle** — real-time scheduling (`SCHED_FIFO`), avoid GC/alloc in audio callback, test Pi 4 vs Pi 5
3. **WebSocket reliability over wifi** — reconnect logic, local state persistence so dropped connection doesn't lose session
4. **Browser input latency for live performance** — keep note-playing on MIDI controller; use browser for arrangement/management
5. **Oxygen 49 pad LED control** — need to verify exact MIDI CC/OSC protocol for RGB pad colors (may be limited)

---

## Open Questions / TODO Discuss

- [ ] Oxygen 49 pad LED capabilities — can we actually set per-pad RGB colors via MIDI? What's the protocol?
- [ ] Exact Oxygen 49 CC map — need to document which CCs each knob/fader/pad sends
- [ ] Footswitch: buy off-the-shelf vs DIY?
- [ ] Audio engine: FluidSynth C API binding in Rust, or run as subprocess with MIDI routing?
- [ ] Should we support multiple Oxygen 49s or other controllers simultaneously?
- [ ] Song/arrangement structure — how deep does it go for v0.1? (scenes only vs full arrangement timeline)
- [ ] Effects: reverb/delay built-in from v0.1, or v0.2?
- [ ] Audio recording (not just MIDI) — v0.1 or later?
- [ ] Pi 4 vs Pi 5 target — audio performance differences?
- [ ] Network discovery — mDNS/Avahi so the browser can auto-find the PiDAW box on LAN?
- [ ] Auth/security — open on LAN, or token-based access?

---

# Refinement Pass v2 (Session 2)

## R1. Hardware — RESOLVED ✅

**Controller: M-Audio Oxygen 49 MK5** (2021, current model, ~$150).

> ⚠️ Corrections: earlier sections of this doc assumed Novation-style hardware (RGB pads,
> aftertouch, expression pedal). Those assumptions are superseded by the inventory below.

Corrected inventory (MK5):

| Control | Count | Notes |
|---------|-------|-------|
| Keys | 49 | Velocity-sensitive, **no aftertouch** |
| Pads | 16 | Velocity-sensitive, **single-color backlight (not RGB)** |
| Knobs | 8 | Assignable rotary |
| Faders | 4 | Assignable — matches our 4 tracks exactly 👍 |
| Pitch/Mod | 1 combined lever | Pitch bend + mod wheel in one |
| Transport buttons | ~6 | Dedicated: rewind/stop/pause/FF/rec/loop |
| Octave up/down, program/bank | buttons | Onboard |
| Sustain pedal input | 1 (1/4" jack) | Works with any normally-open momentary pedal |
| Expression pedal input | ❌ none on MK5 | Removed from mapping |
| MIDI out | USB (+ 5-pin DIN to verify) |

Design implications:
- **Pads are one-color** → blink-pattern encoding (R4) is now *essential*, not just nice. States must be distinguishable by behavior (solid/pulse/heartbeat/sweep), not hue.
- **Dedicated transport buttons send MIDI** → use them for play/stop/rec instead of pad combos. Frees pads, and they're unlabeled-by-us = unambiguous.
- **No expression pedal / aftertouch** → drop from all mappings; sustain jack is the only pedal slot (reserved for a future $15 momentary footswitch — see D7).
- Exact CC map still TBD → build `midi_probe` tool (see Task list) to empirically log every control's output. Do not trust published tables.

All 16 pads + 8 knobs + 4 faders + transport buttons confirmed sufficient for the PLAY/SEQ/MIX mode design.

TODO: confirm model, then document exact CC map for that hardware.

## R2. Domain model (concrete)

```
Project
├── bpm (fixed per project in v0.1)
├── Tracks (max 4)
│   ├── name, color
│   ├── instrument slot: {soundfont, program} or builtin-synth patch
│   └── clip slots[8]          ← one clip per scene
├── Scenes (max 8)             ← vertical slice: what plays together
│   └── per track: clip reference or "empty" (don't change)
├── Patterns (step sequences, owned by clips)
│   ├── length in bars (1–4), resolution 16th
│   └── steps: {note, velocity, on/off}
└── Takes[]                    ← recorded MIDI performances
```

Decisions this forces:
- **Clip = (track × scene) cell.** Chorus drum pattern and verse drum pattern are separate clips, same track. Scene launch plays the whole column. Arrangement and live set become the SAME data structure — big simplification.
- **Quantized launching:** immediate / next beat / next bar. Default **next bar**.
- **Pattern length in bars (1–4), not 16 steps.** 16 steps = 1 bar of 16ths.
- **No separate LIVE/STUDIO modes.** One model, two views: browser has Arrange tab, Oxygen lives in the clip matrix. Same state underneath.

## R3. Instrument architecture (simplification)

> **One shared FluidSynth instance with 16 internal MIDI channels. Track N → channel N.**

- Multi-timbral playback for free (GM soundfonts give drums on ch.10)
- No per-track audio buffer mixing; mixer = FluidSynth channel gain/pan CCs
- CPU/RAM friendly on a Pi

Trade-offs + mitigations:
- No per-track FX inserts → **global FX only in v0.1** (master reverb + delay as simple Rust DSP after FluidSynth output)
- No per-track audio recording → **record MIDI always, render audio offline.** Takes stored as MIDI events; WAV bounce is an on-demand headless render. Non-destructive, can't ruin a take by clipping.

## R4. Screenless feedback (tightened)

1. Oxygen Pro LCD shows static labels only (dynamic text over USB MIDI is limited). Pads remain primary feedback.
2. **Blink patterns > colors.** Encode state in behavior:
   - SEQ mode: playhead sweeps left→right across pads
   - Recording: heartbeat blink
   - Scene armed, waiting on quantize: slow pulse until it fires
   - Unambiguous under stage lights, even with few reliable colors.
3. **Click track is mandatory** for screenless workflow — volume on a knob, toggle on a pad.

## R5. Code shape (first sketch)

```
pocketstage/
├── crates/
│   ├── pocketstage-engine    # audio callback, clock, FX, recorder (lock-free)
│   ├── pocketstage-model     # Project struct, serde schema, versioned file format
│   ├── pocketstage-midi      # ALSA rawmidi I/O, hotplug, mapper, learn mode
│   ├── pocketstage-oxygen    # Oxygen 49 MKV map: CC tables, pad LED protocol, modes
│   └── pocketstage-server    # axum: REST (files/settings) + WS (state/commands)
├── web/                # SvelteKit static build, embedded via rust-embed
└── sounds/             # SF2 management
```
Status: workspace scaffolded at repo root with `crates/pocketstage-engine` (audio spike in progress).

**State flow (single writer):** engine owns all state → UI sends `command` messages over WS → engine applies on audio-thread-safe boundary → broadcasts JSON state snapshots (~15 Hz or on-change). No shared mutable state, no DB.

## R6. v0.1 = vertical slice (the demo that proves everything)

> Boot Pi headless → Oxygen auto-detected → play keys with default 4-patch set → launch 3 scenes via pads (quantized to bar) → build a drum pattern in SEQ mode → sustain pedal hold = play, tap rec pad, perform, stop → open browser on phone: see take list, play it back, save project.

**Explicitly deferred past v0.1:** audio recording (MIDI-only), per-track FX, arrangement timeline, multi-controller, overdub/punch-in, tempo changes mid-song. mDNS discovery stays IN (Avahi is nearly free → `http://pidaw.local`).

## R7. Decision log

| # | Decision | Status |
|---|----------|--------|
| D1 | Controller = **M-Audio Oxygen 49 MK5** | ✅ RESOLVED |
| D2 | Clip matrix model (track × scene), no separate LIVE/STUDIO modes | Proposed |
| D3 | Single FluidSynth instance, tracks = MIDI channels | Proposed |
| D4 | Record MIDI only in v0.1; offline WAV bounce | Proposed |
| D5 | Scene launch quantized to next bar (configurable) | Proposed |
| D6 | Rust workspace structure as sketched (R5) | Proposed |
| D7 | No footswitch owned → v0.1 transport = Oxygen's dedicated transport buttons; sustain jack reserved for optional future momentary pedal (~$15) | ✅ RESOLVED |
| D8 | Ship curated default sound set (you own some SF2s to add later) — see R8 | ✅ RESOLVED |
| D9 | mDNS + no auth in v0.1 (LAN-trusted) | Proposed |
| D10 | Project name = **PocketStage** (was: PiDAW working title) | ✅ RESOLVED |

## R8. Curated default sound set (D8)

Strategy: **GM soundfont as base + a thin "patch" layer on top.**

- **Default SF2: FluidR3 GM** (`fluid-soundfont-gm`) — packaged by most Linux distros, complete GM map incl. drum kit on ch.10, zero licensing friction.
- **Recommended upgrade: GeneralUser GS** (free, ~30 MB, dramatically better quality) — offer as one-tap download in the web UI, or bundled if license permits redistribution.

**Patch layer:** a patch = `{ name, soundfont, gm_program, knob_assignments }` stored as small JSON. Ships with 8 curated starter patches mapped to the 4 tracks:

| Patch | GM program | Default knobs (1–4) |
|-------|-----------|---------------------|
| Grand Piano | Acoustic Grand | brightness, release, chorus, reverb |
| Electric Piano | EPiano | tremolo, decay, chorus, reverb |
| Warm Pad | Slow Strings / pad | filter cutoff, attack, resonance, reverb |
| Analog Lead | Lead 2 (saw) | filter cutoff, resonance, drive*, delay send |
| Fretless Bass | Finger Bass | filter, decay, drive*, reverb |
| Pluck | Synth Pluck | decay, filter, chorus, delay |
| Strings | String Ensemble | attack, release, reverb, chorus |
| Drum Kit | (GM ch.10) | kit level, pan width, room, comp* |

(* = engine-side DSP params added in v0.2; placeholders for now)

Patch bank is user-extensible: browser UI can point any patch at any SF2 on the box + program number. Your own SF2s drop into `sounds/` and appear automatically.

## R9. New tasks generated by this pass

- [~] **T1 — `midi_probe` tool** (tiny Rust binary): lists ALSA MIDI inputs, logs every incoming event with timestamp → move every control on the Oxygen 49 MK5 → produces our ground-truth CC map + LED/feedback behavior. FIRST code artifact. *(DEFERRED by decision; device confirmed present — see T0)*
- [ ] T2 — Verify pad backlight control protocol on MK5 over USB MIDI (can we light/blink pads from the host? If NOT: major UX fallback needed — transport buttons + sound feedback instead). **Likely path found:** dedicated `Editor` SysEx port (see T0).
- [ ] T3 — Check whether MK5 transport/octave buttons are remappable or fixed CCs; design around fixed ones. Note: Mackie/HUI port existence implies template/mode-dependent behavior — probe will need the unit in a known mode.

## R10. Device verification (T0) — done on dev laptop

Oxygen 49 MKV confirmed connected & enumerated (dev machine, PipeWire-based desktop):

```
USB:    ID 0763:0002 M-Audio Oxygen 49 MKV
ALSA:   card 3 "MKV"
Ports:  28:0  Oxygen 49 MKV USB MIDI        <- main control path
        28:1  Oxygen 49 MKV Mackie/HUI      <- MCU/HUI emulation port
        28:2  Oxygen 49 MKV Editor          <- SysEx editor/config port
```

Findings:
- **Editor port** → M-Audio standard SysEx editor protocol almost certainly available for pad LED / display control from host (answers T2 positively, pending protocol details).
- **Mackie/HUI port** → unit has emulation modes; CC values may vary per mode/template. Probe must pin the unit to a known (ideally MIDI/Advanced) mode first.
- ALSA rawmidi coexists fine with PipeWire on laptops.
- Passive 5s listen on hw:3,0 produced no events (idle device) — plumbing OK, no data yet.
- [ ] T4 — Confirm FluidR3_GM redistribution terms for bundling with a Pi image (vs. distro-package install step).
- [ ] T5 — If 5-pin DIN MIDI out exists on the MK5: support chaining external gear later (out of v0.1 scope, just note it).

---

## Name

**PocketStage** ✅ (decided — D10)

Former working title: PiDAW. Alternatives considered: NetSynth, WireDAW, LooseEnds, Stagehand.
Repo folder still `PiDaw`; rename to `PocketStage` when convenient (`mv ~/Projects/PiDaw ~/Projects/PocketStage`).
Binary name: `pocketstage`. Crate prefix: `pocketstage-*`.
