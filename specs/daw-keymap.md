# DAW keymap — configurable DAW-mode bindings (multiphase plan)

**Status: PHASES 1–4 DONE + hardware ear-checked (sessions 13–14b); the
remap loop validated live on the unit (hot reload, command toggles,
reset, corrupt-file boot — see HANDOFF §10 s14b). Q4 PRESET-mode check
PASSED (s14c). Schema v0.1 FROZEN; T7 takes 1–2 decoded — knob semantics
AMENDMENT DRAFTED below (direction RESOLVED CW=1/CCW=65; bank ‹/› = HUI
notes 46/47; buttons 1–8 = HUI notes 0–7). Remaining data gate: **take 3
(conditional)** — reach pinned N1/N2 personality deliberately and re-
sweep; its numbers ship as defaults (all take-2 numbers are NC1/Mackie).
Phase 5 = open ship decision. Decision D26 in scratchpad R14 + HANDOFF §7.**
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
  "fader_buttons": [
    { "source": { "note": 16, "port": "hui" }, "command": "bank_prev" },
    { "source": { "note": 17, "port": "hui" }, "command": "bank_next" }
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
- `fader_buttons`: per-button command bindings (Q2). T7 take 2 decoded:
  **buttons 1–8 = HUI-port notes 0–7** (button N → note N−1); the 9th
  button is **DAW BUTTON MODE** (a device-local mode cycler — never a
  binding target). Source shape `{"note":N,"port":"hui"}`. Caveat: these
  are NC1 numbers; if take 3 lands on N1/N2, defaults come from there.
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
not `<`/`>`), **not user-editable, no readback**. Our keymap is only
valid for one selection → **pin N1 or N2** (plain CC; no Mackie/HUI
protocol emulation — hard non-goal). Setup docs must state the required
unit configuration. STATUS s14c: attempts on our unit stayed on NC1
(all ten preset slots enter DAW as NC1); take-1 captured a plain-MIDI
control set by accident, proving an N-personality IS reachable —
**take 3** must land on it deliberately (device map §T7 take 2).

**T7 probe (hardware, before phases 3–4 consume knob/bank bindings):**
capture what each of the six personalities emits on both ports; whether
the documented SHIFT+pad knob submodes (Volume/Pan/Device/Sends) change
CC numbers or only meaning (device-local state is invisible to us — no
readback); confirm `<`/`>` bank-scroll messages for default bindings.
Non-goals reaffirmed: HUI protocol emulation; mirroring device-local knob
submode state; N3 pad semantics (conflict with D25 unified note rule).

## Knob semantics amendment — DRAFT (session 14b, pending ruling)

T7 take 1 measured the wire truth (specs/device-maps/oxygen-49-mkv.md):
in HUI-family personalities the knobs are **relative rotors** emitting
ch0 CC16–23 on the HUI port (observed values 1..65, never absolute),
while the one plain-MIDI personality emits ordinary **absolute CCs**
(7, 14, 42, 70–73, 91) on the music port. A knob binding must therefore
carry a value MODE, not assume positions:

```json
"knobs": [
  { "source": {"cc": 16, "port": "hui"},
    "target": { "kind": "relative", "step": 2, "param": {} } },
  { "source": {"cc": 71},
    "target": { "kind": "absolute", "param": {} } }
]
```

Proposed rulings (additive to frozen v0.1 — the SOURCE object is
untouched, which is exactly what freezing it bought us):

1. `knobs[]` entry = `{source, target}`, `deny_unknown_fields`.
2. `target.kind` ∈ `relative | absolute`. Relative: each CC message is a
   tick; param moves `step` units per tick (default 1). Absolute: CC
   value 0..=127 maps linearly onto the param range.
3. ~~Relative DIRECTION encoding is NOT measured yet~~ **RESOLVED take 2
   (s14c): clockwise = value 1, counter-clockwise = value 65** — tick
   constants, one message per tick, no position data ever. Knob-group
   selectors (PAN/DEVICE/SENDS) verified wire-neutral: knobs stay CC16–23
   ch0 regardless of group.
4. The `param` leaf stays OPEN (shared namespace with the D22 MIX-
   becomes-data endgame and item-19 patch refs): first real consumer
   rules it; knobs stay shaped-but-unconsumed until then.
5. **Defaults stay EMPTY.** Without per-personality knob CCs + direction
   data, any default is a guess — and guessed defaults are how silent
   misroutes happen (Q7).

Bank commands (`bank_prev`/`bank_next`) and fader_button defaults remain
equally gated on take 2 (`<>` emissions not yet isolated from the
sweeps; buttons confirmed as notes, see correction above).

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
- **PREP LANDED (session 14b):** read-only `GET /api/mapping` — serves the
  two live boot mappings (music + HUI) with `"editable": false`; pure
  observation for debugging/T7, Q7 untouched. PUT stays unbuilt.
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

## Test-first notes (repo law)

Every phase lands behind harness/unit tests before wiring: phase 1 is pure;
phase 2 uses the existing in-process axum router tests; phase 3 extracts a
pure `route_daw_note` so routing tables are CI-testable with zero MIDI.
Gates unchanged: `cargo test/clippy -D warnings/fmt` all four, every push.
