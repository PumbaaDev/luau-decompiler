//! GETGLOBAL / SETGLOBAL name resolution: the name is K[AUX], never K[D].
//!
//! Canonical Luau encoding (`Bytecode.h`, `lvmexecute.cpp` LOP_GETGLOBAL):
//! `A, C = predicted hash slot, AUX = constant index`; the VM reads the name
//! as `VM_KV(aux)`. B is 0, so the "D field" is `C << 8` where
//! `C = luaS_hash(name) & 0xFF`. It is a hash byte, not an index.
//!
//! Bytecode exhibits (raw words from `examples/dump_chunk`):
//!   - Animate main, pc 447: `0x00002318` SETGLOBAL A=35 C=0, AUX=0x94=148,
//!     K[148]="onSwimming", K[0]="script". Old output: `script = onSwimming`.
//!   - StickerTypes p14, pc 6823: `0x05000d35` GETGLOBAL A=13 C=5 (D=1280),
//!     AUX=0x912=2322, K[2322]="tostring", K[1280]="+0.5% Pollen". Old
//!     output: `("+0.5% Pollen")(v.ID)`.
//!   - RootCamera p5/p6/p7: SETGLOBAL C=1 (D=256), K[AUX]="IsAToolEquipped",
//!     chunk.strings[256]="MenuOpened". Old output: `MenuOpened = true`.
//!
//! These tests pin the three sub-mechanisms (D=0 → K[0]; D → K[D] in a large
//! proto; D → chunk.strings[D]) with canonical encodings, and the nearby
//! shapes that must NOT change (coincident D==AUX; the legacy no-AUX
//! fallback a hand-built test stream relies on).

use super::super::resolve_global_name;
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_LOADN: u8 = 4;
const OP_GETGLOBAL: u8 = 7;
const OP_SETGLOBAL: u8 = 8;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn proto(code: Vec<u32>, constants: Vec<Constant>) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: true,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: None,
        line_info: None,
        debug_info: None,
    }
}

fn chunk(p: Proto, strings: Vec<String>) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings,
        protos: vec![p],
        main_proto: 0,
    }
}

fn s(x: &str) -> Constant {
    Constant::String(x.to_string())
}

fn decompile(c: &Chunk) -> String {
    let mut ctx = DecompileContext::new(c);
    decompile_proto(&mut ctx, &c.protos[0], 0, 0)
}

/// Sub-mechanism A (dominant): C = 0 → D = 0 → K[0] is some unrelated string.
/// Canonical Animate shape: `onSwimming = <value>` must NOT become
/// `script = <value>`.
#[test]
fn setglobal_hash_slot_zero_names_k_aux_not_k0() {
    let code = vec![
        insn_ad(OP_LOADN, 0, 7),
        insn_abc(OP_SETGLOBAL, 0, 0, 0), // C = 0 → D = 0
        1,                               // AUX = 1 → K[1] = "onSwimming"
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let c = chunk(proto(code, vec![s("script"), s("onSwimming")]), vec![]);
    let out = decompile(&c);
    assert!(out.contains("onSwimming = 7"), "SETGLOBAL must write K[AUX]:\n{out}");
    assert!(!out.contains("script ="), "K[0] must not be read as the name:\n{out}");
}

/// Sub-mechanism C: a proto with more than `C << 8` constants, where K[D] is a
/// non-identifier string. Old output called that string literal.
#[test]
fn getglobal_large_proto_names_k_aux_not_k_d() {
    let mut consts: Vec<Constant> = (0..1300).map(|i| s(&format!("filler {i}"))).collect();
    consts[1280] = s("+0.5% Pollen");
    consts.push(s("tostring")); // index 1300
    let code = vec![
        insn_abc(OP_GETGLOBAL, 0, 0, 5), // C = 5 → D = 1280
        1300,                            // AUX
        insn_ad(OP_LOADN, 1, 3),
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let c = chunk(proto(code, consts), vec![]);
    let out = decompile(&c);
    assert!(out.contains("tostring(3)"), "GETGLOBAL must read K[AUX]:\n{out}");
    assert!(!out.contains("Pollen"), "K[D] must not be read as the name:\n{out}");
}

/// Sub-mechanism B: small proto (K[D] misses) but a chunk string table larger
/// than `C << 8`, so the old strings[D] tier fired.
#[test]
fn setglobal_large_string_table_names_k_aux_not_strings_d() {
    let mut strings: Vec<String> = (0..300).map(|i| format!("str{i}")).collect();
    strings[256] = "MenuOpened".to_string();
    let code = vec![
        insn_ad(OP_LOADN, 0, 1),
        insn_abc(OP_SETGLOBAL, 0, 0, 1), // C = 1 → D = 256
        2,                               // AUX = 2 → "IsAToolEquipped"
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let c = chunk(proto(code, vec![s("a"), s("b"), s("IsAToolEquipped")]), strings);
    let out = decompile(&c);
    assert!(out.contains("IsAToolEquipped = 1"), "SETGLOBAL must write K[AUX]:\n{out}");
    assert!(!out.contains("MenuOpened"), "strings[D] must not be read as the name:\n{out}");
}

/// Must-not-change: D == AUX with K[D] the name (the coincident shape that
/// many existing hand-built tests use) resolves to the same name.
#[test]
fn coincident_d_equals_aux_unchanged() {
    let p = proto(vec![], vec![s("x"), s("print")]);
    assert_eq!(resolve_global_name(&p, &[], 1, Some(1)).as_deref(), Some("print"));
}

/// Must-not-change: a non-canonical stream with no usable AUX (K[AUX] out of
/// range, e.g. the next instruction word was read as AUX) still resolves
/// through the legacy K[D] tier. `pinned_arm_write_tests` relies on this.
#[test]
fn non_string_aux_falls_back_to_legacy_d_tier() {
    let p = proto(vec![], vec![s("SomeGlobal")]);
    let next_insn_as_aux = insn_abc(6, 3, 1, 0); // a MOVE word, huge as an index
    assert_eq!(
        resolve_global_name(&p, &[], 0, Some(next_insn_as_aux)).as_deref(),
        Some("SomeGlobal")
    );
    assert_eq!(resolve_global_name(&p, &[], 0, None).as_deref(), Some("SomeGlobal"));
}

/// Must-not-change: K[AUX] that is not a String (e.g. a Number) does not win
/// the canonical tier; resolution falls through to the legacy chain.
#[test]
fn number_constant_at_aux_is_not_a_name() {
    let p = proto(vec![], vec![s("Legacy"), Constant::Number(4.0)]);
    assert_eq!(resolve_global_name(&p, &[], 0, Some(1)).as_deref(), Some("Legacy"));
}
