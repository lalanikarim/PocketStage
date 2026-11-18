# DAW keymap — configurable DAW-mode bindings (multiphase plan)

**Status: PHASES 1–4 DONE + hardware ear-checked (sessions 13–14b); the
remap loop validated live on the unit (hot reload, command toggles,
reset, corrupt-file boot — see HANDOFF §10 s14b). Q4 PRESET-mode check
PASSED (s14c). Schema v0.1 FROZEN; T7 takes 1–2 decoded — knob semantics
AMENDMENT RULED + IMPLEMENTED (D36, s17; drafted s14b; direction RESOLVED
CW=1/CCW=65; bank ‹/› = HUI notes 46/47; buttons 1–8 = HUI notes 0–7). **D28 ruled (s14c): NC1 is
the pinned personality** — the only one this unit exposes; its messages
are plain MIDI with Mackie-style numbering, so take-2 numbers ship as
defaults. **Phase 5 ruled to SHIP (D29, s14c) — IMPLEMENTED (D37, s17b):**
MappingSlot + GET/PUT/reset + hot-reloading feeder mappers + Device map card.
**D30 (s15): bank commands ship UNBOUND (‹/› are DAW-mode-only — silent in
PRESET, where MIX lives) and built-in defaults are music-port qualified.**
Decisions D26 + D28 + D29 + D30 + D36 + D37 in scratchpad R14 + HANDOFF §7.**
Requested session 12: "configure DAW mode key and button mappings so we can
manage it in a json file … what we don't want to allow remapping is Preset
mode." All seven open questions answered (rulings at each Q below).

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
  "fader_buttons": [],
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
  documented `<`/`>` track-bank scroll). Momentary semantics.
  **D30 (s15): bank commands SHIP UNBOUND — permanently, on this unit.** The
  T7 numbers exist (HUI notes 46/47) but are DAW-mode-only: guide v1.2 §10 and
  our own preset-mode inventory (`no MIDI output`) both say `‹`/`›` are
  device-local preset scrollers while PRESET is active — which is exactly the
  mode MIX lives in (D21). A default binding would be silent when needed and
  live only where MIX has no consumable controls. Nothing in the schema
  changes and users MAY still write a binding for them — but be precise about
  what that buys today: **the engine has no consumer** (there is no bank-
  paging `Event`; the feeder matches these commands to an empty arm). They are
  vocabulary kept for wire-format stability, so a hand-bound bank key parses,
  routes and does nothing until/unless MIX paging is designed — which D30 also
  argues against on usefulness grounds (8 channels fit one page).
- `fader_buttons`: per-button command bindings (Q2). T7 take 2 decoded:
  **buttons 1–8 = HUI-port notes 0–7** (button N → note N−1); the 9th
  button is **DAW BUTTON MODE** (a device-local mode cycler — never a
  binding target). Source shape `{"note":N,"port":"hui"}`. Per D28 these
  NC1 numbers are the measured source to bind against — but the section
  **ships EMPTY**: buttons already act as channel selectors through the
  built-in mapper path, so a row here only exists to give a button a
  *different* command (e.g. binding `›` on note 47 to `bank_next` is legal
  and fires in DAW mode only — see the `commands` bullet and D30).
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
M|h Mackie variants, N1/N2 plain-MIDI, N3 Ableton clip-launch), cycled
by **holding SHIFT and pressing DAW/PRESET** (guide v1.2 exact combo —
not `<`/`>`), **not user-editable, no readback**. Measured reality
(s14c): this unit exposes **NC1 only** — every documented selection path
stays NC1 across all ten preset slots; take-1's apparent plain-MIDI
segment re-reads as PRESET edit-mode traffic. **D28 supersedes "pin
N1/N2": pin NC1.** Its outputs are plain MIDI messages with Mackie-
style numbering (notes 0–7/46/47, tick constants, PB faders); consuming
them is mapping, while bidirectional Mackie-protocol emulation (LCD,
motor-fader updates) remains a hard non-goal. Setup docs: factory state
IS NC1 — no configuration required.

**T7 probe (hardware, before phases 3–4 consume knob/bank bindings):**
capture what each of the six personalities emits on both ports; whether
the documented SHIFT+pad knob submodes (Volume/Pan/Device/Sends) change
CC numbers or only meaning (device-local state is invisible to us — no
readback); confirm `<`/`>` bank-scroll messages for default bindings.
Non-goals reaffirmed: HUI protocol emulation; mirroring device-local knob
submode state; N3 pad semantics (conflict with D25 unified note rule).
**Executed: takes 1–2 fully decoded (s14b/c, raw logs + answers in the
device map); take 3 cancelled per D28 — see pinning section above.**

## Knob semantics amendment — RULED (D36, session 17; drafted s14b)

T7 take 1 measured the wire truth (specs/device-maps/oxygen-49-mkv.md):
in HUI-family personalities the knobs are **relative rotors** emitting
ch0 CC16–23 on the HUI port (observed values 1..65, never absolute),
while the one plain-MIDI personality emits ordinary **absolute CCs**
(7, 14, 42, 70–73, 91) on the music port. A knob binding must therefore
carry a value MODE, not assume positions:

