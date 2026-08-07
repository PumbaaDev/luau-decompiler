//! `...` must only appear inside a function that is actually vararg.
//!
//! ── THE DEFECT ──────────────────────────────────────────────────────────
//! `ReplicatedStorage.Collectors.LocalCollect` decompiled to:
//!
//!     local function HasCapacity()                 -- params=0, NOT vararg
//!         local service = game:GetService(...)     -- illegal
//!
//! `luau-compile` rejects it outright:
//!
//!     SyntaxError: Cannot use '...' outside of a vararg function
//!
//! Two independent facts show this is our bug, not a hard case:
//!   * `info` reports proto 2 (`HasCapacity`) as `params=0` with no vararg
//!     flag. That flag is READ FROM THE BYTECODE HEADER
//!     (parser/mod.rs:239), so it is authoritative — not inferred.
//!   * `GetService` takes a string. `...` there is not a plausible argument
//!     under any reading of the source.
//!
//! ── ROOT CAUSE ──────────────────────────────────────────────────────────
//! The `GETVARARGS` handler (and its Roblox twin `RbxExt97`) write
//! `Expr::Varargs` into a register unconditionally, with no check that the
//! enclosing proto is vararg. `RbxExt97`'s own comment even asserts "Proto
//! is vararg=true" — an assumption nothing verifies.
//!
//! LocalCollect reports **0 unmapped opcodes** but **9 of 42 bytes filled by
//! bijection completion**. Completion assigns leftover bytes to leftover
//! opcodes to finish the permutation, so a byte that is really something else
//! can land on GETVARARGS. The emitter then turns a mis-decode into output
//! that cannot compile.
//!
//! ── WHY GUARD RATHER THAN CHASE THE MAP ─────────────────────────────────
//! Perfect opcode detection is not on offer. But `is_vararg` comes from the
//! file, so the guard is sound regardless of what the map got wrong: if a
//! non-vararg proto appears to execute GETVARARGS, the decode is wrong, and
//! emitting `...` converts a recoverable mis-decode into uncompilable source.
//!
//! Run with:
//!   BC_CORPUS=<dir> cargo test -p luau-core --release \
//!     --test vararg_legality -- --nocapture

use std::path::{Path, PathBuf};

fn corpus() -> Vec<PathBuf> {
    let dir = std::env::var("BC_CORPUS").unwrap_or_else(|_| {
        r"C:\Users\jep\AppData\Local\Potassium\workspace\bc_extract_1786138100".to_string()
    });
    let p = Path::new(&dir);
    if !p.exists() {
        return Vec::new();
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    v.sort();
    v
}

/// Does this line use `...` as a VARARG, outside any string literal?
///
/// Scans character by character, tracking quote state, so prose ellipses in
/// `"Look at you..."` are not mistaken for varargs. Handles backslash escapes
/// and both quote styles. Long-bracket strings (`[[...]]`) are rare in
/// decompiled output and are not handled — if they appear, this errs toward a
/// false positive, which the test surfaces rather than hides.
fn contains_vararg_outside_strings(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == '\\' {
                    i += 2; // skip escaped char
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == '.' && i + 2 < chars.len() && chars[i + 1] == '.' && chars[i + 2] == '.' {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Walk decompiled source and report functions that use `...` without
/// declaring it. Returns (line, declaration) pairs.
///
/// The main chunk is legitimately vararg in Lua, so only text INSIDE a
/// `function`/`local function` declaration counts.
fn illegal_vararg_uses(src: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    // Stack of (declares_vararg, decl_text, decl_line) for open functions.
    let mut stack: Vec<(bool, String, usize)> = Vec::new();

    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        if t.starts_with("--") {
            continue;
        }

        let is_decl = t.starts_with("function ")
            || t.starts_with("local function ")
            || t.contains("= function(");
        if is_decl {
            // Does the parameter list contain `...`?
            let params = t
                .split_once('(')
                .and_then(|(_, rest)| rest.split_once(')'))
                .map(|(p, _)| p)
                .unwrap_or("");
            stack.push((params.contains("..."), t.to_string(), i + 1));
            continue;
        }

        // `...` used while inside at least one function.
        //
        // Must ignore ellipses inside STRING LITERALS. The first version used a
        // bare `contains("...")` and reported 20 false positives — every one an
        // NPC dialogue file, because lines like
        //     Say = { "Look at you...", ... }
        // are prose, not varargs. SpiritBearInit alone has 2,593 strings and
        // scored 570 "uses", none of them real.
        if !stack.is_empty() && contains_vararg_outside_strings(t) {
            // Legal only if the INNERMOST enclosing function declares it.
            let (declares, decl, decl_line) = stack.last().unwrap();
            if !declares {
                out.push((
                    i + 1,
                    format!("{} (declared line {})", decl.chars().take(60).collect::<String>(), decl_line),
                ));
            }
        }

        // Close the innermost function on a bare `end`.
        if (t == "end" || t.starts_with("end)") || t.starts_with("end,") || t == "end;")
            && !stack.is_empty()
        {
            stack.pop();
        }
    }
    out
}

#[test]
fn no_vararg_outside_a_vararg_function() {
    let files = corpus();
    if files.is_empty() {
        eprintln!("SKIP: bytecode corpus not present");
        return;
    }

    let mut offenders: Vec<(String, usize, String, usize)> = Vec::new();
    let mut scanned = 0usize;

    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Ok(src) = luau_core::decompile(&bytes) else { continue };
        scanned += 1;
        let bad = illegal_vararg_uses(&src);
        if !bad.is_empty() {
            let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            offenders.push((name, bad.len(), bad[0].1.clone(), bad[0].0));
        }
    }

    offenders.sort_by(|a, b| b.1.cmp(&a.1));

    eprintln!();
    eprintln!("  scanned {} chunks", scanned);
    eprintln!("  chunks emitting `...` outside a vararg function: {}", offenders.len());
    for (name, n, decl, line) in offenders.iter().take(12) {
        let short: String = name.chars().take(52).collect();
        eprintln!("    {:>2} uses  {}", n, short);
        eprintln!("             first at line {} in: {}", line, decl);
    }

    assert!(
        offenders.is_empty(),
        "{} chunk(s) emit `...` inside a function that does not declare it. \
         luau-compile rejects this with \"Cannot use '...' outside of a vararg \
         function\", so the output cannot run. GETVARARGS must not produce \
         Expr::Varargs when the enclosing proto's is_vararg flag is false.",
        offenders.len()
    );
}
