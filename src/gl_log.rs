//! OpenGL error checking and file logging helper.
//!
//! Every rendered frame, we drain OpenGL's error queue (`glGetError`) and write
//! any error to `logs/gl_errors.log`. Checking every frame ensures we catch GPU
//! driver or state bugs immediately instead of letting them fail silently.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use glow::HasContext;

/// Writes startup info, any per-frame OpenGL errors, and a shutdown summary
/// into a plain-text log file.
///
/// Rust concept for Python developers:
/// A `struct` is like a Python class holding data fields. Methods are attached
/// separately inside an `impl GlErrorLogger { ... }` block below.
pub struct GlErrorLogger {
    path: PathBuf,
    file: File,
    total_errors: u64,
}

impl GlErrorLogger {
    /// Creates `logs/gl_errors.log` (creating the `logs/` folder if needed)
    /// and writes a header line for this run.
    ///
    /// Rust concept: `Result<Self, String>` is Rust's way of returning either
    /// a valid value (`Ok(logger)`) or an error message (`Err(reason)`), instead
    /// of raising Python exceptions.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create log folder {}: {e}", parent.display()))?;
        }

        // Open in truncate mode so each spike run starts with a clean, easy-to-read log.
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("Failed to open GL log file {}: {e}", path.display()))?;

        let ts = unix_seconds();
        writeln!(file, "=== OpenGL Error Log started (unix_ts={ts}) ===")
            .map_err(|e| format!("Failed to write log header: {e}"))?;
        file.flush()
            .map_err(|e| format!("Failed to flush log header: {e}"))?;

        Ok(Self {
            path,
            file,
            total_errors: 0,
        })
    }

    /// Writes an informational line (such as GPU vendor/renderer or shutdown summary).
    pub fn log_info(&mut self, message: &str) {
        let _ = writeln!(self.file, "[INFO] {message}");
        let _ = self.file.flush();
    }

    /// Drains all queued OpenGL errors for the current frame.
    ///
    /// Why a loop? OpenGL can record multiple error flags in a single frame,
    /// and `glGetError()` returns one error at a time until the queue is empty.
    /// Returns how many errors were found on this frame.
    pub fn check_frame(&mut self, gl: &glow::Context, frame_index: u64, context_label: &str) -> u32 {
        let mut frame_errors = 0;

        loop {
            // `gl.get_error()` is `unsafe` in `glow` because it requires a valid
            // OpenGL context to be current on this thread.
            let code = unsafe { gl.get_error() };
            if code == glow::NO_ERROR {
                break;
            }

            frame_errors += 1;
            self.total_errors += 1;
            let name = gl_error_name(code);
            let line = format!(
                "[GL_ERROR] frame={frame_index} context={context_label} code=0x{code:04X} ({name})"
            );
            eprintln!("{line}");
            let _ = writeln!(self.file, "{line}");
            let _ = self.file.flush();

            // Safety cap: if a broken driver returns an error forever, avoid an infinite loop.
            if frame_errors >= 32 {
                let _ = writeln!(
                    self.file,
                    "[GL_ERROR] frame={frame_index} stopped draining after 32 consecutive errors"
                );
                let _ = self.file.flush();
                break;
            }
        }

        frame_errors
    }

    /// Returns the total number of OpenGL errors recorded across the whole run.
    pub fn total_errors(&self) -> u64 {
        self.total_errors
    }

    /// Returns the path to the log file on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Translates common OpenGL numeric error codes into readable names.
fn gl_error_name(code: u32) -> &'static str {
    match code {
        glow::INVALID_ENUM => "GL_INVALID_ENUM",
        glow::INVALID_VALUE => "GL_INVALID_VALUE",
        glow::INVALID_OPERATION => "GL_INVALID_OPERATION",
        glow::INVALID_FRAMEBUFFER_OPERATION => "GL_INVALID_FRAMEBUFFER_OPERATION",
        glow::OUT_OF_MEMORY => "GL_OUT_OF_MEMORY",
        glow::STACK_UNDERFLOW => "GL_STACK_UNDERFLOW",
        glow::STACK_OVERFLOW => "GL_STACK_OVERFLOW",
        _ => "UNKNOWN_GL_ERROR",
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