```json
"knobs": [
  { "source": {"cc": 16, "port": "hui"},
    "target": { "kind": "relative", "step": 0.02,
                "param": { "track": { "index": 1, "field": "pan" } } } },
  { "source": {"cc": 71},
    "target": { "kind": "absolute",
                "param": { "global": "master_gain" } } }
]
```

Rulings (additive to frozen v0.1 — the SOURCE object is untouched, which is
exactly what freezing it bought us). Implemented session 17: schema in
`keymap.rs` (`KnobTarget`/`KnobKind`/`KnobParam`), consumption in the feeder +
`EngineCore::knob_param`, harness tests `knob_*`:

1. `knobs[]` entry = `{source, target}`, `deny_unknown_fields`.
2. `target.kind` ∈ `relative | absolute`. Relative: each CC message is a
   tick; param moves `step` normalized units per tick (default 0.02 —
   ruled with the leaf, 50 ticks per sweep). Absolute: CC value 0..=127
   maps linearly onto the param's normalized range.
3. ~~Relative DIRECTION encoding is NOT measured yet~~ **RESOLVED take 2
   (s14c): clockwise = value 1, counter-clockwise = value 65** — tick
   constants, one message per tick, no position data ever. Knob-group
   selectors (PAN/DEVICE/SENDS) verified wire-neutral: knobs stay CC16–23
   ch0 regardless of group.
4. ~~The `param` leaf stays OPEN~~ **RULED (D36): closed vocabulary v1,**
   normalized 0..=1 domain for every param:
   `{"track":{"index":N,"field":"gain|pan|reverb|chorus"}}` (1-based index,
   ≤ `MAX_TRACKS`) or `{"global":"master_gain|fx_reverb|fx_chorus"}`. The
   track fields are exactly the D22 MIX fields on purpose — this is the
   MIX-becomes-data endgame's namespace. Item-19 patch refs extend the
   *voice* field (Addition A), never this leaf. Model conversion reuses
   `mix_set_norm` (pan bipolar ±1, CC7/10/91/93 math shared with the MIX
   page and PUT adoption — a knob ride and a web save cannot diverge).
   Globals: `master_gain` → `set_master_gain`; `fx_*` → `set_global_fx`
   pushing ONLY the ridden field (session-10 space-param rule). Rides write
   the project and ride the D24 write-through lane.
5. **Defaults stay EMPTY.** Without per-personality knob CCs + direction
   data, any default is a guess — and guessed defaults are how silent
   misroutes happen (Q7).
6. **Consumption (D36).** The feeder matches every raw CC against `knobs[]`
   (space + number + port; channel is not in the frozen source object) and
   sends exactly one `Event::KnobParam`. A claimed CC **suppresses** the
   built-in MIX knob page for that same event — the map wins, the old
   binding goes inert (Q4 precedent). Live in both unit modes with no
   overlay gate: mapping is data. Relative moves read the CURRENT project
   value; when the field is `None` they start from its documented neutral
   (gains 1.0 — that is what `None` means; pan center; sends/FX mid-scale
   0.5, since soundfont defaults are unreadable — deterministic and neither
   silence nor full blast). Absolute tops out at normalized 1.0 (master
   boost past 1.0 stays a raw-JSON/PUT concern). Rows missing `target` are
   rejected — pre-D36 source-only rows were never consumed, and the
   keymap loader's never-fail-open rule turns such a file into warn +
   defaults.

(Fader-button defaults were gated on T7 take 2 for a while; that gate is long
closed — buttons measured as HUI notes 0–7, and the section still ships empty,
see the `fader_buttons` bullet.) Bank commands were ungated by take 2 and then
re-gated by **D30**: their wire numbers are mode-conditional (DAW-mode-only),
so they ship unbound — see the `commands` bullet above.

## Default source port qualification — D30 (session 15)

Built-in defaults now qualify every note source with `"port":"music"`:

- **Why.** Unqualified rows claim their number on *every* port, so the browser
  row on note 47 collided with any HUI-qualified binding on 47 — the built-in
  map failed its own `validate()` the moment a HUI default was added (measured:
  `voices[6] (Nylon Guitar) and commands[5] both claim note 47`). Qualifying
  the defaults frees the entire HUI note space for DAW-mode bindings.
- **Why it is safe.** Keys/pads transmit on port :0 in *both* unit modes
  (device map: "Keys + pads still appear on port 0"), so browser/overlay
  routing is byte-for-byte unchanged; only phantom HUI matches disappear.
- **Files are unaffected.** `port` is optional in the frozen v0.1 source
  object — hand-written maps keep their any-port semantics.

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

### Phase 1 — `DawKeymap` struct (oxygen crate, pure + serde) — ✅ DONE (s14)

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

### Phase 2 — persistence + HTTP control plane (server crate) — ✅ DONE (s14)

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

### Phase 3 — feeder consumption (engine main.rs) — ✅ DONE (s14; hardware
ear-check + Q4 PASSED s14b/c)

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

### Phase 4 — web UI ("DAW keys" card) — ✅ DONE (s14; remap loop verified
live s14b)

