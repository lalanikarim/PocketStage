# M-Audio Oxygen 49 MKV — measured MIDI behavior

Ground truth captured via `midi_probe watch` (raw ALSA sequencer), July 2026.
Two sessions: DAW mode (accidental) and **Preset mode** (DAW LED unlit).
This file is the spec for `pocketstage-oxygen`'s default mapping.

**Naming convention (session 14):** all user-facing note *labels* use the
Yamaha/DAW style with middle C = **C3** — so the Oxygen's bottom key
(note 36) is called **C1**. MIDI numbers are unchanged; older doc lines
written as "C2..G3" for the default browser mean notes 36..55, which read
C1..G2 under this convention. When in doubt, trust the number.

## Identity & ports

- USB `0763:0002`, three rawmidi/sequencer subdevices:
  - **port 0** — "USB MIDI" → Preset-mode controls, keys, pads
  - **port 1** — " Mackie/HUI" → DAW-mode controls (also active-sense + HUI notes)
  - **port 2** — " Editor SysEx" → explored, no usable protocol found (T2, below)

## Physical inventory (user-verified + guide v1.2)

49 keys · **9 faders, each with a button underneath** · **8 knobs** ·
**8 velocity pads** (SHIFT+pad 1 = bank select) · mod wheel + pitch lever ·
transport: loop / stop / play / record · octave −/+ · SHIFT · tempo/sync ·
note repeat/latch · `‹`/`‹ mode` · `›`/`› mode` · DAW/PRESET.

> The original brainstorm notes ("16 pads, 4 faders") were wrong. Corrected here.

## Preset mode (DAW LED **unlit**) — our control mode

Controls transmit on **MIDI channel 13** (unit was set to ch13; user-changeable).
Keys and pads stay on **channel 1**. All values full-range unless noted.

| Control | Messages | Notes |
|---|---|---|
| Keys (49) | Note On/Off, ch1 | octave buttons shift internally |
| Pads (8 physical) | Notes **36–51** = logical pads 1–16, ch1, velocity-sensitive | 2×4 rows; SHIFT+pad1 → 36–43, SHIFT+pad2 → 44–51 (probe C) |
| Mod wheel | CC1, ch1 | full sweep confirmed |
| Pitch lever | Pitch Bend, ch1, center 0x2000 | |
| Knobs 1–8 | **CC44, 45, 46, 47, 62, 63, 75, 76** (in physical order), ch13 | non-contiguous numbering! |
| Faders 1–9 | **CC33–41**, ch13 | physical order confirmed by sweep order |
| Fader buttons 1–8 | **CC49–56**, ch13, 0/127 | one press = 127 then 0 |
| Fader button 9 | **CC41 — collides with fader 9 position** | bare 127→0 pair; mapper tap-detects (probe B) |
| Transport ▶ ⏹ ⏺ 🔁 | HUI notes on port :1 (86/93/94/95) + system Start/Stop/Reset on port 0 | see probe table below — play vs record ONLY distinguishable via HUI notes |
| Octave −/+ , program ‹ › | **no MIDI output** (internal remapping) | cannot be used as bindings |
| Sustain pedal | **CC64, ch1** — `B0 40 7F` down, `B0 40 00` up (measured, session 16e) | plain switch: one message per transition, nothing repeated while held, no SysEx; shares the note channel (notes measured `90`/`80` ch1 in the same pass) |

### Always-on behaviors (both modes)

- Device **streams TimingClock continuously** (~45 msg/s ≈ 112 BPM, 24 ppq) even
  when "playing" nothing. Engine must consume-and-discard it — or use it as an
  external clock source later (tempo/sync button changes this).
- `Reset` (FF) observed around transport presses and port opens. Device also
  sends active-sensing on the HUI port periodically.
- ~~On opening the HUI port, device pushed CC112/113 val 127→0 bursts (state
  sync?)~~ — did **not** reproduce in the T6 session: ports opened clean. The
  earlier "bursts" were almost certainly echoes of real mode presses.

## T6 — MODE DETECTION: VERIFIED ✅ (session 8, attended capture)

The DAW and PRESET buttons each emit a **momentary pulse on the HUI port
(`hw:*,0,1`)** when pressed — two distinct signals, no state tracking needed:

| press | observed on HUI port | meaning |
|---|---|---|
| **DAW button** | `cc ch0 cc113 val127 → (≈200 ms) val0` | unit switched **into DAW mode** |
| **PRESET button** | `cc ch0 cc112 val127 → (≈200 ms) val0` | unit switched **into PRESET mode** |

