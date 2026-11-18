//! Phase-1 tests for the DAW keymap (specs/daw-keymap.md "Test-first notes").

use crate::keymap::*;
use pocketstage_model::{ActiveVoice, MAX_TRACKS};

fn v(note: u8, program: u8) -> VoiceBinding {
    VoiceBinding {
        source: Source::note(note),
        voice: ActiveVoice::Program(program),
        label: "V".into(),
    }
}

#[test]
fn defaults_round_trip() {
    let d = DawKeymap::default();
    let json = d.to_json_string();
    let back = DawKeymap::from_json_str(&json).expect("defaults re-parse");
    assert_eq!(d, back);
    assert!(d.validate().is_ok(), "built-in defaults must validate");
}

#[test]
fn defaults_match_engine_hardcoded_table() {
    // Regression lock for engine main.rs: VOICE_BROWSER_ANCHOR=36 white keys
    // C2..G3, the curated GM list, CMD_MIX/SEQ/NEXT/FLIP = 56/58/61/63.
    let d = DawKeymap::default();
    let notes: Vec<u8> = d.voices.iter().map(|b| b.source.number()).collect();
    assert_eq!(notes, [36, 38, 40, 41, 43, 45, 47, 48, 50, 52, 53, 55]);
    let programs: Vec<ActiveVoice> = d.voices.iter().map(|b| b.voice.clone()).collect();
    assert_eq!(programs[0], ActiveVoice::Drums);
    for (i, p) in [0u8, 4, 6, 11, 19, 24, 29, 32, 38, 48, 65]
        .iter()
        .enumerate()
    {
        assert_eq!(programs[i + 1], ActiveVoice::Program(*p));
    }
    let cmds: Vec<(u8, Command)> = d
        .commands
        .iter()
        .map(|b| (b.source.number(), b.command))
        .collect();
    assert_eq!(
        cmds,
        [
            (56, Command::MixToggle),
            (58, Command::SeqToggle),
            (61, Command::SeqNextBar),
            (63, Command::SeqFlip),
        ]
    );
    assert!(d.fader_buttons.is_empty(), "no bank defaults pre-T7");
    assert!(d.knobs.is_empty(), "knobs ship empty in v0.1");
}

#[test]
fn default_note_bindings_are_music_port_only() {
    // D30: the voice browser and the four overlay commands are keybed
    // inventory, which the measured map routes exclusively to the music port
    // (port :0). Qualifying the defaults keeps the whole HUI note space free
    // for DAW-mode bindings — unqualified rows used to claim e.g. note 47 on
    // *every* port, so any HUI-qualified binding there failed validation as a
    // duplicate.
    let d = DawKeymap::default();
    for b in &d.voices {
        assert_eq!(
            b.source.port(),
            Some(Port::Music),
            "voice {} must be music-port qualified",
            b.label
        );
    }
    for b in &d.commands {
        assert_eq!(
            b.source.port(),
            Some(Port::Music),
            "command {:?} must be music-port qualified",
            b.command
        );
    }
    assert!(d.validate().is_ok());
}

#[test]
fn hui_note_defaults_coexist_with_default_voices() {
    // The regression this locks: before voices were port-qualified, binding a
    // HUI note that shares a number with a browser slot (46/47 …) was
    // impossible — the unqualified voice row claimed it on every port.
    let mut km = DawKeymap::default();
    for (note, command) in [(46u8, Command::BankPrev), (47u8, Command::BankNext)] {
        km.commands.push(CommandBinding {
            source: Source::note_on(note, Port::Hui),
            command,
        });
    }
    assert_eq!(km.duplicates(), Vec::<String>::new());
    assert!(km.validate().is_ok(), "{:?}", km.validate());
    // Both maps stay unambiguous: same number, different port.
    assert_eq!(
        km.command_for(Source::note(47), Port::Hui),
        Some(Command::BankNext)
    );
    assert_eq!(km.voice_for(47, Port::Hui).map(|b| b.label.as_str()), None);
    assert_eq!(
        km.voice_for(47, Port::Music).map(|b| b.label.as_str()),
        Some("Nylon Guitar")
    );
}

