# CODE_REVIEW.md — session 12 full-code review (findings + fix plan)

End-to-end review of all five crates (~9k lines: engine `main.rs` /
`core.rs` / `transport.rs` / `clock.rs` / `ffi.rs`, server, model, midi,
oxygen, plus the web UI), performed at `f70724d`. This document is the
working checklist: findings get ✅ with a commit hash when fixed; the prose
explains *why* so re-review doesn't need this context resurrected.

**Overall verdict.** Single-writer discipline holds (no shared state into
the audio thread); the render loop is panic-free by construction (clamped
guards, `Option` totals, bounded drains); schema evolution is additive and
disciplined; tests encode real regressions (scenes-panic, i32 scaling bug,
chorus signature). The findings below are cracks, not structural problems.

---

## 🔴 P1 — real bugs

### 1. Project swap silently drops the D25 voice override
**Status: FIXED** — `apply_voice_override()` helper, called by boot and
`apply_swap` (after `self.project = new`, reads the incoming voice); test
`swap_reapplies_voice_program_on_lead` asserts the last lead-channel
program change is the voice.

`core.rs apply_swap()` runs `apply_track_voice` for every track — resetting
channel 0 to track 1's program — but never re-applies `active_voice`. Boot
does it right (`apply_all_voices`: "tracks first, voice wins"). After any
web project load/PUT: `/ws` + stats report the voice, the feeder routes as
if it were live, but keys play track 1's program. Only reboot or re-select
fixes it.

**Fix:** extract the override from `apply_all_voices` into a helper, call
it in both places; harness test asserting `program_change(LEAD_CH, p)` is
recorded after `SetProject`.

### 2. `Track.channel` overrides are half-wired
**Status: OPEN.**

`mix_set` uses `project.track_channel(track)` (override-aware), but
`apply_track_voice` at boot/swap, pattern playback, and `all_notes_off` all
use the raw index `t as i32`. A track with `channel: Some(9)` — the
documented GM-drums use case — plays on ch 3 while MIX CCs land on ch 9:
faders do nothing audible.

**Fix:** wire `track_channel()` everywhere (pattern notes, boot/swap voice
application, all-notes-off). Harness test with a channel-override track
proving program/CC/notes all land on the overridden channel.

### 3. `store.replace()` races on a shared tmp file
**Status: OPEN.**

Three concurrent callers exist by design (axum PUT handlers, the 250 ms
`drain_takes` task, the `drain_edits` task). All write the *same*
`X.json.tmp` path between lock-release and rename: interleaved writes can
rename a torn file over the real project. Low probability, catastrophic
payload (takes live there).

**Fix:** hold the `path` mutex across write+rename (simplest correct fix)
or use per-call unique tmp names. Server test: concurrent replaces leave a
parseable, complete file.

## 🟡 P2 — correctness edges

### 4. Stuck notes when switching to Drums mid-hold
**Status: OPEN.**

Held melodic note → select Drums (browser or PUT) → the key's note-off is
now discarded (<36/>51 filter) or routed to a pad channel; the lead voice
sustains forever. Same class: PRESET→DAW while a black key is held (DAW
branch swallows its note-off).

**Fix:** `Event::SelectVoice` performs `all_notes_off(LEAD_CH)` before the
voice-confirm sound.

### 5. SEQ overlay entered from DAW mode is inert
**Status: OPEN.**

G#3/A#3 arm overlays in both modes, but step editing / CMD_NEXT / CMD_FLIP
routing only exists in the PRESET branch of the router. Entering SEQ via
A#3 in DAW yields a live overlay you cannot use (its keys are browser
keys). MIX is fine there (faders are mode-independent).

**Fix:** refuse `ToggleSeqOverlay` while `daw_mode` (feeder-side check —
the feeder owns the mode variable), so A#3 answers with nothing in DAW.

### 6. `.ps-tmp-*.json` crash leftovers pollute the library
**Status: OPEN.**

`list()` and `open_library`'s "most recent" pick match any `*.json`,
including save-as temp files (`.ps-tmp-Foo.json`). A crash between write
and rename leaves a phantom project that can even become the boot project.

**Fix:** skip file stems starting with `.` in both enumerations; sweep
leftover `.ps-tmp-*` on library open.

### 7. Model validation gaps for takes
**Status: OPEN.**

`Project::validate()` checks pitch but not recorded `NoteOn.velocity`
(1..=127) nor event `channel` (0..=15) — a hand-edited file reaches
FluidSynth with vel 255 / ch 99. Take-name uniqueness is also unvalidated
although names are the adoption key for swap/rename (duplicates rename or
collide more than one take).

**Fix:** extend validation. Additive-safe: only *rejects* invalid files,
no schema bump; test that previously-valid files still parse.

## 🟢 P3 — polish / doc drift

- `white_slot` doc (main.rs) says "Anchor at C4 (60), ABOVE the
  pad-shadowed notes" — pre-unification leftover; anchor is 36/C2. Fixed.
- `guard_slot` doc (core.rs) claims OOB events "bypass the guard
  entirely (returned slot always reads 'up')" — the code *clamps*, so OOB
  events share slots 0/15. Comment corrected to match behavior.
- `Action::LoopToggle` prints "no behavior yet" every press — dead
  transport button, intentionally left (probes pending).
- `ProjectFile::from_json_str` accepts `version: 0` (only upper bound
  checked). Left as-is: version 0 files cannot exist from any writer.
- Settings FFI `CString::new(...).unwrap()` — boot-only with static
  strings; acceptable, left as-is.

## Not bugs / accepted as designed

- Write-through vs PUT last-writer-wins races are documented and accepted
  (R12) — findings 3/7 make the persist layer itself atomic without
  changing that policy.
- `flush_guarded_offs` scans 2048 slots/period — integer-cheap, fine.
- Bank-2 kit channels mirror bank 1 (shared kill groups) — documented
  trade-off in D25.
- Item 15 (live mixer auto-save) stays open: feature, not hygiene.

## Fix order (session 12 plan)

1. engine: voice-after-swap (P1-1).
2. engine: `track_channel()` everywhere + `SelectVoice` note kill
   (P1-2 + P2-4).
3. server: atomic persist + tmp sweep + enumeration filter (P1-3, P2-6).
4. model: take validation gaps (P2-7).
5. engine: SEQ-in-DAW policy + doc drift (P2-5, P3).
6. Live smoke test on the rebuilt binary, then backlog item 15.
