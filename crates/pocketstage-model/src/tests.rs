//! Tests for the project model + file format.

use super::*;

fn sample_project() -> Project {
    let mut p = Project::default_project();
    // Put a 1-bar drum-ish pattern in track 0, scene 0.
    let mut steps = vec![None; STEPS_PER_BAR];
    steps[0] = Some(StepNote {
        pitch: 36,
        velocity: 110,
    });
    steps[4] = Some(StepNote {
        pitch: 42,
        velocity: 90,
    });
    p.tracks[0].clips[0] = Some(Clip {
        name: "groove".into(),
        pattern: Pattern { bars: 1, steps },
    });
    p.takes.push(Take {
        name: "take 1".into(),
        events: vec![
            RecordedEvent::NoteOn {
                tick: 0,
                channel: 0,
                pitch: 60,
                velocity: 100,
            },
            RecordedEvent::NoteOff {
                tick: 4,
                channel: 0,
                pitch: 60,
            },
            RecordedEvent::ProgramChange {
                tick: 8,
                channel: 1,
                program: 4,
            },
        ],
    });
    p
}

#[test]
fn default_project_is_valid() {
    Project::default_project().validate().unwrap();
    sample_project().validate().unwrap();
}

#[test]
fn json_round_trip_is_lossless() {
    let file = ProjectFile {
        version: SCHEMA_VERSION,
        project: sample_project(),
    };
    let json = file.to_json_pretty().unwrap();
    let back = ProjectFile::from_json_str(&json).unwrap();
    assert_eq!(file, back);
}

#[test]
fn future_schema_version_is_rejected_not_guessed() {
    let mut file = ProjectFile {
        version: SCHEMA_VERSION,
        project: sample_project(),
    };
    file.version = SCHEMA_VERSION + 1;
    let json = serde_json::to_string(&file).unwrap();
    // Version gate fires before the body is even attempted.
    assert!(matches!(
        ProjectFile::from_json_str(&json),
        Err(ModelError::UnsupportedVersion(v)) if v == SCHEMA_VERSION + 1
    ));
}

#[test]
fn missing_or_bad_version_is_rejected() {
    assert!(matches!(
        ProjectFile::from_json_str("{}"),
        Err(ModelError::Invalid(_))
    ));
    assert!(matches!(
        ProjectFile::from_json_str("[1,2,3]"),
        Err(ModelError::Invalid(_))
    ));
    assert!(ProjectFile::from_json_str("not json at all").is_err());
}

#[test]
fn clip_slot_mismatch_between_track_and_scene_count() {
    let mut p = Project::default_project();
    p.tracks[0].clips.pop().unwrap(); // 7 slots vs 8 scenes
    assert_eq!(
        p.validate(),
        Err(ValidationError::ClipSlotMismatch {
            track: "Piano".into(),
            slots: 7,
            scenes: 8
        })
    );
}

#[test]
fn range_checks() {
    let mut p = Project::default_project();
    p.bpm = 301.0;
    assert_eq!(p.validate(), Err(ValidationError::BpmOutOfRange(301.0)));
    p.bpm = 120.0;

    p.tracks.push(p.tracks[0].clone()); // 5th track
    assert_eq!(p.validate(), Err(ValidationError::TooManyTracks(5)));
}

#[test]
fn pattern_shape_is_enforced() {
    let clip_ok = |bars: u8, n_steps: usize| Clip {
        name: "x".into(),
        pattern: Pattern {
            bars,
            steps: vec![None; n_steps],
        },
    };
    let mut p = Project::default_project();

    p.tracks[0].clips[0] = Some(clip_ok(5, 5 * STEPS_PER_BAR)); // > MAX_PATTERN_BARS
    assert!(matches!(
        p.validate(),
        Err(ValidationError::PatternBarsOutOfRange { bars: 5 })
    ));

    p.tracks[0].clips[0] = Some(clip_ok(2, 16)); // wrong step count for 2 bars
    assert!(matches!(
        p.validate(),
        Err(ValidationError::PatternStepCount(..))
    ));

    p.tracks[0].clips[0] = Some(Clip {
        name: "x".into(),
        pattern: Pattern {
            bars: 1,
            steps: {
                let mut s = vec![None; STEPS_PER_BAR];
                s[0] = Some(StepNote {
                    pitch: 128,
                    velocity: 100,
                });
                s
            },
        },
    });
    assert_eq!(p.validate(), Err(ValidationError::PitchOutOfRange(128)));
}

#[test]
fn recorded_event_json_shape_is_tagged_and_stable() {
    let ev = RecordedEvent::NoteOn {
        tick: 3,
        channel: 1,
        pitch: 60,
        velocity: 99,
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"note_on","tick":3,"channel":1,"pitch":60,"velocity":99}"#
    );
    let back: RecordedEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ev);
}

#[test]
fn optional_fields_have_sane_defaults() {
    // Hand-written minimal JSON exercising serde defaults (bpm, color,
    // soundfont, gm_program, quantize, takes).
    let json = r#"{
        "version": 1,
        "project": {
            "name": "Minimal",
            "tracks": [{ "name": "T1", "instrument": {}, "clips": [null] }],
            "scenes": [{ "name": "A" }]
        }
    }"#;
    let file = ProjectFile::from_json_str(json).unwrap();
    let t = &file.project.tracks[0];
    assert_eq!(file.project.bpm, 120.0);
    assert_eq!(t.instrument.gm_program, 0);
    assert_eq!(t.instrument.soundfont, None);
    assert_eq!(t.color, [200, 200, 200]);
    file.validate().unwrap();
}
