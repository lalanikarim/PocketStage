//! `pocketstage-oxygen` — M-Audio Oxygen 49 MKV control surface integration.
//!
//! Part 1 (this): a pure, mode-aware input mapper turning decoded
//! [`pocketstage_midi::Event`]s into semantic [`Action`]s using the measured
//! factory map (`specs/device-maps/oxygen-49-mkv.md`), JSON-reassignable.
//!
//! Part 2 (pending T2 SysEx protocol): LED feedback writes on the Editor port.
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

mod mapper;
mod mapping;

pub use mapper::{Action, Mapper};
pub use mapping::{Mapping, TransportNotes, default_mapping, hui_mapping};

#[cfg(test)]
mod tests;
