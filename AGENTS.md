# AGENTS.md — AI coding agent guide for PocketStage

## What this is

A portable lite DAW running headless on Linux (Raspberry Pi / laptop). MIDI
keyboard (M-Audio Oxygen 49 MKV) is the primary live controller; a web UI is
secondary setup/management. All audio stays local — the browser is a remote
control, never an audio path. Single Rust binary, FluidSynth + soundfonts.

## Read these first (in order)

1. `HANDOFF.md` — current state, backlog, environment facts, decision log snapshot
2. `specs/brainstorming/scratchpad.md` — full design brainstorm + decisions D1–D10 (R-sections supersede earlier prose)
3. `SPIKE_RESULTS.md` — validated audio architecture + platform quirks

Do not start feature work without checking the backlog in `HANDOFF.md` §6 and
the decision log (§7). Decisions marked ✅ are settled; re-litigating them in
code is wasted work.

## Build & test

```bash
# System deps (Debian/Ubuntu): libasound2-dev libfluidsynth-dev pkg-config
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # must be zero
cargo fmt --all --check                                  # CI enforces both
```

CI (`.github/workflows/ci.yml`) runs all four gates on every push/PR. Run them
locally before pushing; do not push hoping CI will bless it.

The engine binary opens real audio devices — `cargo run --release -p
pocketstage-engine` is a **manual, human-with-headphones test**. Never run it
in CI or automated loops; there are no device-free integration tests for it yet.

## Crate layout & dependency rules

| Crate | Purpose | Allowed dependencies |
|---|---|---|
| `crates/pocketstage-engine` | audio callback thread, clock, FFI, metrics | `alsa`, `libc`, hand-written FluidSynth FFI |
| `crates/pocketstage-model` | project data model + versioned file format | serde/serde_json/thiserror **only** — no audio, no MIDI, no I/O side effects |
| `crates/pocketstage-midi` | port discovery, streaming parser, hotplug, event pump (`spawn_pump` → mpsc) | `alsa`, `libc`; parser is pure |
| `crates/pocketstage-oxygen` | Oxygen 49 MKV input mapper (measured map → semantic `Action`s) | pure logic + serde; demo bin uses the midi pump. LED feedback pending T2 |
| `pocketstage-server` | axum HTTP control plane: REST project CRUD, `/ws` status push, take persistence | `axum`, `tokio`, model — never audio |

Rules that keep this architecture intact:

- **Single writer:** the engine owns all mutable state; everything else sends
  commands across a channel. Never introduce `Arc<Mutex<...>>` shared into the
  audio thread. (See scratchpad R5.)
- **Audio-thread discipline:** no allocation, no locks, no syscalls that can
  block in the render loop (`main.rs`, the `ps-audio` thread). Events arrive
  via `mpsc::try_recv`, bounded per cycle. Documented exception:
  `EngineCommand::SetProject` (HTTP project swap) assigns + deallocates in the
  drain loop — user-initiated and rare; keep everything else alloc-free.
- **FluidSynth is hand-written FFI** (`engine/src/ffi.rs`) on purpose — the
  crates.io bindings are stale/abandoned. Do not add a FluidSynth crate. Only
  extend `ffi.rs` with new declarations as needed.
- **Model changes are versioned:** any incompatible change to `pocketstage-model`
  shapes requires bumping `SCHEMA_VERSION` and writing an explicit migration.
  Unknown future versions must be rejected, never partially parsed.

## Language & style

- Rust **edition 2024** (toolchain ≥ 1.85; dev box is on current stable).
  Use edition-2024 idioms: `unsafe extern "C"` blocks, let-chains.
- `clippy::nursery` isn't enforced but `-D warnings` is — fix lints, don't
  `#[allow]` them unless there's a comment explaining why.
- Prefer small, boring code; this project optimizes for hackability.

## Platform quirks already discovered (don't rediscover)

- `snd_pcm_hw_params_current()` fails with EBADMSG(77) under PipeWire's ALSA
  plugin → use freshly allocated `HwParams::any(&pcm)`.
- PipeWire holds hardware devices exclusively: raw `plughw:X,Y` is unavailable
  on a desktop session (fine on the Pi, which runs plain ALSA).
- `snd_pcm_delay` over PipeWire's ALSA plugin over-reports (~26 ms vs reality).
- FluidSynth default master gain is 0.2 — set `synth.gain` explicitly.
- FluidSynth 2.3.4 FX gotchas (session-10b post-mortem, `examples/fx_bench.rs`):
  `fluid_synth_set_chorus`'s real C signature is `(synth, nr, level, speed,
  depth_ms, type)`; bool-typed settings (`synth.chorus.active`) have NO public
  setter — toggle via `fluid_synth_chorus_on/reverb_on`; FluidR3 piano/organ/
  strings have zero chorus send (probe chorus with Lead Synth 81); stock
  reverb is room=0.2/damp=0/width=0.5 — never overwrite effect *space* params
  at boot (caused "stacked" held keys). Policy: FX levels only, only when the
  project sets them (`None` = touch nothing). `Settings` must outlive `Synth`.
- Uninhabited enums are the opaque-type idiom for C handles; `#[repr(C)]` on
  them is compile error E0084.

## Device facts (dev box)

- PipeWire audio; Oxygen 49 MKV is ALSA card 3 / sequencer client 28
  (`28:0` USB MIDI, `28:1` Mackie/HUI, `28:2` Editor SysEx).
- Soundfont: `/usr/share/sounds/sf2/FluidR3_GM.sf2`.
- Useful checks: `aconnect -l`, `amidi -p hw:3,0 -d`, `aplay -l`.

## Git conventions

- Push directly to `main` is allowed for the owner (admin bypass); CI still
  runs on push. Non-owner contributors need a PR with 1 approval.
- Linear history required (squash or rebase merges — no merge commits).
- Commit style: `area: imperative summary` (e.g. `model: add take renaming`),
  body explaining *why*.
- **Never use inline heredocs (`<<'EOF'`) or long `-m "..."` strings for commit
  messages / PR bodies.** Write the message to a temp file and reference it:

  ```bash
  cat > /tmp/commit-msg.txt <<'MSG'   # via your file-write tool, not shell heredoc
  area: summary
  ...
  MSG
  git commit -F /tmp/commit-msg.txt
  gh pr create --body-file /tmp/pr-body.txt
  ```

  Shell heredocs and multi-line `-m` flags are error-prone in agent harnesses
  (quoting, `$`, backticks, trailing-whitespace edits). A written file can be
  inspected, edited, and reused.

## Current status snapshot

See `HANDOFF.md` §6 for the authoritative ordered backlog. As of this writing
(session 10 wrap): full performance loop ✅ — record/overdub (D16/D18),
looping takes, PRESET/DAW modes (D11–D15), hardware-style retrigger (D17),
pure tested `transport.rs` state machine, render-loop no-panic law (D20)
with ps-audio watchdog. Web control plane complete for v0.1: REST + `/ws`,
remote transport/scene triggers, mixer + FX sends, clip matrix + step
editor, structure CRUD, take mute/solo/rename/delete/clear-all, project
library with switch/save-as/export (D19). Session 10: device-free engine
harness (`core.rs` — `SynthBackend` trait + fake synth driving
`EngineCore::period()` in CI), global FX knobs, JSON drum kits, SEQ/MIX
design pass (scratchpad R12, D21–D24 proposed). 113 tests, CI gates green.
Next (§6 item 14): implement R12 — D24 write-through lane → MIX overlay →
SEQ overlay, test-first on the harness. Hardware-gated (awaiting gear/ears):
feature ear-checks, pedal CC probe, Pi validation + deployment kit.
