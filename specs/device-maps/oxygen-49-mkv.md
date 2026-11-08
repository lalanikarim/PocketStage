# M-Audio Oxygen 49 MKV — measured MIDI behavior

Ground truth captured via `midi_probe watch` (raw ALSA sequencer), July 2026.
Two sessions: DAW mode (accidental) and **Preset mode** (DAW LED unlit).
This file is the spec for `pocketstage-oxygen`'s default mapping.

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
| Sustain pedal | deferred — no pedal on hand at test time | expect CC64 — verify someday |

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
3. **Sustain pedal**: DEFERRED — user has no pedal on hand. Expect CC64.
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
3. **Sustain pedal** CC (plug in, press) — expect CC64.
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
