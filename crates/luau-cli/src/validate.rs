//! Luau source validation.
//!
//! Strategy, in order of preference:
//!   1. External `luau` / `luau-analyze` / `luau-check` on PATH — full parser.
//!   2. Built-in lexical sanity check — balances brackets/keywords, counts
//!      line numbers for unterminated strings/comments.
//!
//! The built-in checker is intentionally a cheap linter — it won't catch
//! every malformed expression, but it will flag the common failure modes
//! produced by a bad decompile (unbalanced `end`, stray `)` , truncated
//! strings, etc.) so it's useful as a CI smoke test when `luau` isn't
//! installed. No new dependencies added.

use crate::ansi::Colors;
use std::path::PathBuf;
use std::process::Command;

/// Outcome of validating a single file.
#[derive(Debug)]
pub struct ValidationResult {
    pub ok: bool,
    pub checker: &'static str,
    /// List of human-readable diagnostic strings (empty on OK).
    pub errors: Vec<String>,
}

/// Primary entry point.
///
/// If `force_builtin` is true, skips the external luau probe.
pub fn validate_source(src: &str, force_builtin: bool) -> ValidationResult {
    if !force_builtin {
        if let Some(result) = run_external(src) {
            return result;
        }
    }
    run_builtin(src)
}

/// Probe $PATH for any of our candidate external checkers and invoke one.
/// Returns None if no suitable binary is available.
fn run_external(src: &str) -> Option<ValidationResult> {
    // `luau-analyze` ships with Luau releases and does full static analysis.
    // `luau -c` does a parse-only check. We try the most specific first.
    let candidates: &[(&str, &[&str], &str)] = &[
        ("luau-analyze", &["-"], "luau-analyze"),
        ("luau", &["--check", "-"], "luau --check"),
        ("luau", &["-c", "-"], "luau -c"),
    ];

    for (bin, args, label) in candidates {
        if !which(bin) {
            continue;
        }
        let mut cmd = Command::new(bin);
        cmd.args(*args);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let Ok(mut child) = cmd.spawn() else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(src.as_bytes());
        }
        let Ok(output) = child.wait_with_output() else {
            continue;
        };

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let ok = output.status.success();
        let mut errors = Vec::new();
        for line in stderr.lines().chain(stdout.lines()) {
            let l = line.trim();
            if !l.is_empty() {
                errors.push(l.to_string());
            }
        }
        if ok {
            errors.clear();
        }
        // Turn a 'static label
        let checker: &'static str = match *label {
            "luau-analyze" => "luau-analyze",
            "luau --check" => "luau --check",
            "luau -c" => "luau -c",
            _ => "external",
        };
        return Some(ValidationResult { ok, checker, errors });
    }

    None
}

fn which(bin: &str) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        for ext in exe_exts() {
            let mut p = PathBuf::from(&dir);
            if ext.is_empty() {
                p.push(bin);
            } else {
                p.push(format!("{}.{}", bin, ext));
            }
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(windows)]
fn exe_exts() -> &'static [&'static str] {
    &["exe", "cmd", "bat", ""]
}

#[cfg(not(windows))]
fn exe_exts() -> &'static [&'static str] {
    &[""]
}

