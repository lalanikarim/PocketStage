//! Patch catalog (D27, backlog item 19 phase B).
//!
//! Pure data shapes + resolution logic for the runtime soundfont catalog:
//! which fonts are loaded, which patches each exposes, and how a named
//! patch request falls back when its exact reference cannot be honored.
//! Built by the engine at boot from a FluidSynth FFI walk (the sf2 file is
//! never parsed here); served verbatim by `GET /api/patches`.
//!
//! This is NOT part of the project file format — `SCHEMA_VERSION` is
//! unaffected. Phase C adds `PatchRef` (project-side references) on top.

use serde::{Deserialize, Serialize};

/// Percussion bank in FluidSynth's combined convention (bank 128).
pub const PERCUSSION_BANK: u8 = 128;

/// One preset straight off the FFI walk: `(bank, program, name)` as raw
/// c_ints — before range filtering (see [`Catalog::add_font`]).
pub type RawPreset = (i32, i32, String);
/// One loaded font straight off the FFI walk: `(internal name, path, presets)`.
pub type RawFont = (String, String, Vec<RawPreset>);

/// One patch of one loaded font. Names come from the FONT — a hand-typed
/// name table once mislabelled most GM picks (session 13); never do that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchInfo {
    /// Internal soundfont name (FluidSynth's handle for `program_select_
    /// by_sfont_name`), not the file name.
    pub sfont: String,
    pub bank: u8,
    pub program: u8,
    pub name: String,
    /// Convenience flag: `bank == PERCUSSION_BANK`.
    pub has_percussion: bool,
}

/// One font in the catalog — including load attempts that did NOT succeed,
/// so the UI can badge exactly what went wrong instead of showing silence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontInfo {
    /// Internal name read from the font (fallback: file stem when FluidSynth
    /// gave none — never a guessed display name).
    pub name: String,
    pub path: String,
    pub presets: usize,
    /// Font exposes at least one bank-128 preset (kit picker filters on it).
    pub has_percussion: bool,
    /// `Some(reason)` = not loaded (corrupt file / over the RAM budget).
    pub skipped: Option<String>,
}

/// A patch requested by stable name — what phase C's `PatchRef` resolves
/// against. Plain data so tests and (later) model fields share one shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchRequest {
    pub sfont: String,
    pub bank: u8,
    pub program: u8,
}

/// Outcome of [`Catalog::resolve`]: exact hit, a documented substitution,
/// or nothing at all. Fallback is always VISIBLE (D27 rule 4: surface it,
/// never silently swap instruments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<'c> {
    Resolved(&'c PatchInfo),
    FellBack {
        requested: PatchRequest,
        used: &'c PatchInfo,
        note: String,
    },
    Unresolved(PatchRequest),
}

/// The whole catalog as built at boot (immutable afterwards; phase E's
/// rescan replaces the Arc wholesale). Fonts keep scan order — the "any
/// font with that bank/program" fallback rung is first-match by that order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub fonts: Vec<FontInfo>,
    pub patches: Vec<PatchInfo>,
}

impl Catalog {
    /// Build from raw FFI walk output: one `(internal name, path, presets)`
    /// entry per loaded font, presets as `(bank, program, name)`.
    ///
    /// Rules (all unit-tested below):
    /// - presets with `bank > 128` or `program > 127` are dropped — we
    ///   carry the combined convention in one u8 each; exotic MSB/LSB splits
    ///   are out of scope (spec "Out of scope").
    /// - duplicate `(sfont, bank, program)` rows keep the FIRST occurrence.
    /// - patch order follows font order, then (bank, program) as reported.
    pub fn build(entries: Vec<RawFont>) -> Self {
        let mut cat = Catalog::default();
        for (name, path, presets) in entries {
            cat.add_font(name, path, presets);
        }
        cat
    }

