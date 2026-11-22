//! Minimal "hello world" binary to verify that Rust compiles, links `projectM-4.dll`,
//! and calls the `libprojectM` C API at runtime.
//!
//! No OpenGL context or window is created here — that belongs to the two-instance spike.

use std::ffi::CStr;

/// Raw C bindings generated automatically by `build.rs` from `projectM-4/projectM.h`.
///
/// We silence Rust's naming warnings inside this module because C headers use
/// C naming conventions (like `projectm_handle`) instead of Rust's PascalCase.
mod ffi {
    #![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]
    include!(concat!(env!("OUT_DIR"), "/projectm_bindings.rs"));
}

/// Safe Rust helper that fetches the runtime `libprojectM` version string.
///
/// In C (`projectM-4/core.h`), `projectm_get_version_string()` allocates a string
/// inside `projectM-4.dll` and requires the caller to free it with
/// `projectm_free_string()`. This wrapper copies the text into a normal Rust
/// `String` and immediately frees the C pointer so memory can never leak.
fn get_projectm_version_string() -> String {
    // Calling across the C boundary (FFI) requires an `unsafe` block because
    // the Rust compiler cannot verify the internals of a C/C++ DLL.
    unsafe {
        let raw_ptr = ffi::projectm_get_version_string();
        assert!(
            !raw_ptr.is_null(),
            "projectm_get_version_string() returned a null pointer"
        );

        // Borrow the null-terminated C string and copy it into an owned Rust String.
        let version = CStr::from_ptr(raw_ptr).to_string_lossy().into_owned();

        // Free the C string using libprojectM's own allocator helper (not Rust's allocator).
        ffi::projectm_free_string(raw_ptr);

        version
    }
}

/// Safe Rust helper that reads the runtime major, minor, and patch version numbers
/// via `projectm_get_version_components()` from `projectM-4/core.h`.
fn get_projectm_version_components() -> (i32, i32, i32) {
    let mut major: std::os::raw::c_int = 0;
    let mut minor: std::os::raw::c_int = 0;
    let mut patch: std::os::raw::c_int = 0;

    unsafe {
        ffi::projectm_get_version_components(&mut major, &mut minor, &mut patch);
    }

    (major, minor, patch)
}

fn main() {
    let version_str = get_projectm_version_string();
    let (major, minor, patch) = get_projectm_version_components();

    println!("MilkDrop VJ toolchain check passed!");
    println!("libprojectM runtime version string: {version_str}");
    println!("libprojectM version components:     major={major}, minor={minor}, patch={patch}");
}
