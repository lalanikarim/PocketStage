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
| planned: `pocketstage-server` | axum REST/WS | |

Rules that keep this architecture intact:

- **Single writer:** the engine owns all mutable state; everything else sends
  commands across a channel. Never introduce `Arc<Mutex<...>>` shared into the
  audio thread. (See scratchpad R5.)
- **Audio-thread discipline:** no allocation, no locks, no syscalls that can
  block in the render loop (`main.rs`, the `ps-audio` thread). Events arrive
  via `mpsc::try_recv`, bounded per cycle.
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

See `HANDOFF.md` §6 for the authoritative ordered backlog. As of this writing:
audio spike ✅ validated, `pocketstage-model` ✅ done with tests, CI ✅ green;
next up is `pocketstage-midi` (ALSA rawmidi I/O + hotplug), with the deferred
`midi_probe` tool (task T1) available as a quick hardware-session task.
