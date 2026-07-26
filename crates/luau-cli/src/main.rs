mod validate;
mod compare;
mod ansi;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Parser, Debug)]
#[command(
    name = "luau-decompiler",
    about = "The best Luau bytecode decompiler — runs 100% locally",
    version,
    long_about = "A fast, fully offline Luau bytecode decompiler.\n\
                   Supports bytecode v3-8 | Windows, macOS, Linux.\n\n\
                   Quick start:\n  \
                     luau-decompiler script.bin         # decompile a file\n  \
                     luau-decompiler watch ./dir        # auto-decompile folder\n  \
                     luau-decompiler batch ./dir        # batch process a folder\n  \
                     luau-decompiler scan ./dir         # pool opcode-shuffle evidence\n  \
                     luau-decompiler validate out.lua   # syntax-check a Luau file\n  \
                     luau-decompiler compare a.lua b.lua  # diff + similarity\n\n\
                   Use --help on any subcommand for more detail."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Shortcut: path to bytecode file to decompile
    input: Option<PathBuf>,

    /// Output file (stdout if omitted)
    #[arg(short, long, global = true)]
    output: Option<PathBuf>,

    /// Shared opcode-shuffle evidence store.
    ///
    /// Every script from one Roblox client version shares a single opcode
    /// permutation, but a single script only exercises a fraction of it. Point
    /// this at a file and each decompile contributes its own independent
    /// reading of the shuffle, then decodes using the pooled majority of every
    /// reading so far. Scripts from different client versions may share one
    /// store; they are kept apart automatically. Also settable via
    /// `LUAU_OPMAP_CACHE`.
    #[arg(long, global = true, value_name = "PATH")]
    opmap_cache: Option<PathBuf>,

    /// Verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Decompile a bytecode file to Luau source
    Decompile {
        /// Bytecode file (- for stdin)
        input: Option<PathBuf>,
    },

    /// Disassemble bytecode into readable opcodes
    Disassemble {
        input: Option<PathBuf>,
        /// Include debug info (lines, local names)
        #[arg(long)]
        debug_info: bool,
        /// Apply opmap detection/remap before disassembly (diagnostic view)
        #[arg(long)]
        opmap: bool,
    },

    /// Show bytecode metadata
    Info {
        input: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },

    /// Watch a folder and auto-decompile new bytecode files
    Watch {
        /// Folder to watch
        #[arg(default_value = ".")]
        folder: PathBuf,

        /// Output folder for .lua files
        //
        // No `short`: `-o` is taken by the global `--output`. Declaring it here
        // too made clap abort on startup, so `watch` could not run at all.
        #[arg(long)]
        out_dir: Option<PathBuf>,

        /// Also produce .disasm files
        #[arg(long)]
        disasm: bool,

        /// Poll interval in ms
        #[arg(long, default_value = "250")]
        interval: u64,
    },

    /// Batch decompile every bytecode file in a folder
    Batch {
        /// Input folder
        input: PathBuf,

        /// Output folder
        //
        // No `short`: `-o` is taken by the global `--output`. Declaring it here
        // too made clap abort on startup, so `batch` could not run at all.
        #[arg(long)]
        out_dir: Option<PathBuf>,

        /// File extensions to process (comma-separated)
        #[arg(long, default_value = "bin,luac,bytecode,out")]
        extensions: String,

        /// Also produce .disasm files
        #[arg(long)]
        disasm: bool,
    },

    /// Contribute readings of the opcode shuffle to a store, decoding nothing
    ///
    /// Only useful with `--opmap-cache`. Every script from one Roblox client
    /// version shares one opcode permutation, but a single script exercises
    /// only a fraction of it, so a decompile is far more accurate once the
    /// whole set has been pooled than it is on the first arrival.
    ///
    /// `batch` already pools its own folder before decoding any of it, so it
    /// needs no help. This exists for the case `batch` cannot serve: scripts
    /// arriving one at a time, where the decision to decode is made before the
    /// rest of the set exists. Scanning the set first, then decompiling it,
    /// gives a streaming caller the same pooled evidence a batch gets.
    Scan {
        /// Bytecode file, or a folder of them
        input: PathBuf,

        /// File extensions to read when INPUT is a folder (comma-separated)
        #[arg(long, default_value = "bin,luac,bytecode,out")]
        extensions: String,
    },

    /// Validate Luau source — syntax-check a .lua/.luau file
    ///
    /// Uses external `luau`/`luau-analyze` if found on PATH for a full parse.
    /// Otherwise falls back to a built-in lexical sanity check (balanced
    /// keywords, brackets, quotes). Exit code is 0 on OK, 1 on error.
    Validate {
        /// Luau source file (- for stdin)
        input: Option<PathBuf>,

        /// Force the built-in checker (skip external luau/luau-analyze)
        #[arg(long)]
        builtin: bool,

        /// Silence colored output
        #[arg(long)]
        no_color: bool,
    },

    /// Compare two Luau source files — line diff + Jaccard similarity
    ///
    /// Reports a unified line diff plus Jaccard similarity computed on
    /// whitespace-normalized non-blank lines. Exit code is 0 if identical,
    /// 1 if different.
    Compare {
        /// First file
        a: PathBuf,

        /// Second file
        b: PathBuf,

        /// Hide the diff; only print similarity
        #[arg(long)]
        stats_only: bool,

        /// Silence colored output
        #[arg(long)]
        no_color: bool,

        /// Max diff lines to show (0 = unlimited)
        #[arg(long, default_value = "200")]
        max_lines: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    env_logger::Builder::new()
        .filter_level(if cli.verbose {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .init();

    let store = store_path(&cli);

    match cli.command {
        Some(Commands::Decompile { input }) => {
            let data = read_input(input.as_deref())?;
            let source = decompile_with_store(&data, store.as_ref())?;
            write_output(cli.output.as_deref(), &source)?;
        }

        Some(Commands::Disassemble { input, debug_info, opmap }) => {
            let data = read_input(input.as_deref())?;
            let text = if opmap {
                // Diagnostic view of what the lifter actually processes, so it
                // must apply the same map the lifter would — including the
                // shared store, or it would show a map the decompile never used.
                let consulted = store.as_ref().map(|p| consult_store(p, &data));
                let prior = consulted.as_ref().and_then(|(m, _)| *m);
                let text = luau_core::disassemble_with_opmap(&data, prior.as_ref())?;
                if let (Some(p), Some((_, Some(b)))) = (store.as_ref(), consulted.as_ref()) {
                    cast_ballot(p, b);
                }
                text
            } else {
                luau_core::disassemble(&data, debug_info)?
            };
            write_output(cli.output.as_deref(), &text)?;
        }

        Some(Commands::Info { input, json }) => {
            let data = read_input(input.as_deref())?;
            let info = luau_core::info(&data)?;
            let text = if json {
                serde_json::to_string_pretty(&info)?
            } else {
                format_info(&info)
            };
            write_output(cli.output.as_deref(), &text)?;
        }

        Some(Commands::Watch { folder, out_dir, disasm, interval }) => {
            run_watch(&folder, out_dir.as_deref(), disasm, interval, store.as_ref())?;
        }

        Some(Commands::Batch { input, out_dir, extensions, disasm }) => {
            run_batch(&input, out_dir.as_deref(), &extensions, disasm, store.as_ref())?;
        }

        Some(Commands::Scan { input, extensions }) => {
            run_scan(&input, &extensions, store.as_ref())?;
        }

        Some(Commands::Validate { input, builtin, no_color }) => {
            let color = ansi::choose(!no_color);
            let src_bytes = read_input(input.as_deref())?;
            let src = String::from_utf8_lossy(&src_bytes);
            let label = input
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<stdin>".to_string());
            let result = validate::validate_source(&src, builtin);
            let exit_code = validate::report(&label, &result, &color);
            std::process::exit(exit_code);
        }

        Some(Commands::Compare { a, b, stats_only, no_color, max_lines }) => {
            let color = ansi::choose(!no_color);
            let src_a = fs::read_to_string(&a)?;
            let src_b = fs::read_to_string(&b)?;
            let report = compare::compare(&src_a, &src_b);
            let exit_code = compare::print_report(
                &a.display().to_string(),
                &b.display().to_string(),
                &report,
                stats_only,
                max_lines,
                &color,
            );
            std::process::exit(exit_code);
        }

        None => {
            if let Some(ref path) = cli.input {
                let data = read_input(Some(path.as_path()))?;
                let source = decompile_with_store(&data, store.as_ref())?;
                write_output(cli.output.as_deref(), &source)?;
            } else {
                // No args at all — print help hint
                let c = ansi::choose(true);
                eprintln!("{}Luau Decompiler v{}{}", c.bold, env!("CARGO_PKG_VERSION"), c.reset);
                eprintln!();
                eprintln!("  {}Quick start:{}", c.cyan, c.reset);
                eprintln!("    luau-decompiler <file.bin>        Decompile a single file");
                eprintln!("    luau-decompiler watch <dir>       Auto-decompile a folder");
                eprintln!("    luau-decompiler batch <dir>       Batch process a folder");
                eprintln!("    luau-decompiler validate <f.lua>  Syntax-check a Luau file");
                eprintln!("    luau-decompiler compare a b       Diff + similarity");
                eprintln!();
                eprintln!("  Run with {}--help{} for full options, {}--version{} for version.",
                    c.bold, c.reset, c.bold, c.reset);
            }
        }
    }

    Ok(())
}

// ── Shared opcode-shuffle evidence store ──
//
// A decompiler invocation normally sees one script and must infer the whole
// opcode permutation from it. That is weak evidence — a typical script
// exercises well under half the opcode set, so most of the permutation is
// simply not observable from it. Scripts from one client version all share the
// SAME permutation, though, so pooling one independent reading per script and
// taking the majority recovers substantially more of it than any single script
// can.
//
// Deliberately opt-in. With no store configured every code path below is
// exactly what it was before this existed: solo detection, no shared state, no
// file I/O. Turning it on is a decision the caller makes.

/// Resolve the store path from the flag, falling back to `LUAU_OPMAP_CACHE`.
fn store_path(cli: &Cli) -> Option<PathBuf> {
    cli.opmap_cache.clone().or_else(|| {
        std::env::var_os("LUAU_OPMAP_CACHE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
}

/// Pool the store's evidence into a prior for THIS script.
///
/// Only ballots that describe the same opcode permutation take part — a store
/// that outlives a Roblox client update holds readings of two different
/// permutations, and pooling those produces a map correct for neither. The
/// script's own reading is the probe used to pick its peers, so a store may
/// span any number of client versions without the groups interfering.
///
/// A missing or unreadable store is not an error: it means "no evidence yet",
/// and the caller correctly falls back to solo detection. Corrupt lines are
/// skipped rather than fatal — a truncated tail from an interrupted write must
/// cost at most the ballots in it.
fn load_prior(path: &Path, probe: &luau_core::parser::consensus::Ballot) -> Option<[u8; 256]> {
    let text = fs::read_to_string(path).ok()?;
    let book = luau_core::parser::consensus::decode_book(&text);
    let cfg = luau_core::parser::consensus::ConsensusConfig::default();
    let resolved = book.resolve_for(probe, &cfg);
    if resolved.is_empty() {
        return None;
    }
    log::debug!(
        "opmap consensus: {} of {} ballots share this shuffle, {} byte mappings published",
        resolved.ballots,
        book.len(),
        resolved.published()
    );
    Some(resolved.map)
}

/// Contribute this script's own independent reading of the shuffle.
///
/// Append-only, one JSON line per ballot, keyed by content hash so that
/// re-decompiling the same script REPLACES its vote instead of adding another —
/// otherwise a script that happens to be processed often would outweigh the
/// rest of the corpus. Failures are silent by design: contributing evidence is
/// a side benefit, and it must never turn a successful decompile into an error.
fn cast_ballot(path: &Path, ballot: &luau_core::parser::consensus::Ballot) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let line = luau_core::parser::consensus::encode_ballot(ballot);
    if let Ok(mut fh) = fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(fh, "{}", line);
    }
}

/// The prior for this script, and the ballot to file once it is decoded.
///
/// `None` for bytecode carrying no Roblox shuffle — canonical Luau must neither
/// read from nor vote in a Roblox tally.
fn consult_store(
    path: &Path,
    data: &[u8],
) -> (Option<[u8; 256]>, Option<luau_core::parser::consensus::Ballot>) {
    match luau_core::observe_ballot(data) {
        Some(ballot) => (load_prior(path, &ballot), Some(ballot)),
        None => (None, None),
    }
}

/// Contribute one reading per file to the store, decoding nothing.
///
/// Returns how many files actually had a reading to give — canonical Luau
/// carries no Roblox shuffle, so it neither votes nor is an error. Unreadable
/// files are skipped for the same reason a failed decompile does not abort a
/// batch: collecting evidence is best-effort.
fn cast_ballots(store: &Path, files: &[PathBuf]) -> usize {
    let mut cast = 0usize;
    for f in files {
        if let Ok(data) = fs::read(f) {
            if let Some(b) = luau_core::observe_ballot(&data) {
                cast_ballot(store, &b);
                cast += 1;
            }
        }
    }
    cast
}

/// Bytecode files in a folder, in a stable order.
fn bytecode_files(dir: &Path, extensions: &str) -> Result<Vec<PathBuf>> {
    let exts: Vec<&str> = extensions.split(',').map(|s| s.trim()).collect();
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| exts.contains(&p.extension().and_then(|e| e.to_str()).unwrap_or("")))
        .collect();
    files.sort();
    Ok(files)
}

/// Decompile, using and then contributing to the shared store when configured.
fn decompile_with_store(data: &[u8], store: Option<&PathBuf>) -> Result<String> {
    let Some(path) = store else {
        return luau_core::decompile(data);
    };
    let (prior, ballot) = consult_store(path, data);
    let out = luau_core::decompile_with_opmap(data, prior.as_ref()).map(|(src, _)| src);
    // Cast only after a successful decode, and only what this script itself
    // observed — never the prior it was handed.
    if out.is_ok() {
        if let Some(b) = ballot {
            cast_ballot(path, &b);
        }
    }
    out
}

// ── Watch ──

fn run_watch(
    folder: &Path,
    out_dir: Option<&Path>,
    disasm: bool,
    interval_ms: u64,
    store: Option<&PathBuf>,
) -> Result<()> {
    let out = out_dir.unwrap_or(folder);
    fs::create_dir_all(out)?;

    let exts = ["bin", "luac", "bytecode", "out"];
    let mut last_mod: std::collections::HashMap<PathBuf, SystemTime> = Default::default();

    eprintln!("╔══════════════════════════════════════════╗");
    eprintln!("║  Luau Decompiler — Watch Mode            ║");
    eprintln!("╠══════════════════════════════════════════╣");
    eprintln!("║  Watching: {:<29}║", format!("{}", folder.display()));
    eprintln!("║  Output:   {:<29}║", format!("{}", out.display()));
    eprintln!("║  Disasm:   {:<29}║", if disasm { "yes" } else { "no" });
    eprintln!("╚══════════════════════════════════════════╝");
    eprintln!();
    eprintln!("  Drop .bin files into the folder to decompile them.");
    eprintln!("  Press Ctrl+C to stop.");
    eprintln!();

    loop {
        if let Ok(entries) = fs::read_dir(folder) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() { continue; }

                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !exts.contains(&ext) { continue; }

                let modified = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);

                if last_mod.get(&path) == Some(&modified) { continue; }
                last_mod.insert(path.clone(), modified);

                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");

                match fs::read(&path) {
                    Ok(data) if !data.is_empty() => {
                        // Decompile
                        match decompile_with_store(&data, store) {
                            Ok(source) => {
                                let p = out.join(format!("{}.lua", stem));
                                let _ = fs::write(&p, &source);
                                eprintln!("  ✓ {} → {}", path.display(), p.display());
                            }
                            Err(e) => eprintln!("  ✗ {} — {}", path.display(), e),
                        }
                        // Disasm
                        if disasm {
                            if let Ok(text) = luau_core::disassemble(&data, true) {
                                let p = out.join(format!("{}.disasm", stem));
                                let _ = fs::write(&p, &text);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}

// ── Batch ──

fn run_batch(
    input: &Path,
    out_dir: Option<&Path>,
    extensions: &str,
    disasm: bool,
    store: Option<&PathBuf>,
) -> Result<()> {
    let out = out_dir.map(|p| p.to_path_buf()).unwrap_or_else(|| input.join("decompiled"));
    fs::create_dir_all(&out)?;

    let mut ok = 0u32;
    let mut fail = 0u32;
    let start = std::time::Instant::now();

    eprintln!("Batch: {} → {}", input.display(), out.display());

    let files = bytecode_files(input, extensions)?;

    // Pre-pass: pool every script's reading of the opcode shuffle BEFORE
    // decoding any of them.
    //
    // Decompiling straight through would make the evidence ramp up as it went,
    // so the first script decoded would see almost none of it. That is not a
    // small effect — the same folder scores ~63% decoded in arrival order
    // against ~69% once every script has contributed. A batch, unlike a live
    // stream, knows its own boundary, so there is no reason to make the early
    // files pay for arriving early.
    if let Some(path) = store {
        cast_ballots(path, &files);
    }

    for path in files {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");

        match fs::read(&path).map(|d| decompile_with_store(&d, store)) {
            Ok(Ok(source)) => {
                fs::write(out.join(format!("{}.lua", stem)), &source)?;
                if disasm {
                    if let Ok(data) = fs::read(&path) {
                        if let Ok(text) = luau_core::disassemble(&data, true) {
                            let _ = fs::write(out.join(format!("{}.disasm", stem)), &text);
                        }
                    }
                }
                ok += 1;
            }
            _ => { fail += 1; }
        }
    }

    let elapsed = start.elapsed();
    eprintln!("Done: {} ok, {} failed ({:.1}s)", ok, fail, elapsed.as_secs_f64());
    Ok(())
}

// ── Scan ──

/// Pool readings of the opcode shuffle without decoding anything.
///
/// The pre-pass inside `run_batch` is the same operation, but a batch can only
/// pool the folder it was handed. A caller that receives scripts one at a time
/// has no folder to hand over at the moment it must decide, and decoding in
/// arrival order makes every early script pay for the evidence it has not
/// collected yet — measured at 0 of 47 corpus files recovered in arrival order
/// against 6-10 once the whole set has voted first. Exposing the pre-pass on
/// its own lets such a caller scan first and decompile second.
fn run_scan(input: &Path, extensions: &str, store: Option<&PathBuf>) -> Result<()> {
    let Some(path) = store else {
        anyhow::bail!(
            "scan has nowhere to record what it reads — pass --opmap-cache <PATH> \
             (or set LUAU_OPMAP_CACHE)"
        );
    };

    let files = if input.is_dir() {
        bytecode_files(input, extensions)?
    } else {
        vec![input.to_path_buf()]
    };

    let cast = cast_ballots(path, &files);
    eprintln!(
        "Scanned {} file(s) → {} contributed a reading of the opcode shuffle to {}",
        files.len(),
        cast,
        path.display()
    );
    Ok(())
}

// ── Helpers ──

fn read_input(path: Option<&Path>) -> Result<Vec<u8>> {
    match path {
        Some(p) if p.to_str() != Some("-") => Ok(fs::read(p)?),
        _ => {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf)?;
            Ok(buf)
        }
    }
}

fn write_output(path: Option<&Path>, content: &str) -> Result<()> {
    match path {
        Some(p) => {
            if let Some(parent) = p.parent() { fs::create_dir_all(parent)?; }
            fs::write(p, content)?;
            eprintln!("→ {}", p.display());
        }
        None => print!("{}", content),
    }
    Ok(())
}

fn format_info(info: &luau_core::BytecodeInfo) -> String {
    let mut s = format!("Luau Bytecode v{} | {} protos | {} strings | main={}\n\n",
        info.version, info.num_protos, info.num_strings, info.main_proto);
    for p in &info.protos {
        s.push_str(&format!(
            "  [{:>2}] {:<30} params={} stack={} upvals={} insns={} consts={} line={}{}\n",
            p.index,
            p.name.as_deref().unwrap_or("<anon>"),
            p.num_params, p.max_stack, p.num_upvalues,
            p.num_instructions, p.num_constants, p.line_defined,
            if p.has_debug_info { " +debug" } else { "" },
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder under the OS temp dir, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "luau-cli-test-{}-{}-{:?}",
                tag,
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bytecode_files_filters_by_extension_and_sorts() {
        let d = TempDir::new("exts");
        for name in ["b.luac", "a.bin", "notes.txt", "c.bytecode"] {
            fs::write(d.join(name), b"x").unwrap();
        }
        fs::create_dir_all(d.join("sub.bin")).unwrap(); // a DIRECTORY named like a file

        let found = bytecode_files(&d.0, "bin,luac,bytecode,out").unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.bin", "b.luac", "c.bytecode"],
            "must keep only bytecode extensions, only files, in sorted order");
    }

    #[test]
    fn bytecode_files_tolerates_spaces_in_the_extension_list() {
        let d = TempDir::new("spaces");
        fs::write(d.join("a.bin"), b"x").unwrap();
        fs::write(d.join("b.luac"), b"x").unwrap();
        assert_eq!(bytecode_files(&d.0, " bin , luac ").unwrap().len(), 2);
    }

    #[test]
    fn bytecode_files_reports_a_missing_folder() {
        let d = TempDir::new("missing");
        assert!(bytecode_files(&d.join("nope"), "bin").is_err());
    }

    #[test]
    fn cast_ballots_ignores_files_with_nothing_to_say() {
        // Neither an unreadable path nor a non-bytecode blob carries a reading
        // of a Roblox opcode shuffle, and neither may abort the pass or invent
        // a vote. An empty store is the correct outcome, not an error.
        let d = TempDir::new("ballots");
        let store = d.join("store.jsonl");
        fs::write(d.join("junk.bin"), b"not bytecode at all").unwrap();

        let cast = cast_ballots(&store, &[d.join("junk.bin"), d.join("absent.bin")]);
        assert_eq!(cast, 0, "nothing observable must mean no ballots");
        assert!(!store.exists(), "an empty pass must not create a store");
    }

    #[test]
    fn scan_without_a_store_is_an_error_not_a_silent_no_op() {
        // `scan` exists only to record evidence. With nowhere to record it the
        // command would do nothing at all, and a caller that scanned a whole
        // folder before decompiling would silently get the un-pooled result.
        let d = TempDir::new("nostore");
        fs::write(d.join("a.bin"), b"x").unwrap();
        assert!(run_scan(&d.join("a.bin"), "bin", None).is_err());
    }
}
