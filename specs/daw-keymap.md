# DAW keymap — configurable DAW-mode bindings (multiphase plan)

**Status: PLANNED — not started. Decision slot D26 reserved.**
Requested session 12: "configure DAW mode key and button mappings so we can
manage it in a json file … what we don't want to allow remapping is Preset
mode." Revisit before implementing; open questions below must be answered
first (they change schemas, not just code).

## Goal

The DAW-mode *semantic* layer — today hardcoded in `engine/src/main.rs`
(`VOICES`, `VOICE_BROWSER_ANCHOR`, `white_slot()`, `CMD_MIX/CMD_SEQ/
CMD_NEXT/CMD_FLIP`) — becomes data: a JSON file the web UI can view and
edit, hot-reloaded without engine restart.

**Non-goals / hard invariants:**

- **PRESET routing is not expressible in the schema at all** (melodic
  passthrough, 16-hit kit on notes 36–51, SEQ step editing on 36–43).
  "Can't remap preset" is enforced *by construction* — there is no JSON
  shape that describes it. This is the whole safety story; keep it.
- Single writer (R5): the keymap never touches the audio thread. It is
  consumed by the MIDI feeder (main thread) only. No new locks on any
  render path.
- Boot with a missing/corrupt keymap file must NEVER fail: fall back to
  built-in defaults (= today's hardcoded table) + loud log line.
- Device-layer `Mapping` (oxygen crate, what notes/CCs the unit transmits)
  stays a separate concept/file from this *semantic* layer. Optional
  exposure in phase 5 (Q7).

## Proposed storage & schema (draft v0)

File: `~/.pocketstage/keymap.json` — global device config, NOT per-project
(bindings belong to the keyboard, not the song; Q1 if that changes).

```json
{
  "voices": [
    { "note": 36, "voice": "drums",          "label": "Drums" },
    { "note": 38, "voice": { "program": 0 }, "label": "Grand Piano" }
  ],
  "commands": {
    "mix_toggle":   56,
    "seq_toggle":   58,
    "seq_next_bar": 61,
    "seq_flip":     63
  },
  "fader_buttons": [null, null, null, null]
}
```

- `voices`: explicit note→`ActiveVoice` bindings; any notes legal (the
  browser is no longer tied to white-key geometry). Defaults reproduce
  today's C2..G3 / 12-slot table exactly.
- `commands`: the four overlay commands, semantics unchanged, rebindingable.
- `fader_buttons`: optional command-per-fader-button bindings (Q2).
- Validation rules (same discipline as `Mapping`): `deny_unknown_fields`;
  notes 0..=127; programs 0..=127; **no note claimed twice** across
  voices+commands+button targets; label length cap. Invalid file → reject
  PUT with the message, keep running config.

## Reload architecture (keeps R5)

```
web UI ──PUT /api/keymap──> ps-http: validate (oxygen::DawKeymap)
                              ├─ persist ~/.pocketstage/keymap.json
                              └─ publish Arc<DawKeymap> into KeymapSlot
main-thread feeder ── reads slot on each MIDI event (Arc clone under a
                      short std Mutex — feeder is NOT the render path)
engine boot ── load file → KeymapSlot initial value (defaults on error)
```

`AppState` gains `Option<Arc<KeymapSlot>>` (embedded mode only). No
EngineCommand involvement: the audio thread neither reads nor receives the
keymap. Stats/`/ws` may mirror a `keymap_version` counter for UI feedback.

## Phases

### Phase 0 — decisions (blocking)

Answer Q1–Q7 below; freeze schema v0; write D26 into scratchpad R13/R14 +
HANDOFF §7 table. No code.

### Phase 1 — `DawKeymap` struct (oxygen crate, pure + serde)

- Structs: `DawKeymap`, `VoiceBinding`, `CommandBindings`; `default()` =
  today's table; `validate()` with human errors; `from_json_str` /
  `to_json_string`; duplicate-note detection as a method (needed by UI too).
- Tests (device-free, CI): defaults serialize→parse round-trip; unknown
  field rejected; dup notes rejected across all three sections; old/empty
  file → defaults; program/note range checks.
- Acceptance: `cargo test -p pocketstage-oxygen` green; no engine changes.

### Phase 2 — persistence + HTTP control plane (server crate)

- `KeymapSlot` (newtype over `Mutex<Arc<DawKeymap>>`) in the server crate,
  shared with engine main() at boot; file path resolution mirrors the
  project store (`POCKETSTAGE_KEYMAP` env override for tests, like
  `POCKETSTAGE_PROJECT`).
- `GET /api/keymap` (current JSON + defaults for diffing), `PUT`
  (validate→persist→publish; 204/400), `POST /api/keymap/reset`.
- Tests: round-trip via in-process router; corrupt PUT keeps previous map;
  reset restores defaults; file written atomically (reuse the session-12
  durable-write pattern from `ProjectStore::replace`).

### Phase 3 — feeder consumption (engine main.rs)

- Replace hardcoded branches in `run_live_midi` with keymap lookups:
  note → `VoiceSlot(voice,label)` | `Command(cmd)` | fall through to the
  fixed PRESET path. The PRESET branch keeps its literal constants
  (36–51 kit, 36–43 SEQ steps) — untouched by design.
- Selection audition behavior per Q3.
- Harness/unit tests: routing table is pure enough to test without MIDI —
  extract `route_daw_note(&DawKeymap, note) -> DawEvent` as a total
  function; table-test defaults == current hardcoded behavior (regression
  lock), custom maps re-route correctly, unbound notes inert.
- Acceptance: device-free tests green; live test = remap one voice + one
  command from the API, feel it immediately, no restart.

### Phase 4 — web UI ("DAW keys" card)

- Binding table editor: rows = voices + commands (+ buttons if Q2=yes);
  "capture" flow: click a row → press a key on the Oxygen → note arrives
  via a new `POST /api/keymap/capture` ack mechanism or ephemeral `/ws`
  frame carrying raw bound-note presses (Q5 — needs design: how does the
  browser see hardware presses? Feeder could push "keymap probe" notes to
  `/ws` while a capture session is armed via `POST /api/keymap/probe`).
- Conflict highlighting client+server (server re-validates, always).
- Reset-to-defaults button; import/export raw JSON (textarea) as escape hatch.
- Acceptance: full remap loop with zero file-handling by the user.

### Phase 5 — optional: device-layer `Mapping` exposure

- `GET/PUT /api/mapping` for oxygen `Mapping` (pad base, fader CCs…) with
  its existing `validate()`; UI advanced tab. Separate ship decision (Q7).

## Risks & edge cases (carry into implementation)

- **Capture ambiguity**: while a capture session is armed, the pressed key
  is *also* live (voice select/command fires). Mitigation: probe mode
  swallows routed events while armed — decide in Q5.
- **Stuck notes on remap**: re-binding mid-hold can orphan a note-off
  (off arrives under the new map). Feeder routing must be stateless per
  message OR resolve offs with the *on* map; simplest: keymap changes only
  take effect at transport-stopped, or accept + document.
- **Bricking your own controls**: a map that binds nothing to mix_toggle
  while MIX is live — always keep web-side overlay exit available (it is;
  overlays are HTTP-triggerable? verify: currently not! consider adding
  `POST /api/overlay/{mix|seq|none}` as part of phase 2 so a bad keymap is
  never a trap).
- **Schema growth**: keymap file gets `version` like ProjectFile? For v0,
  shape-validate with serde; add version only when it first changes.

## Open questions (answer in phase 0)

1. **Global vs per-project keymap.** Lean: global device config
   (`~/.pocketstage/keymap.json`). Counter: scene-style setups wanting
   per-song bindings.
2. **Binding targets**: keys only, or also fader buttons / knobs / CCs?
   Lean: keys + fader buttons; no raw-CC targets until someone asks.
3. **Audition pitch for arbitrary voice bindings** — press C6 binding, hear
   pitch 84? Lean: yes, WYSIWYG. Alternative: fixed audition note per slot.
4. **Preset-mode overlay commands follow the remap?** Today G#3/A#3 also
   work inside PRESET while an overlay is live. Lean: same keymap drives
   them (one source of truth; instrument routing untouched). Alternative:
   hardcode 56/58 for in-PRESET use — two sources of truth, meh.
5. **Capture mechanism** for the UI: `/ws` raw-press frames while armed vs.
   separate probe endpoint vs. no capture (manual note number entry only —
   simplest honest v1). Lean: manual entry v1, capture as fast-follow.
6. **Voice list editing**: is the 12-slot curated `VOICES` table itself
   editable (any GM program 0–127 per binding), or only its note
   assignments? Schema above says any program — confirm; UI needs a GM
   name picker either way.
7. **Scope**: expose device-layer `Mapping` via the same interface while
   we're at it (phase 5), or strictly DAW keymap this round?

## Test-first notes (repo law)

Every phase lands behind harness/unit tests before wiring: phase 1 is pure;
phase 2 uses the existing in-process axum router tests; phase 3 extracts a
pure `route_daw_note` so routing tables are CI-testable with zero MIDI.
Gates unchanged: `cargo test/clippy -D warnings/fmt` all four, every push.
