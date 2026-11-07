//! Hand-written FFI bindings to the FluidSynth C API (stable across 2.x).
//!
//! Rationale (HANDOFF §5 / §8): crates.io bindings are stale/abandoned; we
//! declare only what the engine needs. Only a `Settings` handle and a single
//! `Synth` handle are exposed — the synth is created on the main thread, then
//! moved to the audio thread and never touched from anywhere else (single-writer).

#![allow(non_camel_case_types)]

use std::ffi::CString;
use std::os::raw::{c_char, c_double, c_int};

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