    /// Add one successfully loaded font (boot loads fonts one at a time,
    /// so `build` and the incremental boot path share one implementation).
    pub fn add_font(&mut self, name: String, path: String, presets: Vec<RawPreset>) {
        let mut count = 0usize;
        let mut perc = false;
        for (bank, program, pname) in presets {
            if !(0..=PERCUSSION_BANK as i32).contains(&bank) || !(0..=127).contains(&program) {
                continue;
            }
            let (bank, program) = (bank as u8, program as u8);
            let dup = self
                .patches
                .iter()
                .any(|p| p.sfont == name && p.bank == bank && p.program == program);
            if dup {
                continue;
            }
            let is_perc = bank == PERCUSSION_BANK;
            perc |= is_perc;
            count += 1;
            self.patches.push(PatchInfo {
                sfont: name.clone(),
                bank,
                program,
                name: pname,
                has_percussion: is_perc,
            });
        }
        self.fonts.push(FontInfo {
            presets: count,
            has_percussion: perc,
            name,
            path,
            skipped: None,
        });
    }

    /// True if an internal font name is already present — FluidSynth's
    /// by-name selection resolves collisions to the LAST loaded font
    /// (Q-R3); callers warn loudly.
    pub fn has_font(&self, name: &str) -> bool {
        self.fonts.iter().any(|f| f.name == name)
    }

    /// Append a font that never made it into the synth (load failure or
    /// budget skip) — visible in `GET /api/patches` with its reason.
    pub fn push_skipped(&mut self, name: String, path: String, reason: String) {
        self.fonts.push(FontInfo {
            name,
            path,
            presets: 0,
            has_percussion: false,
            skipped: Some(reason),
        });
    }

    pub fn find(&self, sfont: &str, bank: u8, program: u8) -> Option<&PatchInfo> {
        self.patches
            .iter()
            .find(|p| p.sfont == sfont && p.bank == bank && p.program == program)
    }

    /// First patch of `sfont` in the percussion bank (one kit per font;
    /// program numbers inside the kit stay GM 35–81 by assumption).
    pub fn find_kit(&self, sfont: &str) -> Option<&PatchInfo> {
        self.patches
            .iter()
            .find(|p| p.sfont == sfont && p.bank == PERCUSSION_BANK)
    }

    /// The D27 fallback chain for a melodic/patch reference:
    /// exact `{sfont,bank,program}` → default font same bank+program →
    /// any loaded font with that bank+program (catalog order) → unresolved.
    pub fn resolve(&self, req: PatchRequest, default_font: &str) -> Resolution<'_> {
        if let Some(p) = self.find(&req.sfont, req.bank, req.program) {
            return Resolution::Resolved(p);
        }
        if req.sfont != default_font
            && let Some(p) = self.find(default_font, req.bank, req.program)
        {
            return Resolution::FellBack {
                note: format!(
                    "'{sfont}' {bank}:{program} not loaded — using default font '{default_font}'",
                    sfont = req.sfont,
                    bank = req.bank,
                    program = req.program,
                ),
                requested: req,
                used: p,
            };
        }
        if let Some(p) = self
            .patches
            .iter()
            .find(|p| p.bank == req.bank && p.program == req.program)
        {
            return Resolution::FellBack {
                note: format!(
                    "'{sfont}' {bank}:{program} not loaded — using '{}' (only source)",
                    p.sfont,
                    sfont = req.sfont,
                    bank = req.bank,
                    program = req.program,
                ),
                requested: req,
                used: p,
            };
        }
        Resolution::Unresolved(req)
    }

    /// Fallback chain for the drum kit: requested font's kit → default
    /// font's kit → any loaded font with a bank-128 preset → unresolved.
    /// `None` from the caller means "default font" was requested.
    pub fn resolve_kit(&self, requested: Option<&str>, default_font: &str) -> Resolution<'_> {
        let req = PatchRequest {
            sfont: requested.unwrap_or(default_font).to_string(),
            bank: PERCUSSION_BANK,
            program: 0, // one kit per font; the number inside is GM-mapped
        };
        // Rung 1: the requested font (or the default when none was given).
        if let Some(p) = self.find_kit(&req.sfont) {
            return Resolution::Resolved(p);
        }
        // Rung 2: the default font's kit.
        if let Some(p) = self.find_kit(default_font) {
            return Resolution::FellBack {
                note: format!(
                    "kit '{}' has no percussion — using default font '{default_font}'",
                    req.sfont
                ),
                requested: req.clone(),
                used: p,
            };
        }
        // Rung 3: any loaded font that carries a bank-128 preset.
        if let Some(p) = self.patches.iter().find(|p| p.bank == PERCUSSION_BANK) {
            return Resolution::FellBack {
                note: format!(
                    "no requested kit loaded — using '{}' (only kit source)",
                    p.sfont
                ),
                requested: req,
                used: p,
            };
        }
        Resolution::Unresolved(req)
    }
}