- Binding table editor: rows = voices + commands (+ buttons if Q2=yes);
  "capture" flow: click a row → press a key on the Oxygen → note arrives
  via a new `POST /api/keymap/capture` ack mechanism or ephemeral `/ws`
  frame carrying raw bound-note presses (Q5 — needs design: how does the
  browser see hardware presses? Feeder could push "keymap probe" notes to
  `/ws` while a capture session is armed via `POST /api/keymap/probe`).
- Conflict highlighting client+server (server re-validates, always).
- Reset-to-defaults button; import/export raw JSON (textarea) as escape hatch.
- Acceptance: full remap loop with zero file-handling by the user.

### Phase 5 — device-layer `Mapping` exposure — IMPLEMENTED (D37, session 17b)

- `GET/PUT /api/mapping` for oxygen `Mapping` (pad base, fader CCs…) with
  its existing `validate()`; UI advanced tab. Separate ship decision (Q7).
- **PREP LANDED (session 14b):** read-only `GET /api/mapping` — serves the
  two live boot mappings (music + HUI) with `"editable": false`; pure
  observation for debugging/T7. PUT owed per D29; the two open rulings
below bind it.
- **SHIPPED (session 17b, D37):** `MappingSlot` (exact `KeymapSlot`
  mirror) + `GET/PUT /api/mapping` + `POST /api/mapping/reset`; web UI
  "Device map" card (raw JSON — advanced surface by design). The two
  rulings below landed as follows:
  - **Loader:** one file, two sections — `MappingPair {music, hui}` in
    oxygen (sections optional; absent = that port's factory default),
    `POCKETSTAGE_MAPPING` env override, `~/.pocketstage/mapping.json`,
    never-fail-open (missing/corrupt/invalid → factory defaults + loud
    `[mapping] WARNING`). `MappingSlot::replace` holds its lock across
    validate→write→rename→publish, so memory and disk can never
    interleave (the D32 lesson applied from day one).
  - **Mapper state on swap: apply-at-read + flush, no transport gate.**
    Inspection showed the spec feared more state than exists: `Mapper`'s
    ONLY cross-message state is the shared-button tap detector
    (`pending_tap: Option<u8>`) — fader positions live in the engine's
    mix state, mode latching lives in the feeder. New pure
    `Mapper::flush_pending()` emits the held 127 as a real Fader action
    (exactly what the next non-tap event would have done); the feeder
    calls it on both mappers before rebuilding them on an Arc-pointer
    change. A remapped pad window mid-hold can orphan one note-off —
    accepted + documented, same risk class keymap hot-reload already
    lives with (Stop and the chatter guard bound the damage).
    Transport-stopped gating rejected: mapping edits are rare, user-paced
    device geometry, and lazy application on the next MIDI message keeps
    the audio thread untouched (R5).
  Gate 3 (T7) was satisfied long ago — takes 1–2 + D28 pinned NC1.
- **Open design rulings required before any PUT ships** (this is the part
  that made Phase 5 different from phases 1–4 — say so in the spec):
  1. **Loader**: mirror the keymap (`POCKETSTAGE_MAPPING` env override,
     `~/.pocketstage/mapping.json`, never-fail-open → measured factory
     defaults + loud warn). One file or two (music + HUI variants)?
     Leaning: one file, two sections, since both mappers boot together.
  2. **Mapper state on swap.** Unlike the DawKeymap (stateless per-message
     lookup), `Mapper` carries cross-message state: fader positions, the
     `127→0` shared-button tap detector, mode-pulse latching. Rebuilding a
     Mapper mid-performance can strand note-ons on channels that no longer
     exist — strictly worse than the semantic-layer remap risk. Options:
     (a) apply at transport-stopped only; (b) restart-only (simplest,
     honest); (c) pure `rebase(old, new)` carry-over fn for the stateful
     bits + panic-free fallthrough for the rest. Ruling owed; (a)+(c)
     hybrid is the likely shape.
  3. **Gate**: T7 must run first — UI-editing measured ground truth without
     knowing what each factory personality actually emits is exactly the
     silent-misroute incident Q7 warned about. T7 capture tooling shipped
     as `midi_probe t7` (labelled segments per personality + CC inventory
     summary for diffs).

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
2. **Binding targets.** ✅ RULING: **keys + fader buttons only** (v0);
   no knobs/raw-CC (tap-vs-ride semantics, nobody asked).
   *Rectified by v0.1 the same session:* CC is now a first-class source
   and the `knobs` section ships shaped-but-empty — knob **target**
   semantics remain out of scope (deferred to T7 + D22 MIX-as-data), so
   the spirit of the ruling holds. Addendum in scratchpad R14 governs.
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
   *(D29, s14c: superseded — Phase 5 ruled to SHIP; the preconditions
   below (loader, mapper-state-on-swap) still bind the implementation.)*

## Test-first notes (repo law)

Every phase lands behind harness/unit tests before wiring: phase 1 is pure;
phase 2 uses the existing in-process axum router tests; phase 3 extracts a
pure `route_daw_note` so routing tables are CI-testable with zero MIDI.
Gates unchanged: `cargo test/clippy -D warnings/fmt` all four, every push.
