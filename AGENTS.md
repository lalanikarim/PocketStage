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
| `crates/pocketstage-oxygen` | Oxygen 49 MKV input mapper (measured map → semantic `Action`s) | pure logic + serde; demo bin uses the midi pump (LED output dropped — T2 found no host-controllable LED protocol) |
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
- `fluid_synth_write_float` emits normalized −1..=1. Convert to S16LE with
  `i16::MAX` (32767), **never** `i32::MAX` — session-10c bug: scaling by i32
  before the i16 clamp saturates everything above ≈−96 dBFS, turning the
  output into a square wave (sounds "crisp" on soft taps, distorted on hard
  hits). Keep `core::to_i16` + `float_to_i16_is_linear_until_true_full_scale`.
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
- **File-edit indent traps**: exact-match edits fail (or land mis-indented)
  when continuation-line whitespace is guessed. The markdown docs wrap at
  ~76–80 cols with **2-space** continuation indents under bullets — but not
  uniformly, so don't assume. Read the exact lines before editing, and check
  `git diff` after every edit call; fix mis-indents immediately rather than
  stacking more edits on top.
- **`pkill -f <pattern>` self-matches**: the pattern also appears in the
  command line of the very shell running it (and of prior wrapper shells), so
  pkill kills its own process (exit 143) and may take unrelated wrappers down
  with it. And `pkill -x pocketstage-engine` silently matches NOTHING —
  kernel `comm` is truncated to 15 chars. Working incantation:
  `pkill -x pocketstage-eng` (exact truncated name), or resolve PIDs first
  and `kill <pid>`.

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

## Standing design laws (permanent; details in the cited decisions)

- **Pads are NOT special** (D25): one unified raw-note stream — Key and Pad
  actions merge in the feeder; no pad-lane branching. Notes cannot be
  instrument *and* buttons at once.
- **The render loop may never panic and never trusts cross-lane data**
  (D20): `get()`/`is_some_and()` over indexing; state machines in pure
  tested modules (`transport.rs` pattern); ps-stats watchdogs ps-audio.
- **UI labels come from the loaded soundfont** (`GET /api/presets`, built
  by boot-time FFI preset iteration) — never hand-typed tables (a GM list
  once mislabelled most picks). **UI icons/artwork come from the brand kit**
  at `assets/brand/icons/` — never hand-drawn.
- **PRESET-mode routing is not configurable** (D26): by construction, no
  keymap schema shape expresses it; only DAW-mode semantics are data
  (`specs/daw-keymap.md`).

## Current status & next work

**This file stays static.** Do not append session updates here. Live state
lives in:

- `HANDOFF.md` §6 — authoritative ordered backlog (what's next)
- `HANDOFF.md` §7 — decision log (D# table; ✅ = settled, don't re-litigate)
- `HANDOFF.md` §9 + §10 — latest session wrap + full session history
- `specs/*.md` — subsystem plans; scratchpad R-sections — design rationale
- `CODE_REVIEW.md` — review findings ledger
