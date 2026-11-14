# Patch catalog — soundfont sets, patch references, voice picker (plan)

**Status: PLANNED — decisions from session 12 chat folded in. Decision
slot D27 reserved.** Backlog items 18 (voice picker) + 19 (soundfont
sets). Implements-next per user; discuss-phase done, open questions
narrowed to Q-R1..Q-R3 (residuals, non-blocking for phase A).

## Goal

Replace the two hardcoded sound assumptions — `SF2_PATH` (one font at
boot) and `VOICES: [12 GM programs]` — with a runtime **catalog**:

- a **patch** = `{sfont_name, bank, program}` + display name;
- catalog built by *asking FluidSynth* (load + preset iteration), never
  by parsing sf2 files;
- voices AND tracks reference patches by stable names;
- drums may live in a **different soundfont** than melodic content;
- the default font + GM kit are always available and overrideable.

## Locked decisions (session 12 chat)

1. **Eager loading** of everything in the scan dir at boot; RAM footprint
   logged; lazy-loading revisited at Pi validation.
2. **Arbitrary banks** allowed (Sound Canvas MSB/LSB etc.); `bank: 128` =
   percussion. Audition is therefore essential (see preview channel).
3. **Dir-drop first, upload as its own phase** (user asked for upload —
   it ships, but after the core; size-capped, `.sf2/.sf3` only, LAN-trust
   model unchanged).
4. **Safe fallbacks everywhere** (user emphasis): missing patch → default
   font → any-font-with-that-bank → silence + loud log. A bad project,
   keymap, or uploaded font must NEVER fail boot or wipe the default
   voice/drums.
5. Voice-vs-track semantics unchanged from D25: exactly ONE active
   performance voice; tracks keep their own instruments for patterns/takes.

## Architecture facts (verified against our FluidSynth 2.3.4)

- `fluid_synth_program_select_by_sfont_name(synth, ch, name, bank, prenum)`
  — THE primitive: stable-name patch selection on any channel, including
  percussion-typed ones. This is what makes cross-sfont drums clean and
  removes sfont-id bookkeeping entirely.
- Preset iteration exists: `fluid_synth_get_sfont`, `fluid_sfont_get_name`,
  `fluid_sfont_iteration_start/next`, `fluid_preset_get_name/banknum/num` —
  catalog needs no sf2 parser and no new crate dependency (hand-rolled FFI
  policy intact; extend `ffi.rs` only).
- **Channels 4–7 are unused** (tracks 0–3, lead shares 0, pads/metronome
  8–15): **ch 7 becomes the PREVIEW channel** — patch audition never
  touches the live voice, project state, or write-through.

## Model changes (additive, v1-safe)

```rust
/// Named patch reference; resolved at boot/swap, never stale ids.
pub struct PatchRef { pub sfont: String, pub bank: u8 /*0..=128*/, pub program: u8 }
```

- `ActiveVoice::Preset(PatchRef)` — new serde variant beside `"drums"` /
  `{"program": n}`. Files USING the new variant stop loading on pre-D27
  builds; everything else round-trips unchanged (documented trade-off).