/// Eager-load budget plan (ships with phase B/E): keep fonts in the given
/// (already sorted!) order while they fit into `budget_bytes`; `None` means
/// unlimited. The DEFAULT font is exempt and must never be passed here —
/// the always-available invariant holds on every tier (D27).
pub fn plan_font_loads(
    sized: &[(String, u64)],
    budget_bytes: Option<u64>,
) -> Vec<(String, bool, Option<String>)> {
    let mut total = 0u64;
    sized
        .iter()
        .map(|(path, size)| match budget_bytes {
            None => (path.clone(), true, None),
            Some(budget) if total.saturating_add(*size) <= budget => {
                total += size;
                (path.clone(), true, None)
            }
            Some(budget) => (
                path.clone(),
                false,
                Some(format!(
                    "over RAM budget ({size} bytes would exceed {budget})"
                )),
            ),
        })
        .collect()
}

// ===================================================== tests (device-free)

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(
        name: &str,
        presets: Vec<(i32, i32, &str)>,
    ) -> (String, String, Vec<(i32, i32, String)>) {
        (
            name.into(),
            format!("/fonts/{name}.sf2"),
            presets
                .into_iter()
                .map(|(b, p, n)| (b, p, n.into()))
                .collect(),
        )
    }

    fn cat() -> Catalog {
        Catalog::build(vec![
            raw(
                "FluidR3",
                vec![
                    (0, 0, "Grand Piano"),
                    (0, 42, "Synth strings"),
                    (128, 0, "GM Kit Standard"),
                ],
            ),
            raw(
                "StudioDrums",
                vec![(128, 0, "Tight Kit"), (8, 33, "Bass Finger")],
            ),
        ])
    }

    #[test]
    fn build_counts_flags_and_drops_out_of_range() {
        let c = Catalog::build(vec![raw(
            "X",
            vec![
                (0, 1, "ok"),
                (128, 0, "kit"),
                (129, 0, "exotic bank — dropped"),
                (0, 128, "program past 127 — dropped"),
                (-1, 0, "negative — dropped"),
            ],
        )]);
        assert_eq!(c.fonts[0].presets, 2);
        assert!(c.fonts[0].has_percussion);
        assert_eq!(c.patches.len(), 2);
        assert!(c.patches[1].has_percussion);
    }

    #[test]
    fn build_drops_duplicate_rows_keeping_first() {
        let c = Catalog::build(vec![raw(
            "X",
            vec![(0, 5, "first"), (0, 5, "second — dropped")],
        )]);
        assert_eq!(c.patches.len(), 1);
        assert_eq!(c.patches[0].name, "first");
    }

    #[test]
    fn resolve_exact_hit_needs_no_fallback() {
        let c = cat();
        let r = c.resolve(
            PatchRequest {
                sfont: "StudioDrums".into(),
                bank: 8,
                program: 33,
            },
            "FluidR3",
        );
        assert!(matches!(r, Resolution::Resolved(p) if p.name == "Bass Finger"));
    }

    #[test]
    fn resolve_falls_back_to_default_font_then_any_font() {
        let c = cat();
        // Missing font → default font rung.
        let r = c.resolve(
            PatchRequest {
                sfont: "Nope".into(),
                bank: 0,
                program: 42,
            },
            "FluidR3",
        );
        match r {
            Resolution::FellBack { used, note, .. } => {
                assert_eq!(used.sfont, "FluidR3");
                assert!(note.contains("default font"));
            }
            other => panic!("expected default-font fallback, got {other:?}"),
        }
        // bank 8:33 exists only in StudioDrums → any-font rung.
        let r = c.resolve(
            PatchRequest {
                sfont: "FluidR3".into(),
                bank: 8,
                program: 33,
            },
            "FluidR3",
        );
        match r {
            Resolution::FellBack { used, note, .. } => {
                assert_eq!(used.sfont, "StudioDrums");
                assert!(note.contains("only source"));
            }
            other => panic!("expected any-font fallback, got {other:?}"),
        }
    }

    #[test]
    fn resolve_unresolved_when_no_font_has_it() {
        let c = cat();
        let r = c.resolve(
            PatchRequest {
                sfont: "X".into(),
                bank: 7,
                program: 77,
            },
            "FluidR3",
        );
        assert!(matches!(r, Resolution::Unresolved(_)));
    }

    #[test]
    fn kit_resolution_walks_its_own_chain() {
        let c = cat();
        // Requested font has a kit → exact.
        assert!(matches!(
            c.resolve_kit(Some("StudioDrums"), "FluidR3"),
            Resolution::Resolved(p) if p.sfont == "StudioDrums"
        ));
        // Requested font loaded but has NO percussion → default-font kit.
        let no_kit = Catalog::build(vec![
            raw("FluidR3", vec![(128, 0, "GM Kit")]),
            raw("Melodic", vec![(0, 1, "Piano")]),
        ]);
        match no_kit.resolve_kit(Some("Melodic"), "FluidR3") {
            Resolution::FellBack { used, note, .. } => {
                assert_eq!(used.sfont, "FluidR3");
                assert!(note.contains("no percussion"));
            }
            other => panic!("expected default-kit fallback, got {other:?}"),
        }
        // Default has no kit either → any-font kit.
        let odd = Catalog::build(vec![
            raw("FluidR3", vec![(0, 1, "Piano")]),
            raw("StudioDrums", vec![(128, 0, "Tight Kit")]),
        ]);
        match odd.resolve_kit(Some("Melodic"), "FluidR3") {
            Resolution::FellBack { used, .. } => assert_eq!(used.sfont, "StudioDrums"),
            other => panic!("expected any-kit fallback, got {other:?}"),
        }
        // No kits at all → unresolved (caller silences + logs loudly).
        let bare = Catalog::build(vec![raw("FluidR3", vec![(0, 1, "Piano")])]);
        assert!(matches!(
            bare.resolve_kit(None, "FluidR3"),
            Resolution::Unresolved(_)
        ));
    }

    #[test]
    fn budget_plan_keeps_order_and_skips_over_budget() {
        let mb = 1_048_576u64;
        let sized = vec![
            ("a".to_string(), 40 * mb),
            ("b".to_string(), 40 * mb),
            ("c".to_string(), 40 * mb),
        ];
        // Unlimited: everything loads.
        assert!(plan_font_loads(&sized, None).iter().all(|(_, ok, _)| *ok));
        // 100 MB budget: a+b fit, c skipped WITH a reason (never silent).
        let plan = plan_font_loads(&sized, Some(100 * mb));
        assert_eq!(
            plan.iter().map(|(_, ok, _)| *ok).collect::<Vec<_>>(),
            vec![true, true, false]
        );
        assert!(plan[2].2.as_deref().unwrap().contains("budget"));
    }

    #[test]
    fn skipped_fonts_are_visible_entries() {
        let mut c = Catalog::default();
        c.push_skipped("broken".into(), "/f/broken.sf2".into(), "corrupt".into());
        assert_eq!(c.fonts.len(), 1);
        assert_eq!(c.fonts[0].skipped.as_deref(), Some("corrupt"));
        // And they resolve to nothing.
        assert!(c.find("broken", 0, 0).is_none());
    }
}
