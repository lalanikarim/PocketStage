# M-Audio Oxygen 49 MKV — measured MIDI behavior

Ground truth captured via `midi_probe watch` (raw ALSA sequencer), July 2026.
Two sessions: DAW mode (accidental) and **Preset mode** (DAW LED unlit).
This file is the spec for `pocketstage-oxygen`'s default mapping.

## Identity & ports

- USB `0763:0002`, three rawmidi/sequencer subdevices:
  - **port 0** — "USB MIDI" → Preset-mode controls, keys, pads
  - **port 1** — " Mackie/HUI" → DAW-mode controls (also active-sense + HUI notes)
  - **port 2** — " Editor SysEx" → unexplored (T2 target)

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
- On opening the HUI port, device pushed CC112/113 val 127→0 bursts (state sync?).

## DAW mode (DAW LED lit) — for reference only

- Controls route to **port 1** (Mackie/HUI): observed CC16–23 among others.
- Keys + pads **still appear on port 0** ch1.
- We do NOT rely on this mode; it's a fallback surface at most.

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

## PENDING PROBES

### B — T2 SysEx expedition (Editor port, LED write-back) — **PENDING**

`midi_probe send hw:3,0,2 <hex>` is built and ready. Plan: verify the port accepts
writes, try M-Audio-family SysEx candidates while watching pad LEDs; check if the
unit replies to any inquiry (identity dump `F0 7E .. F7`). Worst case fallback:
LED-less UX.

### Old open-questions list (superseded by A/B above)

1. **Fader button 9**: only CC49–56 appeared for 9 buttons — press #9 alone.
2. **Transport → message table**: which of loop/stop/play/record maps to
   Start/Stop/Reset (press each once, ≥2 s apart).
3. **Sustain pedal** CC (plug in, press) — expect CC64.
4. **Pad bank 2**: SHIFT+pad1 then press pads — do notes shift (+8? channel change?).
5. T2: SysEx on port 2 (LED write-back protocol).

## Implications for `pocketstage-oxygen` (all implemented in part 1 unless noted)

- Default map must be **mode-aware and channel-tolerant** (controls ch13 by
  factory here, user can change per preset) → match CC numbers across channels,
  or read the unit's channel setting convention.
- Knob CC order `[44,45,46,47,62,63,75,76]` — hard-code as default, JSON-reassignable.
- Transport arrives as **system real-time messages**, not notes/CCs → mapper
  needs SystemMessage support from day one.
- Clock spam: sequencer input must be filtered (`EventFilter::ALL except...`
  or discard TimingClock in the pump) — 45 Hz otherwise.
