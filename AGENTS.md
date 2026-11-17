# AGENTS.md — AI coding agent guide for PocketStage

## What this is

A portable lite DAW running headless on Linux (Raspberry Pi / laptop). MIDI
keyboard (M-Audio Oxygen 49 MKV) is the primary live controller; a web UI is
secondary setup/management. All audio stays local — the browser is a remote
control, never an audio path. Single Rust binary, FluidSynth + soundfonts.

## Read these first (in order)

1. `HANDOFF.md` — current state, the ordered backlog, environment facts, and
   the **authoritative decision log**
2. `specs/brainstorming/scratchpad.md` — full design brainstorm; its
   rationale sections ("R…") supersede earlier prose in the same file
3. `SPIKE_RESULTS.md` — validated audio architecture + platform quirks

Those docs are allowed to change shape — nothing here cites a section number in
them. Do not start feature work without checking the **backlog** and the
**decision log** in `HANDOFF.md`. Decisions marked ✅ are settled;
re-litigating them in code is wasted work.

### How to navigate those docs (and why this file has no pointers into them)

`AGENTS.md` is meant to be stable. The other docs are append-heavy and get
renumbered, split and re-headed as sessions pile up — so **this file refers to
documents and the *roles* of their sections ("the backlog", "the decision
log", "the device map"), never to section numbers, decision IDs, task IDs or
"current through D…" counts.** Look those up live instead of trusting a
reference found here:

```bash
grep -n '^#' HANDOFF.md                          # section inventory, by title
grep -in 'backlog\|decision log' HANDOFF.md       # jump by role keyword
grep -n '| D[0-9]' HANDOFF.md                     # the decision-ID table
```

If you ever find a section number, decision ID or "as of session N" claim in
this file, that's drift — delete it and describe the rule again in full.

## Build & test

```bash
# System deps (Debian/Ubuntu): libasound2-dev libfluidsynth-dev pkg-config
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # must be zero
cargo fmt --all --check                                  # CI enforces both
node scripts/live-merge-check.mjs                        # web UI gate: no npm, no deps
```

CI (`.github/workflows/ci.yml`) runs all five gates on every push/PR. Run them
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
| `crates/pocketstage-oxygen` | Oxygen 49 MKV input mapper (measured map → semantic `Action`s) | pure logic + serde; demo bin uses the midi pump (LED output dropped — hardware probing found no host-controllable LED protocol; see the device map) |
| `pocketstage-server` | axum HTTP control plane: REST project CRUD, `/ws` status push, take persistence | `axum`, `tokio`, model — never audio |

Rules that keep this architecture intact:

- **Single writer:** the engine owns all mutable state; everything else sends
  commands across a channel. Never introduce `Arc<Mutex<...>>` shared into the
  audio thread.
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
  `i16::MAX` (32767), **never** `i32::MAX` — a real bug here: scaling by i32
  before the i16 clamp saturates everything above ≈−96 dBFS, turning the
  output into a square wave (sounds "crisp" on soft taps, distorted on hard
  hits). Keep `core::to_i16` + the `float_to_i16_is_linear_until_true_full_scale`
  test guarding it.
- FluidSynth 2.3.4 FX gotchas (post-mortem; reproducible with
  `crates/pocketstage-engine/examples/fx_bench.rs`):
  `fluid_synth_set_chorus`'s real C signature is `(synth, nr, level, speed,
  depth_ms, type)`; bool-typed settings (`synth.chorus.active`) have NO public
  setter — toggle via `fluid_synth_chorus_on/reverb_on`; FluidR3 piano/organ/
  strings have zero chorus send (probe chorus with Lead Synth 81); stock
  reverb is room=0.2/damp=0/width=0.5 — never overwrite effect *space* params
  at boot (this caused "stacked" held keys). Policy: FX levels only, only when
  the project sets them (`None` = touch nothing). `Settings` must outlive
  `Synth`.
- **FluidSynth will not release notes on a sustained channel**: with the damper
  (CC64) engaged, `all_notes_off` / `all_sounds_off` on that channel silently do
  nothing. Every path that promises silence must send `cc(ch, 64, 0)` *before*
  the kill (and re-arm it afterwards if the pedal is still physically down).
  This bites hardest on the lead channel because track 1 routes there by
  default, so even a per-track silence loop lands on the sustained channel.
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

- PipeWire audio; Oxygen 49 MKV exposes three ports (USB MIDI, Mackie/HUI,
  Editor SysEx). **Card/client numbers are not stable** across boots and
  replugs — resolve them by name every time: `aconnect -l | grep -i -A3 oxygen`.
  `lsusb | grep -i m-audio` confirms presence; if it is absent there, the
  keyboard is powered off or unplugged — that is not a software problem.
- Soundfont: `/usr/share/sounds/sf2/FluidR3_GM.sf2`.
- DAW mode exposes **NC1 only** on this unit's firmware: its controls are
  plain MIDI with Mackie-style numbering on the HUI port — no personality
  hunting needed, factory state is the setup requirement. This is a settled
  decision; check the decision log before re-opening the emulation question.
- Knob/fader/button numbers come from our measured device map, not from
  M-Audio's guide — read `specs/device-maps/oxygen-49-mkv.md` before writing
  anything that matches a raw CC or note number.
- Useful checks: `aconnect -l`, `amidi -p hw:<card>,<port> -d`, `aplay -l`.
  The engine's rawmidi read is **exclusive**: while it runs, `aseqdump -p <c>:0`
  fails with EAGAIN and `amidi` reports the port busy — snooping the keybed
  means stopping the engine first (measured session 16e).
- Vendor manuals for the controller are **not in this repo** (copyright) — see
  the next section for where they live and how to fetch them.

## Vendor documentation (not in repo — copyright)

M-Audio's manuals are copyrighted; this repo is public, so we cite them, we do
not host them. Format-shifting (`pdftotext`) a manual into `.txt`/`.md` is still
a verbatim copy — it just makes the copy less obvious. A DMCA takedown disables
the whole repository, not one file, so this is a hard rule.

| Document | Version | Why it matters |
|---|---|---|
| Oxygen Series MKV — User Guide | v1.2 | Preset-vs-DAW mode behavior, `‹`/`›` mode caveat, control inventory, personality-switch combo |
| Oxygen 49 — Quickstart Guide | v1.1 | button layout / setup cross-checks |

- **Get them from** `m-audio.com/support` (product page → *Manuals &
  Documentation*). Keep the PDFs out of tree; extract greppable text into the
  gitignored `vendor-docs/` directory:
  `pdftotext -layout "Oxygen Series MKV - User Guide - v1.2.pdf" vendor-docs/mkv-guide-v1.2.txt`
- **Ground truth is ours, not theirs:** `specs/device-maps/oxygen-49-mkv.md` is
  the measured document the code follows. Update *that* when a probe teaches us
  something; quote the guide only in brief, always naming document + version
  (and the guide's own section number if you must — external section numbers
  are stable, ours are not). Long pastes into repo files: never.
- `vendor-docs/README.md` is tracked and restates this policy where the files
  would land; the directory itself is gitignored so a stray PDF can't be added
  by accident (`git status` stays clean).

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

## Standing design laws (permanent)

Each of these is backed by a settled decision plus a real bug or probe; the
decision log in `HANDOFF.md` is the authority and this file restates them so
they survive doc churn. If a law below contradicts the decision log, the log
wins — fix this file in the same commit that changes behavior.

- **Pads are NOT special:** one unified raw-note stream — Key and Pad actions
  merge in the feeder; no pad-lane branching. A note cannot be instrument
  *and* button at once.
- **The render loop may never panic and never trusts cross-lane data:**
  `get()`/`is_some_and()` over indexing; state machines in pure tested modules
  (`engine/src/transport.rs` is the pattern); the ps-stats thread watchdogs
  ps-audio. A silent-but-alive engine is worse than a crash.
- **UI labels come from the loaded soundfont** (`GET /api/presets`, built by
  boot-time FFI preset iteration) — never hand-typed tables (a GM list once
  mislabelled most picks). **UI icons/artwork come from the brand kit** at
  `assets/brand/icons/` — never hand-drawn.
- **PRESET-mode routing is not configurable:** by construction no keymap
  schema shape expresses it; only DAW-mode semantics are data (see
  `specs/daw-keymap.md`).

## Where live state lives (never in this file)

**This file stays static.** Do not append session updates, status snapshots or
"what's next" here — they make every session touch a document that should only
change when the architecture does.

| Live question | Authoritative doc |
|---|---|
| What's next / what's done | `HANDOFF.md` backlog section (ordered) |
| Why it works this way, and whether it's settled | `HANDOFF.md` decision log (✅ = don't re-litigate) |
| Latest session wrap + full session history | `HANDOFF.md` resume / session-log sections |
| Design rationale for a subsystem | `specs/brainstorming/scratchpad.md`, then `specs/*.md` plans |
| Measured controller wire behavior | `specs/device-maps/oxygen-49-mkv.md` |
| Audio architecture validation + quirks | `SPIKE_RESULTS.md` |
| Open review findings | `CODE_REVIEW.md` |