/// Built-in lexical sanity checker.
///
/// Not a full parser. Catches:
///   * Unbalanced `(` `[` `{` / `)` `]` `}`
///   * Unbalanced block keywords `do`/`then`/`function`/`repeat`/`if` vs `end`/`until`
///   * Unterminated strings (including `[[ ... ]]`)
///   * Unterminated `--[[ ... ]]` block comments
pub fn run_builtin(src: &str) -> ValidationResult {
    let mut errors = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut paren = 0i32;
    let mut brack = 0i32;
    let mut brace = 0i32;
    // Block openers stacked as (kind, line).
    let mut blocks: Vec<(&'static str, usize)> = Vec::new();

    // Track paren lines to pinpoint mismatches.
    let mut paren_lines: Vec<usize> = Vec::new();
    let mut brack_lines: Vec<usize> = Vec::new();
    let mut brace_lines: Vec<usize> = Vec::new();

    while i < bytes.len() {
        let b = bytes[i];

        // Newlines
        if b == b'\n' {
            line += 1;
            i += 1;
            continue;
        }

        // Comments
        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            // check for long comment --[[ or --[=[
            if let Some((closer, advance_lines, consumed)) = match_long_bracket_open(&bytes[i + 2..]) {
                // long comment; find matching closer
                let start_line = line;
                let mut j = i + 2 + consumed;
                let mut found = false;
                while j + closer.len() <= bytes.len() {
                    if bytes[j] == b'\n' {
                        line += 1;
                    }
                    if bytes[j..].starts_with(closer.as_bytes()) {
                        j += closer.len();
                        found = true;
                        break;
                    }
                    j += 1;
                }
                if !found {
                    errors.push(format!(
                        "line {}: unterminated long comment (looking for {:?})",
                        start_line, closer
                    ));
                    // stop — nothing sane past this
                    return ValidationResult {
                        ok: false,
                        checker: "builtin",
                        errors,
                    };
                }
                let _ = advance_lines;
                i = j;
                continue;
            }
            // line comment — skip to newline
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Long strings [[ ... ]] or [=[ ... ]=]
        if b == b'[' {
            if let Some((closer, _, consumed)) = match_long_bracket_open(&bytes[i..]) {
                let start_line = line;
                let mut j = i + consumed;
                let mut found = false;
                while j + closer.len() <= bytes.len() {
                    if bytes[j] == b'\n' {
                        line += 1;
                    }
                    if bytes[j..].starts_with(closer.as_bytes()) {
                        j += closer.len();
                        found = true;
                        break;
                    }
                    j += 1;
                }
                if !found {
                    errors.push(format!(
                        "line {}: unterminated long string (looking for {:?})",
                        start_line, closer
                    ));
                    return ValidationResult {
                        ok: false,
                        checker: "builtin",
                        errors,
                    };
                }
                i = j;
                continue;
            }
            brack += 1;
            brack_lines.push(line);
            i += 1;
            continue;
        }

        // Normal strings
        if b == b'"' || b == b'\'' {
            let quote = b;
            let start_line = line;
            i += 1;
            let mut terminated = false;
            while i < bytes.len() {
                let c = bytes[i];
                if c == b'\\' && i + 1 < bytes.len() {
                    // escape — skip next char
                    if bytes[i + 1] == b'\n' {
                        line += 1;
                    }
                    i += 2;
                    continue;
                }
                if c == b'\n' {
                    // unescaped newline in short string — error
                    errors.push(format!(
                        "line {}: unterminated string (got newline before closing {})",
                        start_line, quote as char
                    ));
                    return ValidationResult {
                        ok: false,
                        checker: "builtin",
                        errors,
                    };
                }
                if c == quote {
                    i += 1;
                    terminated = true;
                    break;
                }
                i += 1;
            }
            if !terminated {
                errors.push(format!(
                    "line {}: unterminated string at EOF (expected {})",
                    start_line, quote as char
                ));
                return ValidationResult {
                    ok: false,
                    checker: "builtin",
                    errors,
                };
            }
            continue;
        }

        // Identifiers / keywords
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &src[start..i];
            match word {
                "do" => blocks.push(("do", line)),
                "then" => blocks.push(("then", line)),
                "function" => blocks.push(("function", line)),
                "repeat" => blocks.push(("repeat", line)),
                "if" => {
                    // "if" opens a block; `then` pushes another — we collapse: we only push
                    // on `then`, so ignore `if` here.
                }
                "end" => match blocks.pop() {
                    Some(_) => {}
                    None => errors.push(format!("line {}: unexpected `end`", line)),
                },
                "until" => match blocks.last() {
                    Some(("repeat", _)) => {
                        blocks.pop();
                    }
                    _ => errors.push(format!("line {}: unexpected `until`", line)),
                },
                "elseif" | "else" => {
                    // Fine inside `then` block.
                }
                _ => {}
            }
            continue;
        }

        // Bracket accounting
        match b {
            b'(' => {
                paren += 1;
                paren_lines.push(line);
            }
            b')' => {
                paren -= 1;
                paren_lines.pop();
                if paren < 0 {
                    errors.push(format!("line {}: unmatched `)`", line));
                    paren = 0;
                }
            }
            b'{' => {
                brace += 1;
                brace_lines.push(line);
            }
            b'}' => {
                brace -= 1;
                brace_lines.pop();
                if brace < 0 {
                    errors.push(format!("line {}: unmatched `}}`", line));
                    brace = 0;
                }
            }
            b']' => {
                brack -= 1;
                brack_lines.pop();
                if brack < 0 {
                    errors.push(format!("line {}: unmatched `]`", line));
                    brack = 0;
                }
            }
            _ => {}
        }

        i += 1;
    }

    if paren > 0 {
        errors.push(format!(
            "EOF: {} unclosed `(` (first opened line {})",
            paren,
            paren_lines.first().copied().unwrap_or(0)
        ));
    }
    if brace > 0 {
        errors.push(format!(
            "EOF: {} unclosed `{{` (first opened line {})",
            brace,
            brace_lines.first().copied().unwrap_or(0)
        ));
    }
    if brack > 0 {
        errors.push(format!(
            "EOF: {} unclosed `[` (first opened line {})",
            brack,
            brack_lines.first().copied().unwrap_or(0)
        ));
    }
    for (kind, ln) in &blocks {
        errors.push(format!("EOF: missing `end` for `{}` opened on line {}", kind, ln));
    }

    ValidationResult {
        ok: errors.is_empty(),
        checker: "builtin",
        errors,
    }
}

