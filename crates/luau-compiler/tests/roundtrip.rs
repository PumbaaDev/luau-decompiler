//! Phase 1 integration tests: compile each fixture, run the protected output
//! through the bundled `luau` interpreter, and verify the stdout matches the
//! same fixture run directly. Requires `tools/luau/luau.exe` (downloaded from
//! the Luau release artifacts).

use std::path::{Path, PathBuf};
use std::process::Command;

use luau_compiler::{protect, ProtectOptions};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // CARGO_MANIFEST_DIR = .../crates/luau-compiler
    p.pop(); // crates
    p.pop(); // workspace root
    p
}

fn luau_binary() -> Option<PathBuf> {
    let p = workspace_root().join("tools").join("luau").join("luau.exe");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

fn run_luau(luau: &Path, script: &Path) -> (String, String, i32) {
    let out = Command::new(luau).arg(script).output().expect("spawn luau");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let code = out.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

fn assert_roundtrip_with(fixture_relative: &str, opts: &ProtectOptions, tag: &str) {
    let Some(luau) = luau_binary() else {
        eprintln!("skip: tools/luau/luau.exe not present");
        return;
    };
    let root = workspace_root();
    let fixture = root
        .join("crates")
        .join("luau-compiler")
        .join("tests")
        .join("fixtures")
        .join(fixture_relative);
    let source = std::fs::read_to_string(&fixture).expect("read fixture");
    let protected = protect(&source, opts).expect("protect");

    let out_dir = std::env::temp_dir().join("luau_compiler_tests");
    let _ = std::fs::create_dir_all(&out_dir);
    let protected_path = out_dir.join(format!("{fixture_relative}.{tag}.lua"));
    std::fs::write(&protected_path, &protected).expect("write protected");

    let (orig_stdout, orig_stderr, orig_code) = run_luau(&luau, &fixture);
    let (prot_stdout, prot_stderr, prot_code) = run_luau(&luau, &protected_path);

    assert_eq!(orig_code, 0, "original failed: stderr={orig_stderr}");
    assert_eq!(
        prot_code, 0,
        "protected ({tag}) failed: stderr={prot_stderr}\n--protected source--\n{protected}\n"
    );
    assert_eq!(
        prot_stdout, orig_stdout,
        "protected ({tag}) stdout differs from original.\n--orig stderr--\n{orig_stderr}\n--prot stderr--\n{prot_stderr}"
    );
}

fn plain_opts() -> ProtectOptions {
    ProtectOptions {
        encrypt_constants: false,
        ..Default::default()
    }
}

fn encrypted_opts() -> ProtectOptions {
    ProtectOptions {
        encrypt_constants: true,
        seed: Some(0xC0FF_EE42),
        ..Default::default()
    }
}

fn full_opts() -> ProtectOptions {
    ProtectOptions {
        encrypt_constants: true,
        inject_junk: true,
        permute_opcodes: true,
        obfuscate_numbers: true,
        encrypt_operands: true,
        lazy_strings: true,
        flatten_control_flow: true,
        seed: Some(0xBADC_0FFE),
        ..Default::default()
    }
}

fn flatten_opts() -> ProtectOptions {
    ProtectOptions {
        encrypt_constants: false,
        flatten_control_flow: true,
        seed: Some(0x8BADF00D),
        ..Default::default()
    }
}

fn permuted_only_opts(seed: u32) -> ProtectOptions {
    ProtectOptions {
        encrypt_constants: false,
        inject_junk: false,
        permute_opcodes: true,
        seed: Some(seed as u64),
        ..Default::default()
    }
}

#[test]
fn roundtrip_hello_plain() {
    assert_roundtrip_with("hello.lua", &plain_opts(), "plain");
}

#[test]
fn roundtrip_hello_encrypted() {
    assert_roundtrip_with("hello.lua", &encrypted_opts(), "enc");
}

#[test]
fn roundtrip_fib_plain() {
    assert_roundtrip_with("fib.lua", &plain_opts(), "plain");
}

#[test]
fn roundtrip_fib_encrypted() {
    assert_roundtrip_with("fib.lua", &encrypted_opts(), "enc");
}

#[test]
fn roundtrip_hello_full() {
    assert_roundtrip_with("hello.lua", &full_opts(), "full");
}

#[test]
fn roundtrip_fib_full() {
    assert_roundtrip_with("fib.lua", &full_opts(), "full");
}

#[test]
fn roundtrip_hello_permuted() {
    assert_roundtrip_with("hello.lua", &permuted_only_opts(424242), "perm");
}

#[test]
fn roundtrip_fib_permuted() {
    assert_roundtrip_with("fib.lua", &permuted_only_opts(424242), "perm");
}

#[test]
fn roundtrip_hello_flattened() {
    assert_roundtrip_with("hello.lua", &flatten_opts(), "flat");
}

#[test]
fn roundtrip_fib_flattened() {
    assert_roundtrip_with("fib.lua", &flatten_opts(), "flat");
}

#[test]
fn roundtrip_rich_flattened() {
    assert_roundtrip_with("rich.lua", &flatten_opts(), "flat");
}

/// Phase 7A — numeric constants must not appear as literals in the output
/// when `obfuscate_numbers` is on. The fixture's `55` and `10` would
/// otherwise be plainly visible in `_C`.
#[test]
fn no_literal_numbers_when_obfuscated() {
    let source = "local x = 55 + 10\nprint(x)\n";
    let opts = ProtectOptions {
        encrypt_constants: false, // make the const table easier to inspect
        obfuscate_numbers: true,
        seed: Some(7),
        ..Default::default()
    };
    let protected = protect(source, &opts).expect("protect");
    let c_line = protected
        .lines()
        .find(|l| l.starts_with("local _C ="))
        .expect("no _C line");
    assert!(
        !c_line.contains(",55,") && !c_line.ends_with(",55}") && !c_line.contains("{55,"),
        "literal 55 leaked: {c_line}"
    );
    assert!(
        c_line.contains("bit32.bxor"),
        "numbers not actually obfuscated: {c_line}"
    );
}

/// Phase 7B — protect with operand encryption on; output must execute and
/// the operand bytes must not match the unprotected build's operand bytes.
#[test]
fn operand_encryption_round_trip() {
    let Some(luau) = luau_binary() else {
        eprintln!("skip: tools/luau/luau.exe not present");
        return;
    };
    let opts = ProtectOptions {
        encrypt_constants: false,
        encrypt_operands: true,
        seed: Some(0xABCDEF),
        ..Default::default()
    };
    let plain = ProtectOptions { seed: Some(0xABCDEF), encrypt_constants: false, ..Default::default() };
    let src = "local s = 0\nfor i = 1, 5 do s = s + i end\nprint(s)\n";
    let prot_with = protect(src, &opts).expect("with");
    let prot_without = protect(src, &plain).expect("without");
    assert_ne!(prot_with, prot_without, "operand encryption produced identical output");

    let out_dir = std::env::temp_dir().join("luau_compiler_tests");
    let _ = std::fs::create_dir_all(&out_dir);
    let p = out_dir.join("p7b.lua");
    std::fs::write(&p, &prot_with).expect("write");
    let (stdout, stderr, code) = run_luau(&luau, &p);
    assert_eq!(code, 0, "operand-encrypted script failed: {stderr}");
    assert!(stdout.contains("15"), "expected '15' (sum 1..5), got {stdout:?}");
}

/// Phase 7C — lazy-encrypted strings must execute correctly and the `_C`
/// table line must contain the `{"..."}` table-wrapped form for at least
/// one entry.
#[test]
fn lazy_strings_round_trip() {
    let Some(luau) = luau_binary() else {
        eprintln!("skip: tools/luau/luau.exe not present");
        return;
    };
    let opts = ProtectOptions {
        encrypt_constants: true,
        lazy_strings: true,
        seed: Some(0xFEED_BEEF),
        ..Default::default()
    };
    let src = "print(\"alpha\", \"bravo\", \"charlie\")\n";
    let protected = protect(src, &opts).expect("protect");
    let c_line = protected
        .lines()
        .find(|l| l.starts_with("local _C ="))
        .expect("no _C line");
    assert!(
        c_line.contains("{\""),
        "lazy strings missing table wrap in _C: {c_line}"
    );

    let out_dir = std::env::temp_dir().join("luau_compiler_tests");
    let _ = std::fs::create_dir_all(&out_dir);
    let p = out_dir.join("p7c.lua");
    std::fs::write(&p, &protected).expect("write");
    let (stdout, stderr, code) = run_luau(&luau, &p);
    assert_eq!(code, 0, "lazy-strings script failed: {stderr}");
    assert!(stdout.contains("alpha"), "expected 'alpha' in stdout, got {stdout:?}");
}

/// Two different seeds with permutation enabled must produce different bytes
/// (Phase 5 contract — each build re-randomizes the opcode IDs).
#[test]
fn permutation_differs_across_seeds() {
    let src = "local x = 0\nfor i = 1, 5 do x = x + i end\nprint(x)\n";
    let a = protect(src, &permuted_only_opts(1)).expect("a");
    let b = protect(src, &permuted_only_opts(2)).expect("b");
    assert_ne!(a, b, "permutation with different seeds produced identical output");
}

/// Flipping a byte in the encrypted bytecode must break execution. This is
/// the tamper-protection contract: the integrity check is implicit in
/// decryption correctness, not a removable conditional.
#[test]
fn tamper_protection_breaks_execution() {
    let Some(luau) = luau_binary() else {
        eprintln!("skip: tools/luau/luau.exe not present");
        return;
    };
    let root = workspace_root();
    let fixture = root
        .join("crates")
        .join("luau-compiler")
        .join("tests")
        .join("fixtures")
        .join("hello.lua");
    let source = std::fs::read_to_string(&fixture).expect("read fixture");
    let protected = protect(&source, &encrypted_opts()).expect("protect");

    // Find the first `\NNN` escape inside the _P bytecode blob and bump it
    // by 1. That's a single-byte flip in the ciphertext.
    let p_start = protected.find("_P = {{c=\"").expect("no _P found") + "_P = {{c=\"".len();
    let tail = &protected[p_start..];
    let esc_pos_rel = tail
        .find('\\')
        .expect("no escape in bytecode (printable-only byte sequence?)");
    let esc_pos = p_start + esc_pos_rel;
    let digits = &protected[esc_pos + 1..esc_pos + 4];
    let val: u8 = digits.parse().expect("3-digit escape");
    let bumped = val.wrapping_add(7);
    let mut tampered = protected.clone();
    tampered.replace_range(
        esc_pos + 1..esc_pos + 4,
        &format!("{bumped:03}"),
    );

    let out_dir = std::env::temp_dir().join("luau_compiler_tests");
    let _ = std::fs::create_dir_all(&out_dir);
    let path = out_dir.join("hello.tampered.lua");
    std::fs::write(&path, &tampered).expect("write tampered");

    let (_stdout, _stderr, code) = run_luau(&luau, &path);
    assert_ne!(
        code, 0,
        "tampered protected script should NOT exit 0 (integrity broken)"
    );
}

/// Decompile-resistance: the protected script must NOT contain the original
/// string literals in plaintext. An attacker reading the Lua source cannot
/// recover "print", "sum =", "ok", or "fail" without decrypting first.
#[test]
fn decompile_resistance_no_plaintext_strings() {
    let source = std::fs::read_to_string(
        workspace_root()
            .join("crates")
            .join("luau-compiler")
            .join("tests")
            .join("fixtures")
            .join("hello.lua"),
    )
    .expect("read fixture");

    let opts = ProtectOptions {
        encrypt_constants: true,
        inject_junk: true,
        permute_opcodes: true,
        seed: Some(0xDEAD_C0DE),
        ..Default::default()
    };
    let protected = protect(&source, &opts).expect("protect");

    // These are the exact string literals that appear in hello.lua's constant pool.
    // After encryption they become escaped bytes; none of these forms should be
    // findable as a bare string in the emitted Lua source.
    let secret_strings = ["\"print\"", "\"sum =\"", "\"ok\"", "\"fail\""];
    for s in secret_strings {
        assert!(
            !protected.contains(s),
            "protected script leaks plaintext literal: {s}"
        );
    }
}
