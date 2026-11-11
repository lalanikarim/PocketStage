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
        muted: false,
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
    assert_eq!(file.project.loop_bars, 4); // D16 default
    assert_eq!(t.channel, None); // D3: channel = track index
    assert_eq!(t.root_note, None); // D23: SEQ fallback pitch unset by default
    file.validate().unwrap();
}

#[test]
fn root_note_round_trips_and_validates() {
    // Additive v1-safe field (D23): optional, defaults None, range-checked.
    let mut p = Project::default_project();
    p.tracks[0].root_note = Some(48);
    p.validate().unwrap();
    let json = ProjectFile {
        version: SCHEMA_VERSION,
        project: p.clone(),
    }
    .to_json_pretty()
    .unwrap();
    let back = ProjectFile::from_json_str(&json).unwrap();
    assert_eq!(back.project.tracks[0].root_note, Some(48));

    p.tracks[0].root_note = Some(128);
    assert_eq!(p.validate(), Err(ValidationError::PitchOutOfRange(128)));
}

#[test]
fn master_gain_round_trips_and_validates() {
    // Additive v1-safe field: optional, defaults None (= boot setting), 0..=2.
    let mut p = Project::default_project();
    assert_eq!(p.master_gain, None);
    p.validate().unwrap();

    p.master_gain = Some(0.6);
    p.validate().unwrap();
    let json = ProjectFile {
        version: SCHEMA_VERSION,
        project: p.clone(),
    }
    .to_json_pretty()
    .unwrap();
    let back = ProjectFile::from_json_str(&json).unwrap();
    assert_eq!(back.project.master_gain, Some(0.6));

    p.master_gain = Some(2.5);
    assert_eq!(
        p.validate(),
        Err(ValidationError::MasterGainOutOfRange { value: 2.5 })
    );
}

// --------------------------------------------------- slice A: record prep

#[test]
fn track_channel_override_and_default() {
    let mut p = Project::default_project();
    assert_eq!(p.track_channel(2), 2); // D3 default: index
    p.tracks[1].channel = Some(9); // explicit drum routing
    assert_eq!(p.track_channel(1), 9);
    p.validate().unwrap();

    p.tracks[1].channel = Some(16);
    assert_eq!(
        p.validate().unwrap_err(),
        ValidationError::ChannelOutOfRange {
            track: "E.Piano".into(),
            channel: 16
        }
    );
}

#[test]
fn loop_bars_validation() {
    let mut p = Project::default_project();
    for good in LOOP_BARS_RANGE.clone() {
        p.loop_bars = good;
        p.validate().unwrap();
    }
    p.loop_bars = 0;
    assert!(matches!(
        p.validate(),
        Err(ValidationError::LoopBarsOutOfRange(0))
    ));
    p.loop_bars = 17;
    assert!(matches!(
        p.validate(),
        Err(ValidationError::LoopBarsOutOfRange(17))
    ));
}

#[test]
fn take_loop_ticks_floor_and_roundup() {
    let short = Take {
        name: "short".into(),
        muted: false,
        events: vec![RecordedEvent::NoteOn {
            tick: 3,
            channel: 0,
            pitch: 60,
            velocity: 90,
        }],
    };
    // Take under the floor -> project loop_bars (4 bars = 64 ticks).
    assert_eq!(short.loop_ticks(4), 64);

    let long = Take {
        name: "long".into(),
        muted: false,
        events: vec![RecordedEvent::NoteOff {
            tick: 70, // into bar 5 (ticks 64..80)
            channel: 0,
            pitch: 60,
        }],
    };
    // Longer than the floor -> rounds up to whole bars, never truncates.
    assert_eq!(long.loop_ticks(2), 80);
    assert_eq!(long.loop_ticks(8), 128); // floor still wins when bigger
}

