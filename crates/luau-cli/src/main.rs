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
        #[arg(short, long)]
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
        #[arg(short, long)]
        out_dir: Option<PathBuf>,

        /// File extensions to process (comma-separated)
        #[arg(long, default_value = "bin,luac,bytecode,out")]
        extensions: String,

        /// Also produce .disasm files
        #[arg(long)]
        disasm: bool,
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

    match cli.command {
        Some(Commands::Decompile { input }) => {
            let data = read_input(input.as_deref())?;
            write_output(cli.output.as_deref(), &luau_core::decompile(&data)?)?;
        }

        Some(Commands::Disassemble { input, debug_info, opmap }) => {
            let data = read_input(input.as_deref())?;
            let text = if opmap {
                luau_core::disassemble_with_opmap(&data, None)?
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
            run_watch(&folder, out_dir.as_deref(), disasm, interval)?;
        }

        Some(Commands::Batch { input, out_dir, extensions, disasm }) => {
            run_batch(&input, out_dir.as_deref(), &extensions, disasm)?;
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
                write_output(cli.output.as_deref(), &luau_core::decompile(&data)?)?;
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

// ── Watch ──

fn run_watch(folder: &Path, out_dir: Option<&Path>, disasm: bool, interval_ms: u64) -> Result<()> {
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
                        match luau_core::decompile(&data) {
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

fn run_batch(input: &Path, out_dir: Option<&Path>, extensions: &str, disasm: bool) -> Result<()> {
    let out = out_dir.map(|p| p.to_path_buf()).unwrap_or_else(|| input.join("decompiled"));
    fs::create_dir_all(&out)?;

    let exts: Vec<&str> = extensions.split(',').map(|s| s.trim()).collect();
    let mut ok = 0u32;
    let mut fail = 0u32;
    let start = std::time::Instant::now();

    eprintln!("Batch: {} → {}", input.display(), out.display());

    for entry in fs::read_dir(input)?.flatten() {
        let path = entry.path();
        if !path.is_file() { continue; }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !exts.contains(&ext) { continue; }

        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");

        match fs::read(&path).map(|d| luau_core::decompile(&d)) {
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
