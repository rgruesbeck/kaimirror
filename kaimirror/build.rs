//! Locate the cross-compiled device pump and hand its path to the compiler,
//! so `include_bytes!` can fold it into the host binary.  A released
//! kaimirror is then one self-contained file: it carries the pump it installs
//! rather than reaching back into the build tree for it.
//!
//! This only *reads* an artifact -- build.sh builds the pump first.  Invoking
//! cargo from here would mean cargo recursing into itself for a second
//! target, which is a lock-contention hazard for no gain.

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-env-changed=KAIPUMP_BIN");

    let default = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target/armv7-linux-androideabi/release/kaipump");
    let pump = env::var("KAIPUMP_BIN").map(PathBuf::from).unwrap_or(default);
    println!("cargo:rerun-if-changed={}", pump.display());

    // Without the pump, embed nothing rather than failing: `cargo check` and
    // rust-analyzer have no reason to need an NDK.  The binary that results
    // says so at runtime (see adb::push_pump) instead of silently shipping a
    // host half that cannot install its other half.
    let path = if pump.is_file() {
        pump
    } else {
        println!("cargo:warning=no device pump at {} -- this build cannot install one (run ./build.sh)", pump.display());
        let stub = PathBuf::from(env::var("OUT_DIR").unwrap()).join("kaipump.missing");
        fs::write(&stub, []).unwrap();
        stub
    };
    println!("cargo:rustc-env=KAIPUMP_BIN={}", path.display());
}
