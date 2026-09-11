//! `luau-protect` CLI: read a Luau source file, emit a protected Luau script.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};

use luau_compiler::{protect, ProtectOptions};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("luau-protect: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.iter().any(|a| a == "-h" || a == "--help") {
        print_help(&args[0]);
        return Ok(());
    }

    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut opts = ProtectOptions::default();

    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-o" | "--output" => {
                i += 1;
                output = Some(PathBuf::from(
                    args.get(i).context("--output requires a path")?,
                ));
            }
            "--no-encrypt" => opts.encrypt_constants = false,
            "--flatten" => opts.flatten_control_flow = true,
            "--junk" => opts.inject_junk = true,
            "--permute" => opts.permute_opcodes = true,
            "--obfuscate-numbers" => opts.obfuscate_numbers = true,
            "--encrypt-operands" => opts.encrypt_operands = true,
            "--lazy-strings" => opts.lazy_strings = true,
            "--max" => {
                opts.inject_junk = true;
                opts.permute_opcodes = true;
                opts.obfuscate_numbers = true;
                opts.encrypt_operands = true;
                opts.lazy_strings = true;
                opts.flatten_control_flow = true;
            }
            "--seed" => {
                i += 1;
                let s = args.get(i).context("--seed requires a value")?;
                opts.seed = Some(
                    s.parse::<u64>()
                        .with_context(|| format!("invalid seed {s}"))?,
                );
            }
            other if other.starts_with('-') => {
                anyhow::bail!("unknown option {other}");
            }
            _ => {
                if input.is_some() {
                    anyhow::bail!("multiple input files not supported");
                }
                input = Some(PathBuf::from(a));
            }
        }
        i += 1;
    }

    let input = input.context("input file required")?;
    let source = std::fs::read_to_string(&input)
        .with_context(|| format!("reading {}", input.display()))?;
    let protected = protect(&source, &opts).map_err(|e| anyhow::anyhow!(e))?;

    match output {
        Some(p) => {
            std::fs::write(&p, &protected)
                .with_context(|| format!("writing {}", p.display()))?;
            eprintln!(
                "luau-protect: wrote {} bytes to {}",
                protected.len(),
                p.display()
            );
        }
        None => {
            print!("{protected}");
        }
    }
    Ok(())
}

fn print_help(prog: &str) {
    eprintln!(
        "usage: {prog} <input.lua> [-o output.lua] [--no-encrypt] [--flatten] [--junk] [--permute] [--seed N]"
    );
    eprintln!();
    eprintln!("Compile Luau source into a protected, self-contained Luau script.");
    eprintln!();
    eprintln!("Default behavior emits the Phase 1 baseline (custom VM, no encryption).");
    eprintln!("Flags toggle additional Phases as they land:");
    eprintln!("  --no-encrypt          (Phase 2) skip constant/bytecode encryption");
    eprintln!("  --flatten             (Phase 3) flatten control flow into state machine");
    eprintln!("  --junk                (Phase 4) inject decoy constants");
    eprintln!("  --permute             (Phase 5) permute opcode IDs for this build");
    eprintln!("  --obfuscate-numbers   (Phase 7A) emit numeric constants as bit32 expressions");
    eprintln!("  --encrypt-operands    (Phase 7B) XOR-scramble instruction operand bytes");
    eprintln!("  --lazy-strings        (Phase 7C) decrypt string constants on first use");
    eprintln!("  --max                 enable every phase (recommended for production builds)");
    eprintln!("  --seed N              force reproducible RNG seed (default: OS entropy)");
}
