mod validate;
mod compare;
mod ansi;
mod probe_cmd;
mod opmap_db_cmd;
mod run_manifest;

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

    /// Database of MEASURED opcode permutations.
    ///
    /// Where `--opmap-cache` pools guesses, this holds permutations that were
    /// read off a client's own compiler with `probe align`. When an entry
    /// applies to a file, it is used exactly and no detector runs. When none
    /// applies, the decode is byte-for-byte what it would have been without
    /// this flag. Also settable via `LUAU_OPMAP_DB`.
    #[arg(long, global = true, value_name = "PATH")]
    opmap_db: Option<PathBuf>,

    /// Force one database entry by id instead of matching.
    ///
    /// If the named entry cannot decode the file this is an error, not a
    /// fallback: you asked for that entry and need to be told it does not fit.
    #[arg(long, global = true, value_name = "ID")]
    opmap_db_entry: Option<String>,

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

    /// Read a client's opcode permutation off its own compiler
    ///
    /// Every other path in this tool INFERS the permutation from structure,
    /// which has a ceiling. This one does not infer anything. Compile a set of
    /// programs you already have with a compiler whose numbering is documented,
    /// compile the same programs with the client whose numbering is secret, and
    /// line the two instruction streams up. The permutation is then a fact.
    #[command(subcommand)]
    Probe(ProbeCmd),

    /// Inspect and populate the measured-permutation database
    ///
    /// Read-only except for `import`, so a decompile can consult this database
    /// but never write to it.
    #[command(subcommand, name = "opmap-db")]
    OpmapDb(OpmapDbCmd),

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

#[derive(Subcommand, Debug)]
enum OpmapDbCmd {
    /// List every entry
    List,