Capture: 4 presses from a known PRESET start → exactly `113, 112, 113, 112`,
lockstep with the toggles, nothing else on any port. Mode signals appear ONLY
on the HUI port; keys/pads/faders keep flowing on port 0 regardless.

Mapper implications (unblocks D11 mode system v2):
- `ModeChanged(Daw)` / `ModeChanged(Preset)` from cc113/cc112 rising edges
  (val 127; swallow the falling 0 of the pair).
- Initial mode at engine boot is **unknown until the first toggle** — assume
  PRESET (the documented fallback); a mismatch self-heals on first press.
- Fader 1 (`ch13 cc33`) and knob 1 (`ch13 cc44`) confirmed flowing on port 0
  in PRESET mode, consistent with the T1 map.

## DAW mode (DAW LED lit)

- Controls route to **port 1** (Mackie/HUI): observed CC16–23 among others.
- Keys + pads **still appear on port 0** ch1.
- Per D11 this is the *manage* surface: mode entry is now detected via cc113
  (above), and it stops recording but never touches playback (D15).

## PROBES A/B/C — DONE (session 7). Results:

1. **Fader button 9 (probe B)**: sends **CC41 val127→val0 — collides with
   fader 9's position CC** (verified against a full sweep; button pairs contain
   no intermediate values, sweeps can't skip numbers). Mapper detects the bare
   `127→0` pattern → `Action::FaderButtonTap{9}` and swallows the position jumps.
