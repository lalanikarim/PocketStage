//! `pocketstage-oxygen` — M-Audio Oxygen 49 MKV control surface integration.
//!
//! Part 1 (this): a pure, mode-aware input mapper turning decoded
//! [`pocketstage_midi::Event`]s into semantic [`Action`]s using the measured
//! factory map (`specs/device-maps/oxygen-49-mkv.md`), JSON-reassignable.
//!
//! Part 2 (pending T2 SysEx protocol): LED feedback writes on the Editor port.
//!
//! ```
//! use pocketstage_oxygen::{default_mapping, Mapper, Action};
//! use pocketstage_midi::Event;
//!
//! let mapper = Mapper::new(default_mapping());
//! let a = mapper.map(&Event::ControlChange { channel: 12, control: 35, value: 77 });
//! assert_eq!(a, Some(Action::Fader { index: 3, value: 77 }));
//! ```

#![forbid(unsafe_code)]

mod mapper;
mod mapping;

pub use mapper::{Action, Mapper};
pub use mapping::{Mapping, default_mapping};

#[cfg(test)]
mod tests;