    /// Show one entry's full opcode table
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },

    /// Add a map produced by `probe align`
    Import {
        /// The `probe align --out` document, or a bare {hex: NAME} map
        report: PathBuf,

        /// Override the id recorded in the report
        #[arg(long)]
        id: Option<String>,

        /// Human-readable label for this client build
        #[arg(long)]
        build: Option<String>,

        /// Replace an existing entry with the same id
        #[arg(long)]
        force: bool,
    },

    /// Explain, stage by stage, why a file does or does not match an entry
    Match {
        /// Bytecode file to test against the database
        input: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum ProbeCmd {
    /// Write the probe source set to a folder, ready to be compiled twice
    ///
    /// These are ordinary Luau programs, written to force the compiler to emit
    /// every opcode it can be made to emit — including the ones no real script
    /// ever produces by accident.
    Emit {
        /// Folder to write the sources and manifest into
        #[arg(long)]
        out: PathBuf,

        /// Which tier to write: core (small, 77 opcodes), heavy (two more,
        /// hundreds of KB), or all
        #[arg(long, default_value = "core")]
        tier: String,
    },

    /// Derive the permutation from two compilations of the probe sources
    ///
    /// Pairs files by name across the two folders. A file that does not align
    /// is reported and skipped; a prototype that does not align is reported and
    /// skipped. Nothing is ever guessed to fill a gap.
    Align {
        /// Folder (or single file) of probe sources compiled by upstream
        /// `luau-compile` — the numbering we already know
        #[arg(long)]
        canonical: PathBuf,

        /// Folder (or single file) of the SAME sources compiled by the client
        /// whose numbering is being derived
        #[arg(long)]
        client: PathBuf,

        /// Write the derived map here, ready for `opmap-db import`
        #[arg(long)]
        out: Option<PathBuf>,

        /// Label for this build, recorded in the derived map
        #[arg(long)]
        id: Option<String>,

        /// Refuse to write a map with fewer than this many opcodes pinned.
        /// A thin map is worse than none: it would be installed as exact.
        #[arg(long, default_value = "70")]
        min_pinned: usize,

        /// Machine-readable report
        #[arg(long)]
        json: bool,

        /// Write the map even when files contradicted each other. The
        /// contradicted bytes are left unpinned either way.
        #[arg(long)]
        allow_conflicts: bool,
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
    let db = DbSelection {
        path: db_path(&cli),
        entry_id: cli.opmap_db_entry.clone(),
    };

    match cli.command {
        Some(Commands::Decompile { input }) => {
            let data = read_input(input.as_deref())?;
            let source = decompile_with_store(&data, store.as_ref(), &db)?;
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
            run_watch(&folder, out_dir.as_deref(), disasm, interval, store.as_ref(), &db)?;
        }

        Some(Commands::Batch { input, out_dir, extensions, disasm }) => {
            run_batch(&input, out_dir.as_deref(), &extensions, disasm, store.as_ref(), &db)?;
        }

        Some(Commands::Scan { input, extensions }) => {
            run_scan(&input, &extensions, store.as_ref())?;
        }

        Some(Commands::OpmapDb(cmd)) => {
            let path = db
                .path
                .clone()
                .ok_or_else(|| anyhow::anyhow!("pass --opmap-db <PATH> (or set LUAU_OPMAP_DB)"))?;
            match cmd {
                OpmapDbCmd::List => opmap_db_cmd::run_list(&path)?,
                OpmapDbCmd::Show { id, json } => opmap_db_cmd::run_show(&path, &id, json)?,
                OpmapDbCmd::Import {
                    report,
                    id,
                    build,
                    force,
                } => opmap_db_cmd::run_import(
                    &path,
                    &report,
                    id.as_deref(),
                    build.as_deref(),
                    force,
                )?,
                OpmapDbCmd::Match { input } => opmap_db_cmd::run_match(&path, &input)?,
            }
        }

        Some(Commands::Probe(cmd)) => match cmd {
            ProbeCmd::Emit { out, tier } => probe_cmd::run_emit(&out, Some(&tier))?,
            ProbeCmd::Align {
                canonical,
                client,
                out,
                id,
                min_pinned,
                json,
                allow_conflicts,
            } => probe_cmd::run_align(&probe_cmd::AlignOptions {
                canonical: &canonical,
                client: &client,
                out: out.as_deref(),
                id: id.as_deref(),
                min_pinned,
                json,
                allow_conflicts,
            })?,
        },

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
                let source = decompile_with_store(&data, store.as_ref(), &db)?;
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

// ── Measured-permutation database ──

/// Which database to consult, and whether one entry was named explicitly.
pub struct DbSelection {
    pub path: Option<PathBuf>,
    pub entry_id: Option<String>,
}

/// Resolve the database path from the flag, falling back to `LUAU_OPMAP_DB`.
fn db_path(cli: &Cli) -> Option<PathBuf> {
    cli.opmap_db.clone().or_else(|| {
        std::env::var_os("LUAU_OPMAP_DB")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
}

/// Does a measured entry apply to this file?
///
/// Returns `Ok(None)` for every outcome except a verified match, so the caller
/// falls back to inference unchanged. The one hard error is a `--opmap-db-entry`
/// that does not fit: the user named it, so a silent fallback would be a lie.
fn resolve_db_entry(
    db: &DbSelection,
    data: &[u8],
) -> Result<Option<luau_core::parser::opmap_db::DbEntry>> {
    let Some(ref path) = db.path else {
        return Ok(None);
    };
    let loaded = opmap_db_cmd::load(path)?;
    let Ok(chunk) = luau_core::parser::parse(data) else {
        return Ok(None);
    };

    if let Some(ref id) = db.entry_id {
        // Verification failure here is fatal by design.
        loaded.lookup_by_id(&chunk, id)?;
        return Ok(loaded.get(id).cloned());
    }

    let result = loaded.lookup(&chunk);
    match result {
        luau_core::parser::opmap_db::DbLookup::Hit { ref entry_id, .. } => {
            log::debug!("opmap database: {}", result.describe());
            Ok(loaded.get(entry_id).cloned())
        }
        other => {
            log::debug!("opmap database: {}", other.describe());
            Ok(None)
        }
    }
}

/// Decompile, using and then contributing to the shared store when configured.
///
/// The database is consulted first. A verified entry short-circuits everything
/// else: no prior is read, and no ballot is cast, because a file decoded from a
/// measurement has nothing honest to contribute to a tally of guesses.
fn decompile_with_store(
    data: &[u8],
    store: Option<&PathBuf>,
    db: &DbSelection,
) -> Result<String> {
    if let Some(entry) = resolve_db_entry(db, data)? {
        return luau_core::decompile_with_plan(
            data,
            &luau_core::DecodePlan {
                prior: None,
                exact: Some(&entry),
            },
        )
        .map(|(src, _)| src);
    }

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
    db: &DbSelection,
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
                        match decompile_with_store(&data, store, db) {
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
    db: &DbSelection,
) -> Result<()> {
    // Each run gets its own timestamped folder. Writing into a directory that
    // already holds output from a different binary silently mixes generations,
    // and any percentage taken from a mixed folder is meaningless because
    // nothing records which build produced which file. `keep = 3` retains the
    // last few runs for comparison and removes older ones — and only ever
    // touches `run_*` folders this tool created.
    let base = out_dir.map(|p| p.to_path_buf()).unwrap_or_else(|| input.join("decompiled"));
    let out = run_manifest::prepare_run_dir(&base, 3)?;

    let mut ok = 0u32;
    let mut fail = 0u32;
    let start = std::time::Instant::now();
    let started_at = run_manifest::run_timestamp_human();

    eprintln!("Batch: {} → {}", input.display(), out.display());

    let files = bytecode_files(input, extensions)?;

    // Pool by default. Every script in a batch comes from ONE client build and
    // so shares ONE opcode permutation, but each exercises only a fraction of
    // it, so a single file's detectors see a partial, guess-heavy map. Pooling
    // every file's independent reading into a corpus-wide majority is simply
    // using more of the evidence that is already present, and it is decisive:
    // measured over a 628-file dump it takes the compile gate from 600 to 613
    // and semantically-clean files from 88 to 224, because the
    // {GETTABLEKS, SETTABLEKS, NAMECALL} trio — three opcodes with an identical
    // instruction shape, which single-file detection routinely confuses and
    // which was the cause of the `return {}` export-table losses — is resolved
    // by the majority once every file has voted.
    //
    // When the caller named a shared store we honour it (it may pool across
    // several runs). Otherwise we pool into a fresh run-local file inside the
    // output folder. Fresh-per-run is deliberate: a stale pool from an earlier
    // binary would silently mix opcode maps across builds, exactly the trap the
    // timestamped output dir exists to prevent.
    let local_pool = out.join(".opmap_pool.jsonl");
    let store: Option<&PathBuf> = store.or(Some(&local_pool));

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

        match fs::read(&path).map(|d| decompile_with_store(&d, store, db)) {
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

    // Check what we just produced, and record it. A folder of .lua files with
    // no provenance cannot be compared against anything.
    let semantic = summarise_semantics(&out);
    if let Some(s) = &semantic {
        let pct = if s.files_checked > 0 {
            100.0 * s.files_clean as f64 / s.files_checked as f64
        } else {
            0.0
        };
        eprintln!(
            "Semantic: {}/{} clean ({:.1}%), {} defects",
            s.files_clean, s.files_checked, pct, s.total_defects
        );
    }

    let (version, commit, dirty) = run_manifest::collect_build_info();
    let info = run_manifest::RunInfo {
        input: input.to_path_buf(),
        out_dir: out.clone(),
        started: started_at,
        decompiler_version: version,
        git_commit: commit,
        git_dirty: dirty,
        opmap_source: describe_opmap_source(store, db),
        total_inputs: (ok + fail) as usize,
        ok,
        failed: fail,
        elapsed_secs: elapsed.as_secs_f64(),
        semantic,
    };
    let manifest = run_manifest::write_manifest(&info)?;
    eprintln!("Manifest: {}", manifest.display());
    Ok(())
}

/// Run the semantic checker over every .lua file just written.
fn summarise_semantics(dir: &Path) -> Option<run_manifest::SemanticSummary> {
    use luau_core::decompiler::semantic_check::{check, Severity};
    use std::collections::BTreeMap;

    let entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .collect();
    if entries.is_empty() {
        return None;
    }

    let mut clean = 0usize;
    let mut total = 0usize;
    let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();

    for p in &entries {
        let Ok(src) = fs::read_to_string(p) else { continue };
        let protos = src.lines().take(20).find_map(|l| {
            l.trim()
                .strip_prefix("-- Protos:")?
                .trim()
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        });
        let wrong: Vec<_> = check(&src, protos)
            .into_iter()
            .filter(|f| f.severity == Severity::Wrong)
            .collect();
        if wrong.is_empty() {
            clean += 1;
            continue;
        }
        total += wrong.len();
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for f in &wrong {
            *seen.entry(f.check).or_insert(0) += 1;
        }
        for (k, n) in seen {
            let e = counts.entry(k.to_string()).or_insert((0, 0));
            e.0 += 1;
            e.1 += n;
        }
    }

    let mut by_check: Vec<(String, usize, usize)> =
        counts.into_iter().map(|(k, (f, n))| (k, f, n)).collect();
    by_check.sort_by(|a, b| b.2.cmp(&a.2));

    Some(run_manifest::SemanticSummary {
        files_checked: entries.len(),
        files_clean: clean,
        total_defects: total,
        by_check,
    })
}

/// Describe where the opcode map came from — it changes the output enough
/// that a result is not interpretable without it.
fn describe_opmap_source(store: Option<&PathBuf>, db: &DbSelection) -> String {
    if let Some(dbp) = &db.path {
        match &db.entry_id {
            Some(id) => format!(
                "exact entry `{}` from opmap database `{}` (no detection, no completion)",
                id,
                dbp.display()
            ),
            None => format!(
                "opmap database `{}`, entry matched per file where applicable",
                dbp.display()
            ),
        }
    } else if let Some(p) = store {
        format!("pooled cache at `{}` plus per-chunk detection", p.display())
    } else {
        "per-chunk detection only (no cache, no database)".to_string()
    }
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
