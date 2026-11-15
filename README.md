<h1 align="center">
  <img src="assets/brand/icons/pocketstage-lockup.png" width="420" alt="PocketStage" />
</h1>

A portable lite DAW running headless on Linux (Raspberry Pi or laptop). A MIDI
keyboard (**M-Audio Oxygen 49 MKV**) is the primary live controller; a web UI
(phone/tablet browser) is secondary for setup/management. All audio stays local
— the browser is a remote control, never an audio path. Pattern/clip-based
(Ableton-style clip matrix), FluidSynth + soundfonts, single Rust binary.

## Status

Working prototype, driven daily on real hardware (Oxygen 49 MKV +
FluidSynth/FluidR3). Audio core validated by the spike (ALSA + hand-written
FluidSynth FFI: 0 xruns at 256-frame periods, ~1% CPU — see
[`SPIKE_RESULTS.md`](SPIKE_RESULTS.md)) and running in anger since:
pattern/take recording, MIX/SEQ overlays, scenes, a versioned project
format, and an HTTP control plane on :7777 with a web UI (project library,
mixer, clip grid, configurable DAW keymap, live status push). For the full
design and roadmap, start with [`HANDOFF.md`](HANDOFF.md) and
[`specs/brainstorming/scratchpad.md`](specs/brainstorming/scratchpad.md).

## Workspace

- `crates/pocketstage-engine` — audio callback, clock, FX, recorder (lock-free)
- `crates/pocketstage-model` — project data model: clip matrix, patterns, takes,
  versioned JSON file format
- `crates/pocketstage-midi` — ALSA rawmidi port discovery, MIDI byte-stream
  parser, hotplug tracking, engine event pump (`spawn_pump`), `midi_probe` tool
  (Oxygen CC-map ground-truthing)
- `crates/pocketstage-oxygen` — Oxygen 49 MKV measured map → semantic
  actions; pure `DawKeymap` schema v0.1 + validation (D26, no I/O)
- `crates/pocketstage-server` — axum HTTP control plane: project CRUD,
  keymap GET/PUT + reset, overlay endpoints, `/ws` status push — never
  touches audio code

## License

MIT — see [LICENSE](LICENSE).
