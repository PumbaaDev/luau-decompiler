//! Phase 6: source-level decompile-resistance harness.
//!
//! The protected `.lua` IS the deployment artifact for Roblox executor
//! scripts. The question that matters is: can an attacker reading that file
//! recover meaningful identifiers, string literals, or control flow from the
//! original source? This harness extracts distinctive tokens from the input
//! and asserts that none of them appear verbatim in the protected output
//! when full obfuscation is enabled.

use std::collections::BTreeSet;
use std::path::PathBuf;

use luau_compiler::{protect, ProtectOptions};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

/// Strip `--`-style line comments (we don't care about block comments —
/// fixtures don't use them). The token extractor sees only real code.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("--") {
            Some(pos) => &l[..pos],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pull distinctive identifiers + string-literal contents out of a Luau source
/// snippet. We're not trying to be a real parser — we just want long enough
/// tokens that random collisions in the protected output are unlikely.
fn distinctive_tokens(src: &str) -> BTreeSet<String> {
    let src = strip_line_comments(src);
    let src = src.as_str();
    let mut out = BTreeSet::new();

    // Identifiers: any alphanumeric-or-underscore run of length >= 5 that
    // starts with a letter and isn't a reserved word or trivial number.
    let reserved = [
        "local", "function", "return", "true", "false", "while", "for", "do",
        "end", "then", "else", "elseif", "break", "continue", "nil", "and",
        "or", "not", "print", "ipairs", "pairs", "next", "type", "string",
        "table", "math",
    ];
    let mut cur = String::new();
    let push_if_distinctive = |s: &str, out: &mut BTreeSet<String>| {
        if s.len() >= 5
            && s.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false)
            && !reserved.contains(&s)
        {
            out.insert(s.to_string());
        }
    };
    for c in src.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            push_if_distinctive(&cur, &mut out);
            cur.clear();
        }
    }
    push_if_distinctive(&cur, &mut out);

    // String literals: dirty extractor for "..." and '...' (no escape
    // handling needed for our fixtures).
    for delim in &['"', '\''] {
        let bytes = src.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == *delim as u8 {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != *delim as u8 {
                    if bytes[j] == b'\\' {
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                if j <= bytes.len() && j > start {
                    let inner = &src[start..j.min(src.len())];
                    if inner.len() >= 4 {
                        out.insert(inner.to_string());
                    }
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
    }

    out
}

/// Tokens that appear in the protected output of an empty source. These are
/// part of the runtime dispatcher's vocabulary (`locals`, `stack`, `upvals`,
/// the word `value` inside an error message, etc.) and are NOT user-specific
/// leakage — they appear in every build regardless of input.
fn runtime_vocabulary() -> BTreeSet<String> {
    let dummy = protect(
        "return 0\n",
        &ProtectOptions {
            encrypt_constants: true,
            inject_junk: true,
            permute_opcodes: true,
            seed: Some(0xC0FF_EE),
            ..Default::default()
        },
    )
    .expect("protect empty");
    distinctive_tokens(&dummy)
}

fn run_resistance(fixture_relative: &str) {
    let root = workspace_root();
    let fixture = root
        .join("crates")
        .join("luau-compiler")
        .join("tests")
        .join("fixtures")
        .join(fixture_relative);
    let source = std::fs::read_to_string(&fixture).expect("read fixture");

    // User-specific tokens = tokens present in the source MINUS tokens that
    // the runtime dispatcher would emit on its own. Those are the only
    // tokens whose presence in the protected output would indicate a real
    // leak of the input.
    let runtime_vocab = runtime_vocabulary();
    let source_tokens = distinctive_tokens(&source);
    let user_specific: BTreeSet<String> = source_tokens
        .difference(&runtime_vocab)
        .cloned()
        .collect();
    assert!(
        !user_specific.is_empty(),
        "no user-specific tokens — test would be vacuous"
    );

    let opts = ProtectOptions {
        encrypt_constants: true,
        inject_junk: true,
        permute_opcodes: true,
        seed: Some(0xFACE_FEED),
        ..Default::default()
    };
    let protected = protect(&source, &opts).expect("protect");

    let mut leaks: Vec<String> = Vec::new();
    for tok in &user_specific {
        if protected.contains(tok) {
            leaks.push(tok.clone());
        }
    }
    assert!(
        leaks.is_empty(),
        "protected output leaked {} user-specific tokens: {:?}",
        leaks.len(),
        leaks,
    );
}

#[test]
fn no_leakage_hello() {
    run_resistance("hello.lua");
}

#[test]
fn no_leakage_fib() {
    run_resistance("fib.lua");
}

#[test]
fn no_leakage_rich() {
    run_resistance("rich.lua");
}
