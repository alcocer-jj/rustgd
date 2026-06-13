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
// canonical -I and -D flags for the active R installation.

use std::process::Command;

fn main() {
    let output = Command::new("R")
        .args(["CMD", "config", "--cppflags"])
        .output()
        .expect("failed to invoke `R CMD config --cppflags`; is R on PATH?");

    if !output.status.success() {
        panic!(
            "`R CMD config --cppflags` exited non-zero:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let cppflags = String::from_utf8(output.stdout)
        .expect("`R CMD config --cppflags` returned non-UTF-8 output");

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
}
