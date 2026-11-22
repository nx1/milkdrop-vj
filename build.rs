//! Build script for `milkdrop-vj`.
//!
//! Cargo runs this file automatically *before* compiling `src/main.rs`.
//! It does three jobs:
//! 1. Tells Rust where `vcpkg` placed `projectM-4.lib` so the linker can link it.
//! 2. Runs `bindgen` on `projectM-4/projectM.h` to generate Rust FFI bindings.
//! 3. Copies only the required runtime DLLs (`projectM-4.dll` and `glew32.dll`)
//!    next to our compiled `.exe` so Windows finds them automatically at runtime.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Runtime DLLs verified with `dumpbin /dependents` on `projectM-4.dll`.
/// We intentionally leave out `projectM-4-playlist.dll` because our program
/// does not link or use the projectM playlist library.
const REQUIRED_DLLS: &[&str] = &["projectM-4.dll", "glew32.dll"];

fn main() {
    // CARGO_MANIFEST_DIR is the folder that holds Cargo.toml (C:\dev\vj).
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by Cargo"),
    );

    // vcpkg manifest mode (`vcpkg install --triplet=x64-windows`) installs
    // headers, .lib files, and .dll files under `vcpkg_installed/x64-windows`.
    let vcpkg_prefix = manifest_dir.join("vcpkg_installed").join("x64-windows");
    let include_dir = vcpkg_prefix.join("include");
    let lib_dir = vcpkg_prefix.join("lib");
    let bin_dir = vcpkg_prefix.join("bin");

    // Fail early with a clear message if `vcpkg install` has not been run yet.
    let projectm_header = include_dir.join("projectM-4").join("projectM.h");
    if !projectm_header.exists() {
        panic!(
            "Could not find {}. Run `vcpkg install --triplet=x64-windows` in {} first.",
            projectm_header.display(),
            manifest_dir.display()
        );
    }

    // Re-run this build script if the manifest or the main header changes.
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("vcpkg.json").display()
    );
    println!("cargo:rerun-if-changed={}", projectm_header.display());

    // 1. Tell the linker where `projectM-4.lib` lives and link dynamically against it.
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=projectM-4");

    // 2. Generate Rust bindings for the libprojectM C API using bindgen + libclang.
    let bindings = bindgen::Builder::default()
        .header(projectm_header.to_string_lossy())
        .clang_arg(format!("-I{}", include_dir.display()))
        .allowlist_function("projectm_.*")
        .allowlist_type("projectm_.*")
        .allowlist_var("PROJECTM_.*")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed to generate bindings for projectM-4/projectM.h");

    // OUT_DIR is Cargo's private build-artifact folder for this build script.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is always set by Cargo"));
    let profile = env::var("PROFILE").expect("PROFILE is always set by Cargo");

    bindings
        .write_to_file(out_dir.join("projectm_bindings.rs"))
        .expect("Failed to write projectm_bindings.rs");

    // 3. Locate `target/<profile>` robustly (do NOT hardcode ancestor depth),
    //    then copy only the required runtime DLLs next to the executable.
    let profile_dir = find_profile_dir(&out_dir, &profile);
    copy_required_dlls(&bin_dir, &profile_dir);
}

/// Finds the `target/<profile>` directory (such as `target\debug` or `target\release`)
/// by walking up from `OUT_DIR` until we hit the `build` directory and taking its parent.
///
/// Panics with a clear message if the layout does not match `<target>/<profile>/build/...`.
fn find_profile_dir(out_dir: &Path, expected_profile: &str) -> PathBuf {
    let build_dir = out_dir
        .ancestors()
        .find(|dir| dir.file_name().and_then(|name| name.to_str()) == Some("build"))
        .unwrap_or_else(|| {
            panic!(
                "Could not locate a `build` ancestor inside OUT_DIR (`{}`). \
                 Expected a path matching `...\\{}\\build\\...`.",
                out_dir.display(),
                expected_profile
            )
        });

    let profile_dir = build_dir.parent().unwrap_or_else(|| {
        panic!(
            "Found `build` directory at `{}`, but it has no parent profile directory. \
             Expected `...\\{}\\build`.",
            build_dir.display(),
            expected_profile
        )
    });

    let actual_profile = profile_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<non-utf8>");

    if actual_profile != expected_profile {
        panic!(
            "Parent of `build` directory is `{}` (folder name `{}`), \
             but Cargo PROFILE is `{}`. Refusing to copy DLLs to an unexpected location.",
            profile_dir.display(),
            actual_profile,
            expected_profile
        );
    }

    profile_dir.to_path_buf()
}

/// Copies only the DLLs listed in `REQUIRED_DLLS` from `vcpkg_installed/x64-windows/bin`
/// into the compiled executable folder (`target\debug` or `target\release`).
fn copy_required_dlls(bin_dir: &Path, dest_dir: &Path) {
    for dll_name in REQUIRED_DLLS {
        let src_path = bin_dir.join(dll_name);
        if !src_path.exists() {
            panic!(
                "Required runtime DLL `{}` was not found at `{}`.",
                dll_name,
                src_path.display()
            );
        }

        let dest_path = dest_dir.join(dll_name);
        fs::copy(&src_path, &dest_path).unwrap_or_else(|err| {
            panic!(
                "Failed to copy `{}` to `{}`: {}",
                src_path.display(),
                dest_path.display(),
                err
            )
        });
    }
}
