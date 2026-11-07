# Audio Engine Spike — Results

> Date: 2026-11-07 · Code: `crates/pocketstage-engine/src/{main.rs,ffi.rs}`
> Task definition: HANDOFF.md §5. Status: **COMPLETE — architecture validated.**

## What was built

A single-binary spike (~430 lines Rust) proving the risky path end-to-end:

- **FluidSynth via hand-written FFI** (`#[link(name = "fluidsynth")]` + `extern "C"`
  decls against installed `libfluidsynth-dev` 2.3.4). Opaque types as uninhabited
  enums; RAII wrappers `Settings`/`Synth`; `unsafe impl Send` on `Synth` so it is
  *moved once* into the audio thread and never touched elsewhere — the single-writer
  pattern from scratchpad R5, exercised for real.
- **ALSA output via the `alsa` 0.9 crate** (`alsa::PCM` raw API): S16LE stereo,
  48 kHz, 256-frame periods × 3, blocking `writei`, underrun recovery via
  `pcm.recover()`. Device selectable via argv (`default`, `plughw:X,Y`, …).
- **Event path**: scripted note feeder (main thread) → `std::sync::mpsc` → audio
  thread drains ≤64 events per cycle → `fluid_synth_noteon/off/program_change`.
  No locks on the hot path.
- **Audio content**: arpeggio loop @ 120 BPM on ch.0 (GM saw lead ↔ EPiano program
  change every ~8 s) + kick on ch.9 (GM drums) — proves multi-timbral single-instance
  model (D3).

## Measured results (dev laptop, PipeWire ALSA plugin, release build)

| Metric | Result | Notes |
|---|---|---|
| XRUNs | **0** across all runs (≥60 s total) | zero glitches in practice |
| Cycle cadence | 375 cycles / 2.000 s window = exactly 5.333 ms | loop never misses its period |
| CPU (render + interleave only) | **~1.0–1.4%** (~55–75 µs busy per 5333 µs period) | one FluidSynth voice set, 2 voices typical |
| Event→consume latency (engine queue wait) | avg ~1.6–3.5 ms, max ≈ 1 period (5.3 ms), one 11 ms outlier in first window (warmup) | bounded by design: event waits at most until next render cycle |
| Feeder jitter (harness artifact, not engine) | ≤ 1.2 ms | non-RT `thread::sleep` on main thread |
| SCHED_FIFO | **denied** (EPERM) on stock desktop — still glitch-free | Pi will grant it via systemd/limits |
| Output queue (`snd_pcm_delay`) | ~26.6 ms reported | PipeWire ALSA plugin over-reports (includes its own pipeline buffering); re-measure on plain ALSA/Pi |

## Verdict

**The architecture is the architecture.** Hand-rolled ALSA blocking loop +
hand-written FluidSynth FFI works cleanly:

1. Hand-rolled FFI is trivial (~130 lines, compiled first try against headers;
   avoids stale crates.io bindings — scratchpad §8 risk closed).
2. 256-frame periods run glitch-free with ~1% CPU — even the worst-case event→
   render wait is one period (5.3 ms), well inside the <10–15 ms target once the
   PipeWire layer's own delay is excluded. Pi numbers will come later; a few-%
   datapoint here supports the Pi-4 hypothesis.
3. Single-writer state flow (synth owned exclusively by audio thread, mpsc
   boundary) holds up and is pleasant to work with.

## Caveats / not yet done

- **Audible check pending** — needs a human with headphones:
  `cargo run --release` (Ctrl+C stops). Nothing heard by the author of this doc.
- Raw `plughw:` comparison not possible on the dev box (PipeWire holds the
  hardware device exclusively; would need `systemctl --user stop pipewire`).
  The Pi runs plain ALSA anyway — this becomes a first-boot test there.
- First window shows one 11 ms latency outlier + higher busy% — soundfont
  paging / first-touch warmup. Steady state is clean.
- Spike intentionally uses `std::sync::mpsc`; production engine may want a true
  lock-free SPSC ring (`rtrb`) to avoid any mutex inside the callback. mpsc's
  internal mutex was uncontended here; revisit when real MIDI threads join.

## Quirks log (things worth remembering)

- `snd_pcm_hw_params_current()` **fails with EBADMSG(77) on PipeWire's ALSA
  plugin** — use freshly-allocated `HwParams::any(&pcm)` instead. (`sw_params`
  set to best-effort for the same reason.)
- Uninhabited enums (`pub enum fluid_synth_t {}`) are the right opaque-type idiom;
  adding `#[repr(C)]` to them is E0084.
- alsa 0.9 crate: params guards borrow `PCM` — scope them so `PCM` can be moved
  out of `open_playback()`; IO handle via `pcm.io_checked::<i16>()`.
