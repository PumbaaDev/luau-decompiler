//! A fixed-count RETURN of more than 10 values must emit every value.
//!
//! The obfuscated RIVALS VM handlers legitimately `return` 12-13 values
//! (MiscellaneousController proto 136 `RETURN R21..R32`, B=13). The RETURN arm
//! treated `B-1 > 10` as corruption and clamped to ONE value, so the other
//! operands became orphaned dead locals. The bound is now the proto frame.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_LOADN: u8 = 4;
const OP_RETURN: u8 = 22;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

/// `count` LOADNs into R1.., then `RETURN R1 B=count+1`.
fn decompile_return_of(count: u8, return_b: u8, max_stack_size: u8) -> String {
    let mut code: Vec<u32> = (0..count)
        .map(|i| insn_ad(OP_LOADN, 1 + i, 101 + i as i16))
        .collect();
    code.push(insn_abc(OP_RETURN, 1, return_b, 0));
    let proto = Proto {
        max_stack_size,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("wide".to_string()),
        line_info: None,
        debug_info: None,
    };
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    };
    let mut ctx = DecompileContext::new(&chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

#[test]
fn twelve_value_return_emits_all_twelve_values() {
    let out = decompile_return_of(12, 13, 16);
    // Large literals may print in hex, so count the list rather than match text.
    assert_eq!(out.matches(',').count(), 11, "expected 12 return values:
{out}");
    assert!(out.contains("101") && out.contains("111"), "values dropped:
{out}");
}

#[test]
fn ten_value_return_is_unchanged() {
    let out = decompile_return_of(10, 11, 16);
    for v in 101..=110 {
        assert!(out.contains(&v.to_string()), "value {v} was dropped:\n{out}");
    }
}

#[test]
fn return_count_beyond_the_frame_still_falls_back_to_one_value() {
    // B-1 = 12 but the frame only has 8 registers: not a real return window.
    let out = decompile_return_of(12, 13, 8);
    assert!(out.contains("101"), "first value must survive:\n{out}");
    assert!(!out.contains(","), "out-of-frame register must not be read:\n{out}");
}
