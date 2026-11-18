# Patch catalog — soundfont sets, patch references, voice picker (plan)

**Status: IN PROGRESS — phases A–D SHIPPED (sessions 13, 18, 18b, 18c),
phases E–F planned.**
Decisions from session 12 chat folded in; D27 ruled + recorded (phase B, s18).
Backlog items: 18 (voice picker) ✅ closed by phase A, 19 (soundfont sets)
✅ CLOSED by phase D. Residual open questions all answered (Q-R1 fixed gate
shipped in D; Q-R2/Q-R3 answered in phases C/B). Next up: phase E — rescan +
keymap voice refs; resumes here per HANDOFF §9.

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
| **B** ✅ (s18) | Catalog core: FFI additions, scan dir + eager load, preset iteration, `GET /api/patches`, `has_percussion` flags | Shipped as planned. Pure logic lives in `pocketstage-model/src/catalog.rs` (`Catalog`/`PatchInfo`/`FontInfo`/`Resolution` + `plan_font_loads` budget planner; 8 unit tests: dedup keeps first, bank>128/program>127 dropped, every fallback rung, kit chain, budget skip-with-reason). FFI gained only `fluid_sfont_get_name`. **Finding:** on FluidSynth 2.3.4 the internal sfont name IS the load-path (`/usr/share/sounds/sf2/FluidR3_GM.sf2`), so Q-R3 collisions are near-impossible for distinct files — the last-load-wins warning stays as belt-and-braces. Scan dir `POCKETSTAGE_SFONT_DIR` (default `~/.pocketstage/soundfonts`), budget `POCKETSTAGE_SFONT_BUDGET_MB`, default font exempt + path-overridable via `POCKETSTAGE_SOUNDFONT`; corrupt file → visible skipped entry, boot never dies (device-smoke proven: corrupt skip + 141 MB second font loaded). Catalog lives outside the project format — `SCHEMA_VERSION` untouched. |
| **C** ✅ (s18b) | Patch references: `PatchRef` in model (+variants above), boot/swap/select via `program_select_by_sfont_name`, full fallback chain + `/ws` warning badge | Shipped as planned. `PatchRef` = type alias over catalog's `PatchRequest` (one vocabulary); `ActiveVoice::Preset` (externally tagged `{"preset":{…}}`), `Project.drum_kit_patch`, `InstrumentSlot.bank` — all serde-defaulted, old files bit-identical. Resolution runs through the phase-B pure `Catalog` resolvers at boot/swap/scene-launch/voice-select; badges publish to `LiveStatus::patch_notes` (cap 8) and ride every `/ws` frame as `patch_notes`; voice mirror gains `VOICE_PATCH` sentinel. Kit = one `select_patch` per pad channel 8–15, `None` = zero synth calls (today's boot kit). Active-preset unresolved keeps the lead's previous sound + badge (silence would lie). Q-R2 PINNED by test: unresolved track still gets its full CC block. 4 model + 8 engine + 1 server test; live smoke: POST preset → 202, `voice=patch badges=0`, write-through persisted the ref; ghost font → badge; bank 200 → 400. Side effect: `ActiveVoice` lost `Copy` (boxed variant) — `Event` is Clone-only now, lanes are mpsc anyway. |
| **D** ✅ (s18c) | Patch browser UI: grouped/searchable list, **audition on ch 7** (`POST /api/preview`), voice picker fed from catalog; track instrument pickers | item 19 CLOSED. Shipped as planned + badges: search narrows the list (render capped at 300 rows); skipped fonts render visibly with their skip reason (D27 rule 4). Preview = fixed 0.5 s gate, engine-side note-off, latest-wins cut, PREVIEW_CH=7 (shared with track 8 — accepted, next swap/scene re-programs it), exact-match at BOTH HTTP (404) and core (silent return) so an audition can never substitute; zero project state touched (pinned: no write-through edit fires). `/api/health` gains `default_font`; the voice dropdown skips rows duplicating a plain GM program and groups the rest per font (`patch:`-prefixed values POST `{preset:{…}}`); track rows get a catalog-patch box on one shared `<datalist>` (empty = plain GM, unchanged). `/ws` `patch_notes` render as badges; a badge forces a voice-dropdown re-sync so the picker never lies. 3 core + 2 server harness/route tests. Live smoke: 183 fonts/9951 patches; tone captured end-to-end via sink monitor (440 Hz burst ≈ gate); 404 ghost / 400 range; xruns=0. |
| **E** | `POST /api/soundfonts/rescan` (diff by name; removed fonts STAY loaded until reboot — safe rule) + keymap `voices[]` → patch refs (this patch-catalog phase feeds the D26 keymap's `voice` field via Addition A's externally-tagged enum) | rescan during playback = add-only semantics |
| **F** | Soundfont upload endpoint (multipart → scan dir → rescan) | size cap ~512 MB, `.sf2/.sf3` only; LAN-trust unchanged |

## Residual open questions (non-blocking)

- **Q-R1** (answered, phase D): fixed 0.5 s engine-side gate shipped
  (`PREVIEW_GATE_SAMPLES`, latest-wins cut); `/api/preview/stop` deferred
  until someone is annoyed enough to ask.
- **Q-R2** (answered, phase C): YES — pinned by
  `unresolved_track_patch_falls_back_to_gm_and_keeps_its_ccs`: resolution
  only touches the program line; the CC block is font-independent.
- **Q-R3** (answered in phase B): on FluidSynth 2.3.4 the internal sfont
  name IS the load-time file path, so distinct files cannot collide; the
  catalog keeps that name and boot still warns if a name repeats (a font
  loaded twice by identical path is filtered — canonical-path compare).
  Last-load-wins remains FluidSynth's documented fallback semantics.

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