- `Project.drum_kit_patch: Option<PatchRef>` — `None` = GM kit from the
  default font (today's behavior, bit-for-bit).
- `InstrumentSlot`: wire the dead `soundfont` field + add `bank:
  Option<u8>`; resolution falls back to `gm_program` when unset.
- Fallback results are surfaced: engine mirrors "patch X unresolved, used
  Y" into stats/`/ws` so the UI can badge it (not just a log line).

## Drum support across soundfonts (the explicit answer)

Yes: pad channels 8–15 get `program_select_by_sfont_name(drum_kit_patch)`
— drums can come from a different font than anything melodic. Catalog
entries carry `has_percussion` (from bank-128 presence during iteration);
the kit picker filters to fonts that actually have kits. Fallback chain:

```
requested {X, 128} → X loaded+has 128? use it
  → default font has 128? use it + warn badge
    → any loaded font with 128? use it + warn badge
      → silence on those channels + LOUD log (never a failed boot)
```

Kit NOTE mapping stays GM (35–81) inside whichever kit is selected —
documented assumption for non-GM kits.

## Boot invariant (the "always available" rule)

Default soundfont (`FluidR3_GM.sf2` path constant + env override
`POCKETSTAGE_SOUNDFONT`) loads unconditionally BEFORE project/keymap are
read; scan-dir fonts load after and failures are per-file warnings. If the
DEFAULT font itself is missing, boot still aborts (nothing sane to fall
back to) with a message that names the exact expected path.

## HTTP surface

- `GET /api/patches` — catalog: `[{sfont, bank, program, name,
  has_percussion}]` + which are unresolved references right now.
- `POST /api/voice` — body = any `ActiveVoice` value (GM today, patch
  later); engine-side SelectVoice path; persists via write-through like
  the hardware browser. Fixes backlog item 18 immediately for GM.
- `POST /api/preview` — `{sfont, bank, program, note?, vel?}` on ch 7,
  fire-and-forget (note-off after a fixed gate). Never touches project.
- Phase E: `POST /api/soundfonts/rescan`; phase F: `POST
  /api/soundfonts/upload` (multipart, size cap, extension check, then
  implicit rescan).

## Phases (each ships standalone value)

| Phase | Deliverable | Notes / test-first |
|---|---|---|
| **A** ✅ (s13) | Voice picker UI + `POST /api/voice` (GM-only) | item 18 shipped. Shipped BETTER than planned: instead of the intended "full 128-name table", labels come from the loaded font — boot-time `Synth::preset_list` FFI iteration (main thread, pre-audio; symbols `fluid_preset_get_num`/`_get_banknum`) → immutable catalog on AppState → `GET /api/presets`. A hand-typed GM table was tried first and mislabelled most programs (session-13 post-mortem) — do not reintroduce static name tables. Phase B inherits: preset-iteration FFI already exists. |
| **B** | Catalog core: FFI additions, scan dir + eager load, preset iteration, `GET /api/patches`, `has_percussion` flags | fake-backend harness can't cover FFI — pure catalog-shape logic (dedup, naming, fallback resolution fn) unit-tested; thin FFI walk verified by device smoke |
| **C** | Patch references: `PatchRef` in model (+variants above), boot/swap/select via `program_select_by_sfont_name`, full fallback chain + `/ws` warning badge | resolution logic as a pure function over `Catalog` — table-driven CI tests incl. every fallback rung |
| **D** | Patch browser UI: grouped/searchable list, **audition on ch 7** (`POST /api/preview`), voice picker fed from catalog; track instrument pickers | item 19 closes |
| **E** | `POST /api/soundfonts/rescan` (diff by name; removed fonts STAY loaded until reboot — safe rule) + keymap `voices[]` → patch refs (feeds D26 phase E) | rescan during playback = add-only semantics |
| **F** | Soundfont upload endpoint (multipart → scan dir → rescan) | size cap ~512 MB, `.sf2/.sf3` only; LAN-trust unchanged |

## Residual open questions (non-blocking)

- **Q-R1**: preview gate length — fixed 0.5 s note-off timer on the
  engine side vs explicit `/api/preview/stop`? Lean: fixed + stop endpoint
  later if it annoys.
- **Q-R2**: when a track's patch is unresolved, does MIX overlay still
  work on its channel? (It should — CCs are font-independent; confirm in
  phase C tests.)
- **Q-R3**: catalog name collisions — two files with the same internal
  name? `fluid_synth_get_sfont_by_name` ambiguity: key catalog by FILE
  path, display file stem; document that internal names must be unique or
  last-load wins (FluidSynth semantics).

## Capacity model (voices ≠ soundfonts)

The 12 browser slots are *curation*, not architecture: patches and fonts
are many-to-many (12 slots from one font, or from twelve; catalog may
hold thousands). Runtime cost model:

- All loaded fonts are resident simultaneously; a voice switch between
  patches of different fonts is an O(1) `program_select_by_sfont_name`
  — no load hitch, given both fonts are loaded (why eager loading keeps
  switching instant).
- The ceiling is RAM, not a font count. Device tiers run the same code
  with different scan-dir contents: Pi-class = FluidR3 + one kit
  (~200–300 MB); richer boards load more.
- **Budget guard (cheap, ships with phase B/E):**
  `POCKETSTAGE_SFONT_BUDGET_MB` caps eager boot-loading of *scan-dir*
  fonts (best-effort within budget, per-font skip warnings on `/ws`).
  The default font + GM kit are exempt — the always-available invariant
  holds on every tier.
- **Lazy activation (deferred decision):** `sfload` on first use instead
  of boot — acceptable for low-RAM devices (voice-select is user-paced;
  one-time load hitch) but changes fallback semantics (mid-set load
  failure must degrade like an unresolved patch). Revisit with real Pi
  RAM numbers; other future hardware may make it moot.

## Out of scope (explicit)

- sf2 editing / authoring; per-note kit-layer remapping beyond GM layout;
  SF3 compression concerns; multi-out soundfonts; bank MSB/LSB split
  (we carry one u8 bank — FluidSynth's combined convention).
