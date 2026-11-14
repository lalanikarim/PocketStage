# DAW keymap — configurable DAW-mode bindings (multiphase plan)

**Status: PHASE 0 DONE — schema v0.1 FROZEN (session 13; v0 rectified after
MKV docs review — see schema section). Phases 1–4 ready to implement,
T7 probe gates knob/bank consumption. Decision D26 in scratchpad R14 +
HANDOFF §7.**
Requested session 12: "configure DAW mode key and button mappings so we can
manage it in a json file … what we don't want to allow remapping is Preset
mode." All seven open questions answered (rulings at each Q below); no code
written yet.

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

## Storage & schema — **v0.1 FROZEN (rectification of v0, session 13)**

v0 was re-frozen as v0.1 BEFORE any code or keymap file existed — the one
free moment to change shape. Reason: docs review (Oxygen MKV User Guide
v1.2) showed DAW mode's primary controls include knobs (CC sources, HUI
port), which v0's note-only binding shape could not express without two
permanent binding paradigms. v0.1 freezes the SOURCE vocabulary once.

File: `~/.pocketstage/keymap.json` — global device config, NOT per-project
(bindings belong to the keyboard, not the song; Q1 if that changes).

**Source object (frozen vocabulary — all bindings everywhere use it):**

```json
{ "note": 36 }                 // keybed/pad note (music or HUI port)
{ "cc": 44 }                   // knob/fader CC
{ "note": 56, "port": "hui" }  // optional qualifier: "music" | "hui" | absent = any
```

Notes and CCs are disjoint number spaces — dup-detection is per space;
note 44 and cc 44 may coexist. `port` defaults to any; it exists because
measured ground truth routes DAW-mode controls on the HUI port and keys/
pads on the music port.

```json
{
  "voices": [
    { "source": { "note": 36 }, "voice": "drums",          "label": "Drums" },
    { "source": { "note": 38 }, "voice": { "program": 0 }, "label": "Grand Piano" }
  ],
  "commands": [
    { "source": { "note": 56 }, "command": "mix_toggle" },
    { "source": { "note": 58 }, "command": "seq_toggle" },
    { "source": { "note": 61 }, "command": "seq_next_bar" },
    { "source": { "note": 63 }, "command": "seq_flip" }
  ],
  "fader_buttons": [
    { "source": { "cc": 49, "port": "hui" }, "command": "bank_prev" },
    { "source": { "cc": 50, "port": "hui" }, "command": "bank_next" }
  ],
  "knobs": []
}
```

- `voices`: source→`ActiveVoice` bindings; any notes legal (browser no
  longer tied to white-key geometry). Defaults reproduce today's C2..G3 /
  12-slot table exactly. `voice` stays an externally-tagged enum
  (`"drums"` | `{"program": N}`) — Addition A: item 19 may add
  `{"patch": …}` without breaking files.
- `commands`: command vocabulary FROZEN at:
  `mix_toggle`, `seq_toggle`, `seq_next_bar`, `seq_flip`,
  `bank_prev`, `bank_next` (last two = MIX-overlay paging, mirroring the
  documented `<`/`>` track-bank scroll; no default binding in built-in
  defaults until T7 measures the real notes). Momentary semantics.
- `fader_buttons`: per-button command bindings (Q2), source is a CC on
  the HUI port per measured map.
- `knobs`: **section-shaped, ships empty.** Target semantics are
  continuous parameters, NOT momentary commands — deliberately NOT frozen
  in v0.1 (Q-V2a): parameter naming (`{"param": {"track": N, "target":
  "gain"}}`-style) is an intended-shape follow-up amendment informed by
  T7 + the D22 MIX-overlay endgame (MIX mapping becomes data). Adding the
  `param` target later is additive; the SOURCE side needed no change —
  that's what freezing the source vocabulary buys.
- Validation (same discipline as `Mapping`): `deny_unknown_fields`; notes
  0..=127, CCs 0..=127, programs 0..=127; no source claimed twice within
  each space (note/cc, port-aware); label length cap. Invalid file →
  reject PUT with the message, keep running config.
- **No version field** (Q-V3): phase 1 code stamps `SCHEMA = 1` internally
  when it ships; a version gate on an unimplemented schema is ceremony.
  First real incompatible change adds the explicit gate, ProjectFile-style.

## Device personality pinning & T7 (new in v0.1)

