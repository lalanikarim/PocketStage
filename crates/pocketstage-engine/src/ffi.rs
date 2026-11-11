//! Hand-written FFI bindings to the FluidSynth C API (stable across 2.x).
//!
//! Rationale (HANDOFF §5 / §8): crates.io bindings are stale/abandoned; we
//! declare only what the engine needs. Only a `Settings` handle and a single
//! `Synth` handle are exposed — the synth is created on the main thread, then
//! moved to the audio thread and never touched from anywhere else (single-writer).

#![allow(non_camel_case_types)]
// Lab examples pull this file in via #[path] and use only a subset of the
// declarations, so "never used" warnings there are expected noise — the
// engine binary itself uses everything it declares.
#![allow(dead_code)]

use std::ffi::CString;
use std::os::raw::{c_char, c_double, c_float, c_int};

/// Opaque FluidSynth types (uninhabited enums = zero-sized, can only be held by pointer).
pub enum fluid_settings_t {}
pub enum fluid_synth_t {}

#[link(name = "fluidsynth")]
unsafe extern "C" {
    // --- settings ---
    pub fn new_fluid_settings() -> *mut fluid_settings_t;
    pub fn delete_fluid_settings(settings: *mut fluid_settings_t);
    pub fn fluid_settings_setnum(
        settings: *mut fluid_settings_t,
        name: *const c_char,
        val: c_double,
    ) -> c_int;
    // --- synth lifecycle ---
    pub fn new_fluid_synth(settings: *mut fluid_settings_t) -> *mut fluid_synth_t;
    pub fn delete_fluid_synth(synth: *mut fluid_synth_t);

    // --- soundfont / events / rendering ---
    pub fn fluid_synth_sfload(
        synth: *mut fluid_synth_t,
        filename: *const c_char,
        reset_presets: c_int,
    ) -> c_int;
    pub fn fluid_synth_noteon(
        synth: *mut fluid_synth_t,
        chan: c_int,
        key: c_int,
        vel: c_int,
    ) -> c_int;
    pub fn fluid_synth_noteoff(synth: *mut fluid_synth_t, chan: c_int, key: c_int) -> c_int;
    pub fn fluid_synth_program_change(
        synth: *mut fluid_synth_t,
        chan: c_int,
        program: c_int,
    ) -> c_int;
    pub fn fluid_synth_cc(synth: *mut fluid_synth_t, chan: c_int, ctrl: c_int, val: c_int)
    -> c_int;
    pub fn fluid_synth_bank_select(synth: *mut fluid_synth_t, chan: c_int, bank: c_int) -> c_int;
    pub fn fluid_synth_all_sounds_off(synth: *mut fluid_synth_t, chan: c_int) -> c_int;
    pub fn fluid_synth_set_channel_type(
        synth: *mut fluid_synth_t,
        chan: c_int,
        type_: c_int,
    ) -> c_int;
    pub fn fluid_synth_all_notes_off(synth: *mut fluid_synth_t, chan: c_int) -> c_int;
    pub fn fluid_synth_reverb_on(synth: *mut fluid_synth_t, fx_group: c_int, on: c_int) -> c_int;
    pub fn fluid_synth_chorus_on(synth: *mut fluid_synth_t, fx_group: c_int, on: c_int) -> c_int;
    // Effect AMOUNT knobs ride the per-fx-group level setters (this build
    // has exactly one group, index 0). Space/rate/depth params are never
    // touched — see Synth::set_global_fx for why.
    //
    // History: session 10 declared `fluid_synth_set_chorus` with five args
    // in the wrong order (real signature takes `(synth, nr, level, speed,
    // depth_ms, type)`) AND called `fluid_synth_set_reverb(0.5, 0.4, 0.7, v)`
    // unconditionally at boot — this build's stock room-size is 0.2, so we
    // doubled the tail and held notes "stacked" (user report). Both calls
    // are gone; examples/fx_bench.rs has the measurements.
    pub fn fluid_synth_set_reverb_group_level(
        synth: *mut fluid_synth_t,
        fx_group: c_int,
        level: c_double,
    ) -> c_int;
    pub fn fluid_synth_set_chorus_group_level(
        synth: *mut fluid_synth_t,
        fx_group: c_int,
        level: c_double,
    ) -> c_int;
    // Master gain, live-adjustable (FluidSynth >= 2.0): scales the same
    // signal path as the `synth.gain` setting, so a running engine can be
    // turned up from the web UI without a reboot. Returns void.
    pub fn fluid_synth_set_gain(synth: *mut fluid_synth_t, gain: c_float);
    pub fn fluid_synth_write_float(
        synth: *mut fluid_synth_t,
        len: c_int,
        lout: *mut f32,
        loff: c_int,
        lincr: c_int,
        rout: *mut f32,
        roff: c_int,
        rincr: c_int,
    ) -> c_int;
}

/// Owning RAII wrapper around `fluid_settings_t`. Must outlive the `Synth`
/// created from it (FluidSynth keeps a reference, it does not copy).
pub struct Settings(pub(crate) *mut fluid_settings_t);

impl Settings {
    pub fn new() -> Option<Settings> {
        let ptr = unsafe { new_fluid_settings() };
        if ptr.is_null() {
            None
        } else {
            Some(Settings(ptr))
        }
    }

    pub fn set_num(&self, name: &str, val: f64) {
        let name = CString::new(name).unwrap();
        unsafe { fluid_settings_setnum(self.0, name.as_ptr(), val as c_double) };
    }
}

impl Drop for Settings {
    fn drop(&mut self) {
        unsafe { delete_fluid_settings(self.0) }
    }
}