2. **Transport table (probe A2 — identical in Preset AND DAW mode)**:
   | button | port :1 HUI note | port 0 system message |
   |---|---|---|
   | 🔁 loop | 86 (D6) | *nothing* |
   | ⏹ stop | 93 (A6) | Stop `FC` |
   | ▶ play | 94 (A#6) | Start `FA` + Reset·Reset |
   | ⏺ record | 95 (B6) | Start `FA` + Reset·Reset — **same as play!** |
   ⇒ Engine must listen to port `:1` notes to distinguish loop & record.
   `Mapping::transport_notes` + `hui_mapping()` implement this per-port.
3. **Sustain pedal**: DONE (session 16e, first time a pedal was on hand).
   Raw capture (`amidi -p hw:1,0 -d -T monotonic`, engine stopped — the engine's
   rawmidi read is exclusive, so snooping means stopping it; `aseqdump` on the
   sequencer port fails with EAGAIN while the engine runs):
   | event | bytes | decoded |
   |---|---|---|
   | pedal down | `B0 40 7F` | CC **64** (damper), ch1, value **127** |
   | pedal up | `B0 40 00` | CC64 ch1 value 0 |
   Five presses: exactly one `7F` + one `00` each, no bounce/repeats, no
   intermediate values (not graded), no clock/ActiveSensing side effects beyond
   the always-on clock. Same channel as the keybed notes in this unit state,
   which is what makes it usable as a straight per-channel damper pass-through.
   The keyboard's own display showed `127` on press, matching the wire.
4. **Pad banks (probes A2/C)**: physical layout 2 rows × 4; top row = higher notes.
   | | bottom row | top row |
   |---|---|---|
   | SHIFT+pad1 bank (“pads 1–8”) | 36–39 | 40–43 |
   | SHIFT+pad2 bank (“pads 9–16”) | 44–47 | 48–51 |
   Default state = bank 1. ⇒ **logical pads 1–16 = notes 36–51**, mapper default
   `pad_count: 16`. (Notes 44–51 shadow low piano keys — accepted tradeoff.)

## T2 — SysEx expedition (Editor port, LED write-back) — **DONE, INCONCLUSIVE**

Session run with `midi_probe send`/`watch`; user watching pad LEDs throughout.
Findings:

1. **Pad LEDs are local momentary state**: all 8 lit by default; a physically
   pressed pad's LED goes off for the duration of the press, back on on release.
   Identical behavior in Preset and DAW mode.
2. **Incoming notes do NOT drive the LEDs.** Note On/Off sent to port 2 (Editor)
   and port 0 (USB MIDI), in both modes, at the note the unit itself currently
   emits for that pad — zero LED change.
3. **Port 2 is silent both ways.** No reply to: standard identify (`F0 7E 00 06
   0F 00 F7`), non-standard ID inquiry, M-Audio (ID `0x43`) subcommand/dump/
   LED guesses, CC101, Program Change. The unit's only transmissions all session
   were its own physical controls.
4. **Verdict: no controllable LED protocol found** with available tools/docs
   (m-audio.com unreachable from dev box). → **Fallback adopted: LED-less UX**
   (transport buttons + audio cues per risk list). Revisit only if M-Audio docs
   or community SysEx references surface.
5. **Mapping re-confirmed, no drift**: a top-row pad press emitted
   `note_on ch1 key 40` — exactly the mapped value for that position
   (bank 1: bottom row 36–39, top row 40–43). The T2 session's initial
   "drift" suspicion was a misread; the unit matched the device map.

### Old open-questions list (superseded by A/B/C and T2 above)

1. **Fader button 9**: only CC49–56 appeared for 9 buttons — press #9 alone.
2. **Transport → message table**: which of loop/stop/play/record maps to
   Start/Stop/Reset (press each once, ≥2 s apart).
3. ~~**Sustain pedal** CC (plug in, press) — expect CC64.~~ — measured: CC64 ch1,
   127/0 (session 16e).
4. **Pad bank 2**: SHIFT+pad1 then press pads — do notes shift (+8? channel change?).
5. ~~T2: SysEx on port 2 (LED write-back protocol).~~ — done, inconclusive.

## Implications for `pocketstage-oxygen` (all implemented in part 1 unless noted)

- Default map must be **mode-aware and channel-tolerant** (controls ch13 by
  factory here, user can change per preset) → match CC numbers across channels,
  or read the unit's channel setting convention.
- Knob CC order `[44,45,46,47,62,63,75,76]` — hard-code as default, JSON-reassignable.
- Transport arrives as **system real-time messages**, not notes/CCs → mapper
  needs SystemMessage support from day one.
- Clock spam: sequencer input must be filtered (`EventFilter::ALL except...`
  or discard TimingClock in the pump) — 45 Hz otherwise.

## T7 — DAW-mode personality sweep (take 1, session 14)

Tool: `midi_probe t7` (labelled segments + CC-inventory summary). Raw log:
`t7-capture-2026-11-14.log` (20 899 events). **Take-1 caveats:** PRESET-SHIFT
segment partial; segment→personality labels tentative (click positions not
counted at the unit); `<`/`>` presses not isolated from the general sweeps.

**Findings:**

1. **Baseline re-confirmed** (seg 0): PRESET mode emits exactly the factory
   map — faders CC33–41, knobs CC44–47+62/63/75/76, fader buttons CC49–56
   (clean 0/127 press+release pairs), mod CC1, all on ch13 (1-based).
2. **Five of six DAW segments are the same family**: knobs = endless rotary
   encoders → **HUI port, ch0, CC16–23**, observed values 1..65 (relative
   tick-style, never absolute); faders → **pitch-bend on the HUI port**
   (~907 events per sweep, NOT CC); strip/fader buttons → **Note On/Off on
   the HUI port** (notes ≈16–47, Mackie-style), NOT CC.
3. **One personality is plain MIDI on the music port** (seg 4): knobs/faders
   arrive as GM-flavoured absolute CCs on ch0 — 7, 14, 42, 70, 71, 72, 73,
   91 — HUI port silent apart from mode pulses. This is the profile our
   N1/N2 pinning wants; which factory name it is needs take 2.
4. **Mode pulses re-confirmed**: DAW/PRESET buttons = cc113/cc112 momentary
   on the HUI port (T6 holds).
5. Some PRESET+SHIFT combos emit **CC123 (all notes off)** on the music
   port — worth knowing before anyone wonders about mid-phrase note kills.

**Implications for the DAW keymap (D26):**

- `fader_buttons` defaults must be **note sources on the HUI port** — the
  spec example's `{"cc":49,"port":"hui"}` was wrong; v0.1 schema handles
  the correct shape fine, only the doc example mis-guessed.
- Knob targets will need **relative-encoder semantics** (value = tick
  delta-ish, not position). Freezing knobs unfrozen in v0.1 is validated —
  a naive absolute-value param design would have been wrong.
- Fader *position* on HUI-family personalities rides pitch-bend: NOT
  expressible as a v0.1 source. Accepted — PocketStage's MIX rides PRESET-
  mode faders (CC33–41), and pinning plain-MIDI keeps us off the Mackie
  protocol entirely.

**Take 2 owed (FULFILLED — see "T7 take 2" section below for all of
these):** full PRESET-SHIFT sweep; isolate `<`/`>` presses (bank
command defaults); identify N1/N2 by unit display + click count from a
known start; per-button HUI note table (which physical button =
which note); **transport buttons (play/stop/record/loop) one press per
personality** — A2 measured notes 94/93/95/86 on the HUI port for one
configuration only; confirm they are personality-invariant (or capture
what N1/N2 send instead, e.g. system-RT start/stop).

## Field notes (session 14b) — unit states that masquerade as software bugs

- The unit can enter an **octave-DUAL layer state** via SHIFT combos:
  one press → two note-ons an octave apart on the music port (pads
  unaffected). Undocumented in guide v1.2. A power-cycle cleared it.
  Symptom: "repeating dual notes" / "everything sounds doubled".
- **Octave shift survives unit power-cycle** and its buttons emit no
  MIDI (silent internal remap, as documented) — a shifted unit moves
  every keymap binding off its physical key with zero log evidence.
  Before debugging any "bindings stopped working" report: press the
  bottom key at claimed octave 0 and confirm it sends note **36**
  (`midi_probe watch`). Both states were caused by T7 button-mashing;
  expect them after any exploratory session.
- Project-wide label convention (s14b): middle C = **C3** (Yamaha/DAW
  style), bottom key = **C1**. Wire numbers unchanged; when names and
  numbers disagree, trust the number.

## T7 take 2 (session 14c) — decoded, guide cross-referenced

Raw log: `t7b-capture-2026-11-15.log`. Systematic sweeps + the v1.2 user
guide PDF (`~/Documents/Oxygen Series MKV - User Guide - v1.2.pdf`, extract
tooling: pdftotext). **Decoded, all in NC1 (the only personality we could
select):**

- **Knob direction encoding RESOLVED**: rotors emit only two constants —
  **CW = 1, CCW = 65** — one message per tick, no position. Take-1's
  "1..65 range" was never a range.
- **Bank scroll <>**: ‹ = HUI-port **note 46**, › = HUI-port **note 47**
  (clean press+release pairs). **Correction (s15, D30):** these numbers are
  only live in **DAW mode**. Guide v1.2 §10 is explicit — "In DAW mode, < and >
  will scroll through track banks… In Preset Mode, the < and > buttons will
  scroll through presets" — and our own preset-mode inventory lists these very
  buttons as **no MIDI output**. So take 2 measured them where they are Mackie
  controls; in PRESET mode they put zero bytes on the wire and can never drive
  a host-side MIX page. Do NOT ship them as keymap defaults (see D30).
- **Fader buttons 1–8** = HUI-port **notes 0–7** (button N → note N−1),
  one press each, cleanly separated.
- **"Button 9" = DAW BUTTON MODE**, not a control: SHIFT + it cycles
  fader-button *meaning* (Track Rec Arm / Select / Mute / Solo,
  device-local). Closes the T6 open question — after pressing it, our
  buttons went silent on the wire in NC1. Setup docs must leave this at
  its default mode.
- **DAW KNOB CONTROL**: pad secondary functions (guide: SHIFT+pads;
  physically labeled PAN/DEVICE/SENDS) — direct presses emit music-port
  ch0 notes **PAN=36, DEVICE=37, SENDS=38**. Knob wire numbers do NOT
  change across groups (verified: CC16–23 ch0 throughout). Device-local
  group state; PocketStage ignores it.
- **PAD BANK 1/2** = SHIFT+pad1/pad2, sticky — bank 2 sends notes 44–51
  (this, not a bug, explains the pad-offset sightings in session 14b).
- **Transport personality-invariant confirmed**: play 94, stop 93,
  record 95, loop 86 (HUI notes) — matches A2 exactly. Bonus guide fact:
  double-press STOP = stop + rewind to start.
- **Take-1 CC123 mystery solved from the guide**: SHIFT + PANIC sends
  All-Notes-Off on all 16 channels.

**Personality selection (guide, unresolved on our unit):** "press and
hold the SHIFT button and the DAW/PRESET button" cycles NC1 → NC2 → M|h
→ N1 → N2 → N3. Session 14c attempts stayed on NC1 (all P01–P10 slots
enter DAW as NC1). Take-1's plain-MIDI segment (music-port GM-ish CCs
7/14/42/70–73/91) was very likely an **N-personality reached by
accident** mid-mash — meaning N1/N2 ARE reachable and need a deliberate
re-capture. **Take 3 (conditional):** reach N1/N2 intentionally via the
guide combo, verify on the display, full sweep → that personality's
numbers are what keymap DEFAULTS must use (D26 pins plain-MIDI; NC1 is
Mackie and off-limits for defaults).

**Resolution (s14c, D28):** N-personalities proved unreachable on this
unit's firmware (guide combo hold-SHIFT+DAW/PRESET does nothing; take-1's
"plain MIDI" segment re-read as likely PRESET *edit-mode* traffic).
**NC1 is pinned.** Its messages are plain MIDI with Mackie-style
numbering — reading notes 0–7/46/47 and tick constants is mapping, not
the bidirectional Mackie protocol (LCD/motor faders) which remains a
non-goal. Take-2 numbers are the shipped keymap default source; MIX
keeps riding PRESET-mode faders CC33–41. Setup requirement: none —
factory state IS NC1. Take 3 cancelled.
