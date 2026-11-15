//! Phase-1 tests for the DAW keymap (specs/daw-keymap.md "Test-first notes").

use crate::keymap::*;
use pocketstage_model::ActiveVoice;

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
    let programs: Vec<ActiveVoice> = d.voices.iter().map(|b| b.voice).collect();
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
