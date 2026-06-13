// build.rs
//
// Compiles src/rustgd_device.c into the rustgd staticlib so that R's
// graphics device callbacks live alongside the Rust code instead of
// being a separate translation unit added by R's package build.
//
// This is needed because cargo also builds the `document` binary
// (extendr's R-wrapper generator), and that binary links the staticlib
// in cargo's environment, where R's package build hasn't yet compiled
// anything in src/. Putting the C file under cargo's control ensures
// the rustgd_register_device / rustgd_set_device_size symbols are
// always present in the staticlib, satisfying both the document
// binary's link step and R's final package .so link step.
//
// R headers come from `R CMD config --cppflags`, which emits the
// canonical -I and -D flags for the active R installation. See
// `r_cppflags` for how R is located and invoked robustly, including
// under `R CMD check --as-cran`.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let cppflags = r_cppflags();

    let mut builder = cc::Build::new();
    builder.file("src/rustgd_device.c");

    // Parse out -I and -D tokens from R's cppflags and forward them
    // to the cc invocation. R's cppflags typically looks like:
    //   -I/Library/Frameworks/R.framework/Resources/include
    // possibly with additional -D defines on some platforms.
    for token in cppflags.split_whitespace() {
        if let Some(path) = token.strip_prefix("-I") {
            builder.include(path);
        } else if let Some(define) = token.strip_prefix("-D") {
            if let Some((name, value)) = define.split_once('=') {
                builder.define(name, Some(value));
            } else {
                builder.define(define, None);
            }
        }
    }

    builder.compile("rustgd_device");

    println!("cargo:rerun-if-changed=src/rustgd_device.c");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=R_HOME");
}

// Return R's C preprocessor flags via `R CMD config --cppflags`.
//
// Locating R: prefer the `R_HOME` environment variable, which R sets for
// every package build (R CMD INSTALL, and the build nested inside R CMD
// check), and fall back to `R` on PATH for a plain `cargo build` run outside
// an R build. This avoids depending on PATH, which is not guaranteed inside
// `R CMD check`.
//
// Environment: `R CMD check` exports `R_TESTS` pointing at a relative startup
// file. The nested R that `R CMD config` starts then tries to load that file
// from the wrong working directory and aborts, which is why the bare call
// failed only under `R CMD check --as-cran`. Removing `R_TESTS` for this one
// command sidesteps it without affecting the rest of the build.
fn r_cppflags() -> String {
    let r_bin = locate_r();

    let output = Command::new(&r_bin)
        .args(["CMD", "config", "--cppflags"])
        .env_remove("R_TESTS")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to invoke `{} CMD config --cppflags`: {e}; is R installed?",
                r_bin.to_string_lossy()
            )
        });

    if !output.status.success() {
        panic!(
            "`{} CMD config --cppflags` exited non-zero:\n{}",
            r_bin.to_string_lossy(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    String::from_utf8(output.stdout)
        .expect("`R CMD config --cppflags` returned non-UTF-8 output")
}

// Resolve the R executable: `$R_HOME/bin/R` (or R.exe on Windows) when R_HOME
// is set and that file exists, otherwise the bare `R`/`R.exe` looked up on PATH.
fn locate_r() -> OsString {
    let exe = if cfg!(windows) { "R.exe" } else { "R" };
    if let Some(home) = env::var_os("R_HOME") {
        let mut p = PathBuf::from(home);
        p.push("bin");
        p.push(exe);
        if p.is_file() {
            return p.into_os_string();
        }
    }
    OsString::from(exe)
}