#[test]
fn take_events_at_uses_sorted_window() {
    let t = Take {
        name: "w".into(),
        muted: false,
        events: vec![
            RecordedEvent::NoteOn {
                tick: 0,
                channel: 0,
                pitch: 60,
                velocity: 90,
            },
            RecordedEvent::NoteOff {
                tick: 4,
                channel: 0,
                pitch: 60,
            },
            RecordedEvent::NoteOn {
                tick: 4,
                channel: 1,
                pitch: 64,
                velocity: 80,
            },
        ],
    };
    assert!(t.is_sorted());
    assert_eq!(t.events_at(0).len(), 1);
    assert_eq!(t.events_at(4).len(), 2);
    assert_eq!(t.events_at(5).len(), 0);
    assert_eq!(t.events_at(3).len(), 0);
}

#[test]
fn unsorted_take_is_rejected() {
    let mut p = Project::default_project();
    p.takes.push(Take {
        name: "bad".into(),
        muted: false,
        events: vec![
            RecordedEvent::NoteOn {
                tick: 8,
                channel: 0,
                pitch: 60,
                velocity: 90,
            },
            RecordedEvent::NoteOn {
                tick: 2,
                channel: 0,
                pitch: 62,
                velocity: 90,
            },
        ],
    });
    assert_eq!(
        p.validate().unwrap_err(),
        ValidationError::UnsortedTake { take: "bad".into() }
    );
}

// ------------------------------------------------------- mixer (web slice 5)

#[test]
fn mixer_fields_default_when_absent() {
    // Pre-mixer files must parse unchanged: gain/pan None, mute false.
    let json = r#"{
        "version": 1,
        "project": {
            "name": "PreMixer",
            "tracks": [{ "name": "T1", "instrument": {}, "clips": [null] }],
            "scenes": [{ "name": "A" }]
        }
    }"#;
    let file = ProjectFile::from_json_str(json).unwrap();
    let t = &file.project.tracks[0];
    assert_eq!(t.gain, None);
    assert_eq!(t.pan, None);
    assert!(!t.mute);
    file.validate().unwrap();
}

#[test]
fn mixer_values_round_trip_and_validate() {
    let mut p = sample_project();
    p.tracks[0].gain = Some(0.5);
    p.tracks[0].pan = Some(-1.0);
    p.tracks[0].mute = true;
    p.validate().unwrap();
    let file = ProjectFile {
        version: SCHEMA_VERSION,
        project: p.clone(),
    };
    let json = file.to_json_pretty().unwrap();
    let back = ProjectFile::from_json_str(&json).unwrap();
    assert_eq!(back.project, p);
}

#[test]
fn take_muted_defaults_false_and_round_trips() {
    let json = r#"{
        "version": 1,
        "project": {
            "name": "MuteTest",
            "tracks": [{ "name": "T1", "instrument": {}, "clips": [null] }],
            "scenes": [{ "name": "A" }],
            "takes": [{ "name": "t", "events": [] }]
        }
    }"#;
    let file = ProjectFile::from_json_str(json).unwrap();
    assert!(!file.project.takes[0].muted);

    let mut p = sample_project();
    p.takes[0].muted = true;
    let file = ProjectFile {
        version: SCHEMA_VERSION,
        project: p.clone(),
    };
    let back = ProjectFile::from_json_str(&file.to_json_pretty().unwrap()).unwrap();
    assert_eq!(back.project, p);
}

#[test]
fn mixer_range_checks() {
    let mut p = sample_project();
    p.tracks[0].gain = Some(2.5);
    assert!(matches!(
        p.validate().unwrap_err(),
        ValidationError::GainOutOfRange { .. }
    ));
    p.tracks[0].gain = Some(0.0);
    p.tracks[0].pan = Some(-1.5);
    assert!(matches!(
        p.validate().unwrap_err(),
        ValidationError::PanOutOfRange { .. }
    ));
}