/// Match `[[`, `[=[`, `[==[`, ... — returning the matching closer string,
/// number of lines consumed by the opener (always 0 here, one-line opener),
/// and the byte count the opener occupies.
fn match_long_bracket_open(rest: &[u8]) -> Option<(String, usize, usize)> {
    if rest.is_empty() || rest[0] != b'[' {
        return None;
    }
    let mut eqs = 0usize;
    let mut k = 1usize;
    while k < rest.len() && rest[k] == b'=' {
        eqs += 1;
        k += 1;
    }
    if k >= rest.len() || rest[k] != b'[' {
        return None;
    }
    let consumed = k + 1;
    let closer = format!("]{}]", "=".repeat(eqs));
    Some((closer, 0, consumed))
}

/// Print a formatted report and return an exit code.
pub fn report(label: &str, r: &ValidationResult, c: &Colors) -> i32 {
    if r.ok {
        println!(
            "{}OK{}  {}  ({}{}{})",
            c.green, c.reset, label, c.dim, r.checker, c.reset
        );
        0
    } else {
        println!(
            "{}FAIL{}  {}  ({}{}{})",
            c.red, c.reset, label, c.dim, r.checker, c.reset
        );
        for e in &r.errors {
            println!("  {}•{} {}", c.yellow, c.reset, e);
        }
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_accepts_trivial() {
        let r = run_builtin("local x = 1\nreturn x\n");
        assert!(r.ok, "trivial snippet should validate: {:?}", r.errors);
    }

    #[test]
    fn builtin_accepts_balanced_blocks() {
        let src = r#"
            local function f(a, b)
                if a > b then
                    return a
                else
                    return b
                end
            end
            for i = 1, 10 do
                print(i)
            end
            return f(1, 2)
        "#;
        let r = run_builtin(src);
        assert!(r.ok, "balanced source should validate: {:?}", r.errors);
    }

    #[test]
    fn builtin_flags_missing_end() {
        let src = "local function f()\n  return 1\n";
        let r = run_builtin(src);
        assert!(!r.ok);
        assert!(
            r.errors.iter().any(|e| e.contains("missing `end`")),
            "expected missing end diagnostic, got {:?}",
            r.errors
        );
    }

    #[test]
    fn builtin_flags_unbalanced_paren() {
        let src = "local x = (1 + 2\nreturn x\n";
        let r = run_builtin(src);
        assert!(!r.ok);
        assert!(
            r.errors.iter().any(|e| e.contains("unclosed `(`")),
            "expected unclosed paren, got {:?}",
            r.errors
        );
    }

    #[test]
    fn builtin_flags_stray_close_brace() {
        let src = "local x = 1}\nreturn x\n";
        let r = run_builtin(src);
        assert!(!r.ok);
        assert!(
            r.errors.iter().any(|e| e.contains("unmatched `}`")),
            "expected unmatched brace, got {:?}",
            r.errors
        );
    }

    #[test]
    fn builtin_flags_unterminated_string() {
        let src = "local s = \"hello\nreturn s\n";
        let r = run_builtin(src);
        assert!(!r.ok);
        assert!(
            r.errors.iter().any(|e| e.contains("unterminated string")),
            "expected string diagnostic, got {:?}",
            r.errors
        );
    }

    #[test]
    fn builtin_accepts_long_string() {
        let src = "local s = [[hello\nworld]]\nreturn s\n";
        let r = run_builtin(src);
        assert!(r.ok, "long string should validate: {:?}", r.errors);
    }

    #[test]
    fn builtin_accepts_long_string_with_equals() {
        let src = "local s = [==[he]]llo]==]\nreturn s\n";
        let r = run_builtin(src);
        assert!(r.ok, "level-2 long string should validate: {:?}", r.errors);
    }

    #[test]
    fn builtin_flags_unterminated_long_string() {
        let src = "local s = [[hello\nworld\n";
        let r = run_builtin(src);
        assert!(!r.ok);
        assert!(
            r.errors.iter().any(|e| e.contains("unterminated long string")),
            "expected long string diagnostic, got {:?}",
            r.errors
        );
    }

    #[test]
    fn builtin_accepts_line_comment() {
        let src = "-- this is a comment\nlocal x = 1\nreturn x\n";
        let r = run_builtin(src);
        assert!(r.ok, "line comment should validate: {:?}", r.errors);
    }

    #[test]
    fn builtin_accepts_block_comment() {
        let src = "--[[ block\ncomment ]]\nlocal x = 1\nreturn x\n";
        let r = run_builtin(src);
        assert!(r.ok, "block comment should validate: {:?}", r.errors);
    }

    #[test]
    fn builtin_flags_unterminated_block_comment() {
        let src = "--[[ block\ncomment\n";
        let r = run_builtin(src);
        assert!(!r.ok);
        assert!(
            r.errors.iter().any(|e| e.contains("unterminated long comment")),
            "expected block comment diagnostic, got {:?}",
            r.errors
        );
    }

    #[test]
    fn builtin_handles_repeat_until() {
        let src = "repeat\n  x = x + 1\nuntil x > 10\n";
        let r = run_builtin(src);
        assert!(r.ok, "repeat/until should validate: {:?}", r.errors);
    }

    #[test]
    fn builtin_flags_repeat_without_until() {
        let src = "repeat\n  x = x + 1\n";
        let r = run_builtin(src);
        assert!(!r.ok);
        assert!(
            r.errors.iter().any(|e| e.contains("missing `end` for `repeat`")),
            "expected missing until/end, got {:?}",
            r.errors
        );
    }

    #[test]
    fn validate_source_delegates_to_builtin_when_forced() {
        let r = validate_source("local x = 1", true);
        assert_eq!(r.checker, "builtin");
        assert!(r.ok);
    }
}
