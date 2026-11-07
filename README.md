# PocketStage

A portable lite DAW running headless on Linux (Raspberry Pi or laptop). A MIDI
keyboard (**M-Audio Oxygen 49 MKV**) is the primary live controller; a web UI
(phone/tablet browser) is secondary for setup/management. All audio stays local
— the browser is a remote control, never an audio path. Pattern/clip-based
(Ableton-style clip matrix), FluidSynth + soundfonts, single Rust binary.

## Status

Early prototype. The audio engine spike (ALSA + hand-written FluidSynth FFI)
is validated: 0 xruns at 256-frame periods, ~1% CPU — see
[`SPIKE_RESULTS.md`](SPIKE_RESULTS.md). For the full design and roadmap, start
with [`HANDOFF.md`](HANDOFF.md) and [`specs/brainstorming/scratchpad.md`](specs/brainstorming/scratchpad.md).

## Workspace

- `crates/pocketstage-engine` — audio callback, clock, FX, recorder (lock-free)
- `crates/pocketstage-model` — project data model: clip matrix, patterns, takes,
  versioned JSON file format
- planned: `pocketstage-midi`, `pocketstage-oxygen`, `pocketstage-server`

## License

MIT — see [LICENSE](LICENSE).