/// Owning RAII wrapper around `fluid_synth_t`.
///
/// Safety contract: the synth is only ever touched from the thread that owns
/// this value (the audio callback thread). `unsafe impl Send` exists solely so
/// it can be *moved once* into the audio thread at startup.
pub struct Synth(pub(crate) *mut fluid_synth_t);

// The synth pointer is created on the main thread but only ever dereferenced
// from the audio thread after the move — happens-before is established by
// thread spawn. Single writer, no shared state.
unsafe impl Send for Synth {}

impl Synth {
    pub fn new(settings: &Settings) -> Option<Synth> {
        let ptr = unsafe { new_fluid_synth(settings.0) };
        if ptr.is_null() {
            None
        } else {
            Some(Synth(ptr))
        }
    }

    /// Loads a soundfont; returns the sfont id on success.
    pub fn load_soundfont(&self, path: &str) -> Result<i32, String> {
        let cpath = CString::new(path).map_err(|e| e.to_string())?;
        let id = unsafe { fluid_synth_sfload(self.0, cpath.as_ptr(), 1) };
        if id < 0 {
            Err(format!("fluid_synth_sfload failed for {path}"))
        } else {
            Ok(id)
        }
    }

    #[inline]
    pub fn note_on(&self, chan: i32, key: i32, vel: i32) {
        unsafe { fluid_synth_noteon(self.0, chan, key, vel) };
    }

    #[inline]
    pub fn note_off(&self, chan: i32, key: i32) {
        unsafe { fluid_synth_noteoff(self.0, chan, key) };
    }

    #[inline]
    pub fn program_change(&self, chan: i32, program: i32) {
        unsafe { fluid_synth_program_change(self.0, chan, program) };
    }

    /// Silences every voice on one channel (used when a scene switch stops a
    /// track's pattern). Note: keys share their track's channel in v0.1, so
    /// this also cuts held key notes on that channel — accepted tradeoff.
    #[inline]
    pub fn all_notes_off(&self, chan: i32) {
        unsafe { fluid_synth_all_notes_off(self.0, chan) };
    }

    /// Send a control change (used for mixer CC7 volume / CC10 pan).
    #[inline]
    pub fn cc(&self, chan: i32, ctrl: i32, val: i32) {
        unsafe { fluid_synth_cc(self.0, chan, ctrl, val) };
    }

    /// Select the bank subsequent program changes on `chan` resolve against
    /// (GM: 128 = percussion, 0..127 = melodic).
    #[inline]
    pub fn bank_select(&self, chan: i32, bank: i32) {
        unsafe { fluid_synth_bank_select(self.0, chan, bank) };
    }

    /// Immediately kill every voice on a channel (CC120 semantics). Unlike
    /// note-off this also stops one-shot samples that have no release stage —
    /// lab-proven instant. Used for same-pad retriggers; each pad owns its
    /// own channel, so collateral cuts are impossible.
    #[inline]
    pub fn all_sounds_off(&self, chan: i32) {
        unsafe { fluid_synth_all_sounds_off(self.0, chan) };
    }

    /// Mark a channel percussive (resolves to bank 128 presets regardless of
    /// GM mode) — required for drum notes on channels other than 9.
    #[inline]
    pub fn set_channel_type_drum(&self, chan: i32) {
        unsafe { fluid_synth_set_channel_type(self.0, chan, 1) }; // CHANNEL_TYPE_DRUM
    }

    /// Master gain (project `master_gain`): live equivalent of the
    /// `synth.gain` setting. Pure output level — no FX space params
    /// involved. Clamped to 0..=4 defensively (the model validates
    /// 0..=2; FluidSynth itself accepts anything finite).
    #[inline]
    pub fn set_gain(&self, gain: f64) {
        unsafe { fluid_synth_set_gain(self.0, gain.clamp(0.0, 4.0) as c_float) }
    }

    /// Global FX amount knobs — LEVELS ONLY, and only where the project
    /// speaks (`None` = touch nothing). Room size/damping/width and chorus
    /// rate/depth are never written: FluidSynth 2.3.4's built-in values are
    /// a coherent whole (e.g. room-size default 0.2), and session 10's
    /// unconditional overwrite of them is what made held keys "stack".
    ///
    /// - `reverb Some(v)` → effect on, group level = v (its native 0..=1).
    ///   `Some(0.0)` keeps the unit powered but silent — same as muting the
    ///   send; fine.
    /// - `chorus Some(v)` → on/off via `fluid_synth_chorus_on`; when > 0,
    ///   group level = v × 4 (stock default 2 sits at v = 0.5).
    #[inline]
    pub fn set_global_fx(&self, reverb: Option<f64>, chorus: Option<f64>) {
        unsafe {
            if let Some(v) = reverb {
                fluid_synth_reverb_on(self.0, -1, 1);
                fluid_synth_set_reverb_group_level(self.0, 0, v.clamp(0.0, 1.0));
            }
            if let Some(v) = chorus {
                let on = v > 0.0;
                fluid_synth_chorus_on(self.0, -1, on as c_int);
                if on {
                    fluid_synth_set_chorus_group_level(self.0, 0, (v * 4.0).clamp(0.0, 4.0));
                }
            }
        }
    }

    /// Renders `frames` audio frames into separate L / R float buffers (0..=1 stride).
    #[inline]
    pub fn render(&self, frames: usize, left: &mut [f32], right: &mut [f32]) {
        debug_assert!(left.len() >= frames && right.len() >= frames);
        unsafe {
            fluid_synth_write_float(
                self.0,
                frames as c_int,
                left.as_mut_ptr(),
                0,
                1,
                right.as_mut_ptr(),
                0,
                1,
            )
        };
    }
}

impl Drop for Synth {
    fn drop(&mut self) {
        unsafe { delete_fluid_synth(self.0) }
    }
}