The MKV guide documents DAW mode as six factory personalities (NC1/NC2/
M|h Mackie variants, N1/N2 plain-MIDI, N3 Ableton clip-launch), selected
SHIFT+DAW/PRESET + `<`/`>`, **not user-editable, no readback**. Our
keymap is only valid for one selection → **pin N1 or N2** (plain CC;
no Mackie/HUI protocol emulation — hard non-goal). Setup docs must state
the required unit configuration.

**T7 probe (hardware, before phases 3–4 consume knob/bank bindings):**
capture what each of the six personalities emits on both ports; whether
the documented SHIFT+pad knob submodes (Volume/Pan/Device/Sends) change
CC numbers or only meaning (device-local state is invisible to us — no
readback); confirm `<`/`>` bank-scroll messages for default bindings.
Non-goals reaffirmed: HUI protocol emulation; mirroring device-local knob
submode state; N3 pad semantics (conflict with D25 unified note rule).

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

### Phase 0 — decisions ✅ DONE (session 13)

Q1–Q7 answered (rulings at each Q below, rationale in scratchpad R14).
Q1–Q7 answered; schema then RECTIFIED to v0.1 the same session after the
MKV User Guide review (knobs are CC sources on the HUI port — v0's
note-only shape couldn't express them). v0.1 = frozen source object
(`{"note":N}` / `{"cc":N}` + optional `port`), typed sections sharing it,
command vocabulary incl. bank_prev/next, knobs section shaped-but-empty,
no version field. `voice` stays externally-tagged (Addition A); phase 2
must ship `POST /api/overlay/{mix|seq|none}` (Addition B). D26 in
scratchpad R14 + HANDOFF §7.

### Phase 1 — `DawKeymap` struct (oxygen crate, pure + serde)

- Structs: `DawKeymap`, `Source` (frozen v0.1 enum: note/cc + optional
  port), `VoiceBinding`, `CommandBinding`, `FaderButtonBinding`,
  `KnobBinding` (empty-allowed, no target semantics yet); `default()` =
  today's table; `validate()` with human errors; `from_json_str` /
  `to_json_string`; per-space duplicate detection as a method (needed by
  UI too).
- Tests (device-free, CI): defaults serialize→parse round-trip; unknown
  field rejected; dup sources rejected within each space (note/cc,
  port-aware); empty file → defaults; program/note/cc range checks.
- Acceptance: `cargo test -p pocketstage-oxygen` green; no engine changes.

### Phase 2 — persistence + HTTP control plane (server crate)

- `KeymapSlot` (newtype over `Mutex<Arc<DawKeymap>>`) in the server crate,
  shared with engine main() at boot; file path resolution mirrors the
  project store (`POCKETSTAGE_KEYMAP` env override for tests, like
  `POCKETSTAGE_PROJECT`).
- `GET /api/keymap` (current JSON + defaults for diffing), `PUT`
  (validate→persist→publish; 204/400), `POST /api/keymap/reset`.
  **REQUIRED (Addition B):** `POST /api/overlay/{mix|seq|none}` — same
  EngineCommand pattern as `/api/voice` — so a bad keymap can never trap
  the user in an overlay.
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

1. **Global vs per-project keymap.** ✅ RULING: **global**
   (`~/.pocketstage/keymap.json`) — bindings belong to the keyboard, not
   the song.
2. **Binding targets.** ✅ RULING: **keys + fader buttons only**; no
   knobs/raw-CC (tap-vs-ride semantics, nobody asked).
3. **Audition pitch for arbitrary voice bindings.** ✅ RULING: **WYSIWYG**
   — the pressed pitch sounds on the bound voice.
4. **Preset-mode overlay commands follow the remap?** ✅ RULING: **yes** —
   one map, one source of truth; PRESET instrument routing untouched.
5. **Capture mechanism** for the UI. ✅ RULING: **manual note-number entry
   in v1** (+ raw JSON escape hatch); capture is a fast-follow at most.
6. **Voice list editing**: ✅ RULING: **any GM program per slot** — and
   labels are free now: build the picker from `GET /api/presets` (the
   session-13 font catalog; Addition A keeps the serde form forward-compat).
7. **Scope**: ✅ RULING: **strictly DAW keymap this round** — device-layer
   `Mapping` is measured ground truth; UI-editing it without hardware
   probes invites silent misroute incidents. Phase 5 stays a separate
   ship decision.

## Test-first notes (repo law)

Every phase lands behind harness/unit tests before wiring: phase 1 is pure;
phase 2 uses the existing in-process axum router tests; phase 3 extracts a
pure `route_daw_note` so routing tables are CI-testable with zero MIDI.
Gates unchanged: `cargo test/clippy -D warnings/fmt` all four, every push.
