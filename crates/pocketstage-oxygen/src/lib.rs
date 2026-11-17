//! `pocketstage-oxygen` — M-Audio Oxygen 49 MKV control surface integration.
//!
//! Part 1 (this): a pure, mode-aware input mapper turning decoded
//! [`pocketstage_midi::Event`]s into semantic [`Action`]s using the measured
//! factory map (`specs/device-maps/oxygen-49-mkv.md`), JSON-reassignable.
//!
//! Part 2 — **dropped, not pending**: T2 explored the Editor SysEx port and
//! found no host-controllable LED protocol (pad LEDs are local momentary
//! state), so there is no output half to this crate. State feedback is
//! voice-confirmed instead (D21: kick/rim/tom) — see HANDOFF "T2".
//!
//! Per-port mapping: feed port `:0` (music) events through a mapper built on
//! [`default_mapping`] and port `:1` (HUI) events through one built on
//! [`hui_mapping`] — the latter is how transport ▶/⏺/⏹/🔁 become distinguishable.
//!
//! ```
//! use pocketstage_oxygen::{default_mapping, Mapper, Action};
//! use pocketstage_midi::Event;
//!
//! let mut mapper = Mapper::new(default_mapping());
//! let mut out = Vec::new();
//! mapper.map(&Event::ControlChange { channel: 12, control: 35, value: 77 }, &mut out);
//! assert_eq!(out, [Action::Fader { index: 3, value: 77 }]);
//! ```

#![forbid(unsafe_code)]

mod keymap;
mod mapper;
mod mapping;

pub use keymap::{
    Command, DawKeymap, FaderButtonBinding, GlobalParam, KEYMAP_SCHEMA, KnobBinding, KnobKind,
    KnobParam, KnobTarget, LABEL_MAX, Port, RELATIVE_CCW, RELATIVE_CW, Source, Space, TrackField,
    TrackTarget, VoiceBinding, default_step,
};
pub use mapper::{Action, Mapper, Mode};
pub use mapping::{Mapping, TransportNotes, default_mapping, hui_mapping};

#[cfg(test)]
mod keymap_tests;
#[cfg(test)]
mod tests;