#[test]
fn fx_sends_default_none_and_validate_range() {
    let mut p = Project::default_project();
    assert_eq!(p.tracks[0].reverb, None);
    assert_eq!(p.tracks[0].chorus, None);
    p.validate().unwrap();

    p.tracks[0].reverb = Some(0.4);
    p.tracks[0].chorus = Some(1.0);
    p.validate().unwrap();
    let json = serde_json::to_string(&p).unwrap();
    let back: Project = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p);

    p.tracks[0].chorus = Some(1.5);
    assert!(p.validate().is_err());
}

// ------------------------------------------------------------ global FX knobs

#[test]
fn global_fx_fields_default_to_none() {
    let p = Project::default_project();
    assert_eq!(p.fx_reverb, None);
    assert_eq!(p.fx_chorus, None);
    p.validate().unwrap();
}

#[test]
fn global_fx_validates_range() {
    let mut p = Project::default_project();
    p.fx_reverb = Some(0.25);
    p.fx_chorus = Some(1.0);
    p.validate().unwrap();

    p.fx_reverb = Some(1.5);
    assert!(matches!(
        p.validate(),
        Err(ValidationError::FxOutOfRange {
            name: "fx_reverb",
            ..
        })
    ));
    p.fx_reverb = None;
    p.fx_chorus = Some(-0.1);
    assert!(matches!(
        p.validate(),
        Err(ValidationError::FxOutOfRange {
            name: "fx_chorus",
            ..
        })
    ));
}

#[test]
fn old_files_without_fx_fields_still_parse() {
    // Additive-field policy: a v1 file predating fx_reverb/fx_chorus loads
    // with None defaults, and round-trips once present.
    let mut p = Project::default_project();
    p.fx_reverb = Some(0.6);
    let json = serde_json::to_string(&ProjectFile {
        version: SCHEMA_VERSION,
        project: p.clone(),
    })
    .unwrap();
    let back = ProjectFile::from_json_str(&json).unwrap();
    assert_eq!(back.project.fx_reverb, Some(0.6));
    assert_eq!(back.project.fx_chorus, None);

    let stripped = json
        .replace("\"fx_reverb\":0.6,", "")
        .replace("\"fx_reverb\": 0.6,", "");
    let back = ProjectFile::from_json_str(&stripped).unwrap();
    assert_eq!(back.project.fx_reverb, None);
    back.project.validate().unwrap();
}

// ------------------------------------------------------------ JSON drum kits

#[test]
fn drum_kit_defaults_to_none_and_validates_gm_range() {
    let mut p = Project::default_project();
    assert_eq!(p.drum_kit, None);
    p.validate().unwrap();

    p.drum_kit = Some([36, 38, 37, 39, 42, 46, 47, 49]);
    p.validate().unwrap();

    p.drum_kit = Some([36, 34, 37, 39, 42, 46, 47, 49]); // 34 < 35
    assert!(matches!(
        p.validate(),
        Err(ValidationError::DrumNoteOutOfRange { note: 34 })
    ));
    p.drum_kit = Some([36, 38, 37, 39, 42, 46, 47, 99]); // 99 > 81
    assert!(matches!(
        p.validate(),
        Err(ValidationError::DrumNoteOutOfRange { note: 99 })
    ));
}

#[test]
fn drum_kit_json_round_trip_and_wrong_length_rejected() {
    let mut p = Project::default_project();
    p.drum_kit = Some([35, 36, 37, 38, 39, 40, 41, 42]);
    let json = serde_json::to_string(&ProjectFile {
        version: SCHEMA_VERSION,
        project: p.clone(),
    })
    .unwrap();
    let back = ProjectFile::from_json_str(&json).unwrap();
    assert_eq!(back.project.drum_kit, p.drum_kit);

    // serde fixes the array length — a 7-element kit must not parse.
    let short = json.replace("[35,36,37,38,39,40,41,42]", "[35,36,37,38,39,40,41]");
    assert!(ProjectFile::from_json_str(&short).is_err());
}
