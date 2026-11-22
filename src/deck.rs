//! Safe Rust wrapper around the C `libprojectM` 4.x library.
//!
//! All `unsafe` `libprojectM` FFI calls are kept inside this single module.
//! The rest of the program interacts only with safe Rust types (`Deck`, `Result`,
//! `&Path`, `&[f32]`) and never touches raw C pointers directly.
//!
//! Rust concepts explained for Python developers:
//! - **Ownership of a C handle:** In C, `projectm_create()` hands back a raw pointer
//!   (`projectm_handle`). If you forget to call `projectm_destroy()`, GPU memory leaks;
//!   if you call it twice, the program crashes. By storing that pointer privately inside
//!   `struct Deck`, Rust guarantees exactly ONE owner exists at any time.
//! - **`Drop` trait:** Like Python's `__del__`, except deterministic: Rust automatically
//!   runs `Drop::drop(&mut self)` at the exact line where a `Deck` goes out of scope.
//!   Our `Drop` implementation calls `projectm_destroy()`, so cleanup is automatic.

use std::cell::RefCell;
use std::ffi::{c_void, CStr, CString};
use std::path::Path;
use std::ptr;

/// Raw C bindings generated automatically by `build.rs` from `projectM-4/projectM.h`.
mod ffi {
    #![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]
    include!(concat!(env!("OUT_DIR"), "/projectm_bindings.rs"));
}

/// Minimal Windows GLEW C bindings from `glew32.dll` (verified against `GL/glew.h`).
///
/// Why this is required on Windows:
/// `libprojectM 4.1.8` compiles its OpenGL calls against `glew32.dll` (`projectM-opengl.h`),
/// and expects the host app to call `glewInit()` once on an active OpenGL context before
/// calling `projectm_create()` (confirmed in `projectM`'s own `src/sdl-test-ui/setup.cpp`).
/// Without `glewInit()`, every GLEW function pointer stays null (`0x0`), causing an
/// immediate `0xC0000005` access violation inside `projectm_create()`.
#[cfg(target_os = "windows")]
mod glew_ffi {
    pub const GLEW_OK: u32 = 0;

    extern "C" {
        pub fn glewInit() -> u32;
        pub fn glewGetErrorString(error: u32) -> *const std::os::raw::c_char;
    }
}

/// Initializes GLEW's OpenGL function table on the currently active OpenGL context.
/// Must be called once on Windows before the first `Deck::new()`.
pub fn init_gl_loader_for_projectm() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    unsafe {
        let err = glew_ffi::glewInit();
        if err != glew_ffi::GLEW_OK {
            let msg_ptr = glew_ffi::glewGetErrorString(err);
            let msg = if msg_ptr.is_null() {
                format!("GLEW error code {err}")
            } else {
                CStr::from_ptr(msg_ptr).to_string_lossy().into_owned()
            };
            return Err(format!("glewInit() failed (code {err}): {msg}"));
        }
    }
    Ok(())
}

/// Returns the runtime `libprojectM` version string (for example `"4.1.8"`).
/// Safely frees the C-allocated string via `projectm_free_string()`.
pub fn projectm_version_string() -> String {
    unsafe {
        let raw_ptr = ffi::projectm_get_version_string();
        assert!(
            !raw_ptr.is_null(),
            "projectm_get_version_string() returned null"
        );
        let version = CStr::from_ptr(raw_ptr).to_string_lossy().into_owned();
        ffi::projectm_free_string(raw_ptr);
        version
    }
}

/// Returns the maximum number of PCM audio frames per channel that `libprojectM`
/// stores in one call (typically 512).
pub fn max_pcm_samples_per_channel() -> usize {
    unsafe { ffi::projectm_pcm_get_max_samples() as usize }
}

/// Callback invoked by `libprojectM` inside `projectm_load_preset_file()` if a preset
/// fails to parse or compile.
///
/// `extern "C"` tells Rust to use the standard C calling convention so `projectM-4.dll`
/// can call this function directly.
unsafe extern "C" fn on_preset_switch_failed(
    preset_filename: *const std::os::raw::c_char,
    message: *const std::os::raw::c_char,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }

    let filename = if preset_filename.is_null() {
        "<unknown>".to_string()
    } else {
        CStr::from_ptr(preset_filename)
            .to_string_lossy()
            .into_owned()
    };

    let reason = if message.is_null() {
        "<no error message>".to_string()
    } else {
        CStr::from_ptr(message).to_string_lossy().into_owned()
    };

    // Borrow our heap-allocated slot owned by `Deck` and store the error text.
    let error_slot = &*(user_data as *const RefCell<Option<String>>);
    *error_slot.borrow_mut() = Some(format!("Failed to load preset `{filename}`: {reason}"));
}

