//! Run files through the LIBRARY entry points, the calls `luau-wasm`,
//! `luau-server` and the `luau-ffi` C ABI make.
//!
//! Exists to measure library-vs-CLI parity: run it and `luau-decompiler` over
//! the same inputs and diff the two output folders.
//!
//! ```text
//! cargo run --release -p luau-core --example library_batch -- [options] <out_dir> <file>...
//!   --fallback         V10PlusMode::DetectorFallback (the CLI's v10+ behaviour)
//!   --custom-db PATH   OpmapDbChoice::Custom (the library twin of --opmap-db)
//!   --disasm           disassemble_with_options(bytes, true, opts) -> <stem>.disasm
//!   --disasm-opmap     disassemble_with_opmap_options(bytes, opts) -> <stem>.disasm
//!                      (the listing through the map the decompile uses)
//!   --opmap-none       decompile_with_opmap(bytes, None) -> <stem>.lua
//!   --disasm-opmap-none disassemble_with_opmap(bytes, None) -> <stem>.disasm
//!   --info             info(bytes) -> <stem>.info (Debug form)
//! ```
//!
//! Each output is written per input stem; a failure writes `<stem>.err`
//! holding the error, so a refusal is visible rather than silent.

use luau_core::{DecompileOptions, OpmapDbChoice, V10PlusMode};
use std::path::{Path, PathBuf};

enum Op {
    Decompile,
    Disasm,
    DisasmOpmap,
    OpmapNone,
    DisasmOpmapNone,
    Info,
}

fn main() {
    let mut args = std::env::args_os().skip(1).peekable();
    let (mut fallback, mut custom, mut op) = (false, None::<PathBuf>, Op::Decompile);
    while let Some(flag) = args.peek().and_then(|a| a.to_str()).map(str::to_owned) {
        match flag.as_str() {
            "--fallback" => fallback = true,
            "--custom-db" => {
                args.next();
                custom = args.peek().map(PathBuf::from);
            }
            "--disasm" => op = Op::Disasm,
            "--disasm-opmap" => op = Op::DisasmOpmap,
            "--opmap-none" => op = Op::OpmapNone,
            "--disasm-opmap-none" => op = Op::DisasmOpmapNone,
            "--info" => op = Op::Info,
            _ => break,
        }
        args.next();
    }
    let Some(out_dir) = args.next().map(PathBuf::from) else {
        eprintln!(
            "usage: library_batch [--fallback] [--custom-db PATH]              [--disasm|--disasm-opmap|--opmap-none|--disasm-opmap-none|--info] <out_dir> <file>..."
        );
        std::process::exit(2);
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        std::process::exit(1);
    }
    let custom_db = custom.map(|p| match luau_core::parser::opmap_db::OpmapDb::load(&p) {
        Ok((db, _)) => db,
        Err(e) => {
            eprintln!("custom db {}: {e:#}", p.display());
            std::process::exit(1);
        }
    });
    let opts = DecompileOptions {
        opmap_db: custom_db.as_ref().map_or(OpmapDbChoice::Bundled, OpmapDbChoice::Custom),
        v10plus: if fallback { V10PlusMode::DetectorFallback } else { V10PlusMode::MeasuredOnly },
    };
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
        let (ext, result) = match op {
            Op::Decompile => ("lua", luau_core::decompile_with_options(&data, &opts)),
            Op::Disasm => ("disasm", luau_core::disassemble_with_options(&data, true, &opts)),
            Op::DisasmOpmap => {
                ("disasm", luau_core::disassemble_with_opmap_options(&data, &opts))
            }
            Op::OpmapNone => {
                ("lua", luau_core::decompile_with_opmap(&data, None).map(|(src, _)| src))
            }
            Op::DisasmOpmapNone => ("disasm", luau_core::disassemble_with_opmap(&data, None)),
            Op::Info => ("info", luau_core::info(&data).map(|i| format!("{i:#?}\n"))),
        };
        let written = match result {
            Ok(text) => {
                ok += 1;
                std::fs::write(out_dir.join(format!("{stem}.{ext}")), text)
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
