//! Decompile files through the LIBRARY entry point (`luau_core::decompile`),
//! the exact call `luau-wasm`, `luau-server` and the `luau-ffi` C ABI make.
//!
//! Exists to measure library-vs-CLI parity: run it and `luau-decompiler
//! decompile` over the same inputs and diff the two output folders.
//!
//! ```text
//! cargo run --release -p luau-core --example library_batch -- <out_dir> <file>...
//! ```
//!
//! Each `<stem>.lua` is the library's output; a failure writes
//! `<stem>.err` holding the error, so a refusal is visible rather than silent.

use std::path::{Path, PathBuf};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(out_dir) = args.next().map(PathBuf::from) else {
        eprintln!("usage: library_batch <out_dir> <file>...");
        std::process::exit(2);
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        std::process::exit(1);
    }
    let (mut ok, mut failed) = (0usize, 0usize);
    for input in args.map(PathBuf::from) {
        let stem = stem_of(&input);
        let data = match std::fs::read(&input) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("read {}: {e}", input.display());
                failed += 1;
                continue;
            }
        };
        let written = match luau_core::decompile(&data) {
            Ok(src) => {
                ok += 1;
                std::fs::write(out_dir.join(format!("{stem}.lua")), src)
            }
            Err(e) => {
                failed += 1;
                std::fs::write(out_dir.join(format!("{stem}.err")), format!("{e:#}\n"))
            }
        };
        if let Err(e) = written {
            eprintln!("write {stem}: {e}");
        }
    }
    eprintln!("library_batch: {ok} ok, {failed} failed");
}

fn stem_of(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_string())
}