/// Safe owner of one `libprojectM` visualizer instance.
///
/// Important OpenGL rule: The caller must ensure the OpenGL context that created
/// this `Deck` is current on the calling thread whenever calling `new`, `resize`,
/// `load_preset`, `render_frame`, or dropping the `Deck`.
pub struct Deck {
    handle: ffi::projectm_handle,
    /// Boxed on the heap (`Box<...>`) so its memory address stays fixed even if
    /// the `Deck` struct moves in memory; `libprojectM` stores a raw pointer to it
    /// as callback `user_data`.
    last_load_error: Box<RefCell<Option<String>>>,
}

impl Deck {
    /// Creates a new `libprojectM` instance on the currently active OpenGL context.
    pub fn new(width: usize, height: usize) -> Result<Self, String> {
        let handle = unsafe { ffi::projectm_create() };
        if handle.is_null() {
            return Err(
                "projectm_create() returned null (check that an OpenGL 3.3+ context is current)"
                    .to_string(),
            );
        }

        // Allocate the callback error slot on the heap before registering its address.
        let last_load_error = Box::new(RefCell::new(None));
        let user_data_ptr: *mut c_void = (&*last_load_error as *const RefCell<Option<String>>)
            as *mut RefCell<Option<String>> as *mut c_void;

        unsafe {
            ffi::projectm_set_window_size(handle, width, height);
            ffi::projectm_set_fps(handle, 60);
            ffi::projectm_set_aspect_correction(handle, true);
            // Human-in-the-loop: lock preset so libprojectM never switches behind our back.
            ffi::projectm_set_preset_locked(handle, true);
            ffi::projectm_set_soft_cut_duration(handle, 1.5);
            ffi::projectm_set_beat_sensitivity(handle, 1.0);
            ffi::projectm_set_preset_switch_failed_event_callback(
                handle,
                Some(on_preset_switch_failed),
                user_data_ptr,
            );
        }

        Ok(Self {
            handle,
            last_load_error,
        })
    }

    /// Updates the rendering size (in pixels) for this deck.
    #[allow(dead_code)]
    pub fn resize(&self, width: usize, height: usize) {
        unsafe {
            ffi::projectm_set_window_size(self.handle, width, height);
        }
    }

    /// Informs `libprojectM` of the current frames-per-second rate (used by preset math).
    pub fn set_fps(&self, fps: i32) {
        unsafe {
            ffi::projectm_set_fps(self.handle, fps.max(1));
        }
    }

    /// Loads a `.milk` preset file onto this deck.
    ///
    /// Returns `Ok(())` on success, or `Err(message)` if `libprojectM` rejected the preset.
    /// If loading fails, `libprojectM` keeps rendering the previous working preset.
    pub fn load_preset(&self, path: &Path, smooth_transition: bool) -> Result<(), String> {
        let path_str = path.to_string_lossy();
        let c_path = CString::new(path_str.as_bytes())
            .map_err(|e| format!("Preset path `{path_str}` contains an interior null byte: {e}"))?;

        // Clear any leftover error before triggering the load.
        self.last_load_error.borrow_mut().take();

        unsafe {
            ffi::projectm_load_preset_file(self.handle, c_path.as_ptr(), smooth_transition);
        }

        if let Some(err_msg) = self.last_load_error.borrow_mut().take() {
            Err(err_msg)
        } else {
            Ok(())
        }
    }

    /// Feeds interleaved stereo floating-point PCM audio (`[L0, R0, L1, R1, ...]`,
    /// values in `-1.0..=1.0`) into this deck's audio analyzer.
    pub fn add_pcm_stereo(&self, interleaved_lr: &[f32]) {
        let frames = interleaved_lr.len() / 2;
        if frames == 0 {
            return;
        }
        let max_frames = max_pcm_samples_per_channel();
        let clamped_frames = frames.min(max_frames);

        unsafe {
            ffi::projectm_pcm_add_float(
                self.handle,
                interleaved_lr.as_ptr(),
                clamped_frames as u32,
                ffi::projectm_channels_PROJECTM_STEREO,
            );
        }
    }

    /// Renders a single frame to the active OpenGL context's default framebuffer.
    pub fn render_frame(&self) {
        unsafe {
            ffi::projectm_opengl_render_frame(self.handle);
        }
    }
}

/// Deterministic cleanup: runs automatically when `Deck` goes out of scope.
impl Drop for Deck {
    fn drop(&mut self) {
        unsafe {
            // Unregister our callback first so libprojectM cannot touch `last_load_error`
            // during teardown, then destroy the C instance.
            ffi::projectm_set_preset_switch_failed_event_callback(
                self.handle,
                None,
                ptr::null_mut(),
            );
            ffi::projectm_destroy(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that `cargo test` (running from `target/<profile>/deps/`) can load
    /// `projectM-4.dll` via `cargo:rustc-link-search=native=<vcpkg_bin>` and read
    /// the runtime version string.
    #[test]
    fn runtime_version_is_4_1_8() {
        assert_eq!(projectm_version_string(), "4.1.8");
    }
}