#[test]
fn empty_file_yields_defaults() {
    assert_eq!(DawKeymap::from_json_str("").unwrap(), DawKeymap::default());
    assert_eq!(
        DawKeymap::from_json_str("  \n").unwrap(),
        DawKeymap::default()
    );
    assert_eq!(
        DawKeymap::from_json_str("{}").unwrap(),
        DawKeymap::default()
    );
}

#[test]
fn partial_file_overrides_one_section() {
    let json = r#"{ "commands": [ { "source": {"note": 60}, "command": "mix_toggle" } ] }"#;
    let k = DawKeymap::from_json_str(json).unwrap();
    assert_eq!(k.commands.len(), 1);
    assert_eq!(k.commands[0].command, Command::MixToggle);
    // Other sections keep their defaults (per-section serde defaults).
    assert_eq!(k.voices, DawKeymap::default().voices);
}

#[test]
fn unknown_field_rejected() {
    let json = r#"{ "voices": [], "hacks": [] }"#;
    assert!(DawKeymap::from_json_str(json).is_err());
    let nested = r#"{ "voices": [ { "source": {"note": 36}, "voice": "drums", "label": "D", "extra": 1 } ] }"#;
    assert!(DawKeymap::from_json_str(nested).is_err());
}

#[test]
fn source_shapes() {
    // note / cc / port forms from the frozen schema.
    for (txt, expect) in [
        (r#"{"note":36}"#, Source::note(36)),
        (r#"{"cc":44}"#, Source::cc(44)),
        (
            r#"{"note":56,"port":"hui"}"#,
            Source::note_on(56, Port::Hui),
        ),
        (
            r#"{"cc":49,"port":"music"}"#,
            Source::cc_on(49, Port::Music),
        ),
    ] {
        let s: Source = serde_json::from_str(txt).unwrap();
        assert_eq!(s, expect, "parse {txt}");
        assert_eq!(serde_json::to_string(&s).unwrap(), txt);
    }
    // Invalid shapes rejected at parse time.
    assert!(serde_json::from_str::<Source>(r#"{"note":1,"cc":2}"#).is_err());
    assert!(serde_json::from_str::<Source>(r#"{}"#).is_err());
    assert!(serde_json::from_str::<Source>(r#"{"note":1,"bogus":true}"#).is_err());
    assert!(serde_json::from_str::<Source>(r#"{"note":1,"port":"usb"}"#).is_err());
}

#[test]
fn note_and_cc_spaces_are_disjoint() {
    let mut k = DawKeymap::default();
    k.fader_buttons.push(FaderButtonBinding {
        source: Source::cc(44),
        command: Command::BankPrev,
    });
    // Default voices include note 44 (slot 2) — cc 44 must NOT collide.
    assert!(k.validate().is_ok(), "{:?}", k.validate());
}

#[test]
fn duplicate_notes_rejected_across_sections() {
    let mut k = DawKeymap::default();
    k.commands.push(CommandBinding {
        source: Source::note(36), // already voice slot 0
        command: Command::MixToggle,
    });
    let err = k.validate().unwrap_err();
    assert!(
        err.contains("voices[0]") && err.contains("commands[4]"),
        "{err}"
    );
    // The duplicates() method reports the same pair for the UI.
    assert_eq!(k.duplicates().len(), 1);
}

#[test]
fn port_qualified_duplicates_are_port_aware() {
    let mut k = DawKeymap {
        voices: vec![],
        commands: vec![],
        fader_buttons: vec![],
        knobs: vec![],
    };
    // Same note on different ports: legal.
    k.voices.push(VoiceBinding {
        source: Source::note_on(50, Port::Music),
        voice: ActiveVoice::Program(1),
        label: "A".into(),
    });
    k.commands.push(CommandBinding {
        source: Source::note_on(50, Port::Hui),
        command: Command::MixToggle,
    });
    assert!(k.validate().is_ok());
    // Unqualified overlaps either: illegal.
    k.commands.push(CommandBinding {
        source: Source::note(50),
        command: Command::SeqToggle,
    });
    let err = k.validate().unwrap_err();
    assert!(err.contains("note 50"), "{err}");
}

#[test]
fn range_checks() {
    // u8 admits >127; validate must catch it in every space.
    let mut k = DawKeymap::default();
    k.voices.push(v(128, 0));
    assert!(k.validate().unwrap_err().contains("note 128"));
    k.voices.pop();
    k.fader_buttons.push(FaderButtonBinding {
        source: Source::cc(200),
        command: Command::BankNext,
    });
    assert!(k.validate().unwrap_err().contains("cc 200"));
    k.fader_buttons.clear();
    k.voices.push(v(60, 128));
    assert!(k.validate().unwrap_err().contains("program 128"));
}

#[test]
fn label_rules() {
    let mut k = DawKeymap::default();
    k.voices.push(VoiceBinding {
        source: Source::note(70),
        voice: ActiveVoice::Drums,
        label: "x".repeat(LABEL_MAX + 1),
    });
    assert!(k.validate().unwrap_err().contains("label"));
    k.voices.last_mut().unwrap().label = String::new();
    assert!(k.validate().unwrap_err().contains("empty"));
}

#[test]
fn lookups_are_port_aware() {
    let mut k = DawKeymap::default();
    k.fader_buttons.push(FaderButtonBinding {
        source: Source::cc_on(49, Port::Hui),
        command: Command::BankPrev,
    });
    // Default voice/command lookups (unqualified bindings match any port).
    let b = k.voice_for(40, Port::Music).unwrap();
    assert_eq!(b.voice, ActiveVoice::Program(4));
    assert_eq!(
        k.command_for(Source::note(56), Port::Music),
        Some(Command::MixToggle)
    );
    // HUI-only button does not match on the music port.
    assert_eq!(
        k.command_for(Source::cc(49), Port::Hui),
        Some(Command::BankPrev)
    );
    assert_eq!(k.command_for(Source::cc(49), Port::Music), None);
    // Unbound sources are inert.
    assert!(k.voice_for(70, Port::Music).is_none());
}

#[test]
fn schema_stamp_exists() {
    // Q-V3: no version field in the file; code carries the stamp.
    assert_eq!(KEYMAP_SCHEMA, 1);
}

// ---- knob targets (D36) ----------------------------------------------------

fn target(kind: KnobKind, step: f64, param: KnobParam) -> KnobTarget {
    KnobTarget { kind, step, param }
}

const PAN2: KnobParam = KnobParam::Track(TrackTarget {
    index: 2,
    field: TrackField::Pan,
});

#[test]
fn knob_relative_row_round_trip_and_step_default() {
    let json = r#"{
        "knobs": [
            { "source": {"cc": 16, "port": "hui"},
              "target": {"kind": "relative", "param": {"track": {"index": 2, "field": "pan"}}} }
        ]
    }"#;
    let k = DawKeymap::from_json_str(json).expect("knob row parses");
    assert_eq!(k.knobs.len(), 1);
    let b = &k.knobs[0];
    assert_eq!(b.source, Source::cc_on(16, Port::Hui));
    assert_eq!(b.target.kind, KnobKind::Relative);
    assert_eq!(b.target.step, default_step(), "step defaults to 0.02");
    assert_eq!(b.target.param, PAN2);
    k.validate().expect("knob row validates");
    let back = DawKeymap::from_json_str(&k.to_json_string()).expect("re-parse");
    assert_eq!(k, back);
}

#[test]
fn knob_absolute_global_row_round_trip() {
    let json = r#"{
        "knobs": [
            { "source": {"cc": 71},
              "target": {"kind": "absolute", "param": {"global": "master_gain"}} }
        ]
    }"#;
    let k = DawKeymap::from_json_str(json).expect("absolute global row parses");
    assert_eq!(
        k.knobs[0].target.param,
        KnobParam::Global(GlobalParam::MasterGain)
    );
    k.validate().expect("validates");
    assert_eq!(DawKeymap::from_json_str(&k.to_json_string()).unwrap(), k);
}

#[test]
fn knob_row_requires_target() {
    // Pre-D36 source-only rows are rejected (they were never consumed; the
    // loader's never-fail-open rule turns such a file into warn + defaults).
    let json = r#"{"knobs": [{"source": {"cc": 16}}]}"#;
    assert!(DawKeymap::from_json_str(json).is_err());
}

#[test]
fn knob_param_vocabulary_is_closed() {
    for bad in [
        r#"{"kind":"relative","param":{"track":{"index":2,"field":"filter"}}}"#,
        r#"{"kind":"relative","param":{"global":"tempo"}}"#,
        r#"{"kind":"slide","param":{"global":"master_gain"}}"#,
        r#"{"kind":"absolute","param":{"track":{"index":2}}}"#, // field required
    ] {
        let json = format!(r#"{{"knobs":[{{"source":{{"cc":16}},"target":{bad}}}]}}"#);
        assert!(
            DawKeymap::from_json_str(&json).is_err(),
            "must reject: {bad}"
        );
    }
}

#[test]
fn knob_target_validation_ranges() {
    let row = |param: KnobParam, step: f64| DawKeymap {
        knobs: vec![KnobBinding {
            source: Source::cc_on(16, Port::Hui),
            target: target(KnobKind::Relative, step, param),
        }],
        ..Default::default()
    };
    assert!(row(PAN2, 0.02).validate().is_ok());
    let too_low = KnobParam::Track(TrackTarget {
        index: 0,
        field: TrackField::Gain,
    });
    assert!(
        row(too_low, 0.02)
            .validate()
            .unwrap_err()
            .contains("index 0")
    );
    let too_high = KnobParam::Track(TrackTarget {
        index: MAX_TRACKS as u8 + 1,
        field: TrackField::Gain,
    });
    assert!(
        row(too_high, 0.02)
            .validate()
            .unwrap_err()
            .contains("index")
    );
    assert!(row(PAN2, 0.0).validate().unwrap_err_text().contains("step"));
    assert!(row(PAN2, 1.5).validate().unwrap_err_text().contains("step"));
}

trait ErrText {
    fn unwrap_err_text(self) -> String;
}
impl ErrText for Result<(), String> {
    fn unwrap_err_text(self) -> String {
        self.expect_err("expected a validation error")
    }
}

#[test]
fn knob_ticks_use_measured_direction_constants() {
    let rel = target(KnobKind::Relative, 0.25, PAN2);
    assert_eq!(rel.apply(RELATIVE_CW, 0.5), Some(0.75)); // CW = +step
    assert_eq!(rel.apply(RELATIVE_CCW, 0.5), Some(0.25)); // CCW = -step
    assert_eq!(rel.apply(40, 0.5), None); // unmeasured value: ignored
    let abs = target(KnobKind::Absolute, 0.02, PAN2);
    assert_eq!(abs.apply(0, 0.9), Some(0.0));
    assert_eq!(abs.apply(127, 0.9), Some(1.0));
    assert_eq!(abs.apply(64, 0.9), Some(64.0 / 127.0));
}

#[test]
fn knob_sources_join_duplicate_detection() {
    let mut k = DawKeymap::default();
    let row = |cc: u8| KnobBinding {
        source: Source::cc_on(cc, Port::Hui),
        target: target(KnobKind::Relative, 0.02, PAN2),
    };
    k.knobs.push(row(16));
    k.knobs.push(row(16));
    assert!(k.validate().unwrap_err_text().contains("duplicate"));
    // Notes and CCs stay disjoint spaces: a cc-16 knob + note-16 command coexist.
    k.knobs.pop();
    k.commands.push(CommandBinding {
        source: Source::note_on(16, Port::Hui),
        command: Command::MixToggle,
    });
    k.validate()
        .expect("cc 16 and note 16 are different spaces");
    // ...but a CC claimed by BOTH a knob and a command is a collision.
    k.commands.last_mut().unwrap().source = Source::cc_on(16, Port::Hui);
    assert!(k.validate().unwrap_err_text().contains("duplicate"));
}

#[test]
fn knob_for_lookup_is_port_aware() {
    let mut k = DawKeymap::default();
    k.knobs.push(KnobBinding {
        source: Source::cc_on(16, Port::Hui),
        target: target(KnobKind::Relative, 0.02, PAN2),
    });
    k.knobs.push(KnobBinding {
        source: Source::cc(71),
        target: target(KnobKind::Absolute, 0.02, PAN2),
    });
    assert!(
        k.knob_for(Source::cc_on(16, Port::Hui), Port::Hui)
            .is_some()
    );
    assert!(
        k.knob_for(Source::cc_on(16, Port::Music), Port::Music)
            .is_none()
    );
    // Unqualified row matches any port; notes never match a cc-space lookup.
    assert!(
        k.knob_for(Source::cc_on(71, Port::Music), Port::Music)
            .is_some()
    );
    assert!(
        k.knob_for(Source::note_on(71, Port::Music), Port::Music)
            .is_none()
    );
}
