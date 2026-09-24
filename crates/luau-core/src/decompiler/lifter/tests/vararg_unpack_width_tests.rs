//! GETVARARGS must name EVERY register it writes, not the first ten.
//!
//! Found on PetSim99 `ExclusiveEggCmds_GargantuanHatch` proto 23
//! (`play(arg1, arg2, arg3, ...)`, frame 80): PC 42 is
//! `GETVARARGS R6 count=14`. A flat `count.min(10)` in the handler named only
//! R6..R15; R16..R19 were never written by the lifter, so every later read of
//! them (all inside closures that CAPTURE them) became an unbound
//! `v16`/`v17`/`v18`/`v19` that evaluates to nil.
//!
//! The instruction writes R(A)..R(A+B-2) and the proto declares its frame, so
//! the bytecode bounds the width exactly.

use crate::decompiler::lifter::opcode_handlers::vararg_unpack_width;
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_RETURN: u8 = 22;
const OP_GETVARARGS: u8 = 64;
const OP_PREPVARARGS: u8 = 65;

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

/// ```text
/// 0: PREPVARARGS nfixed=0
/// 1: GETVARARGS  R0 count=14      ; local r0..r13 = ...
/// 2: RETURN      R10 4 values     ; return r10, r11, r12, r13
/// ```
fn make_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_PREPVARARGS, 0, 0, 0),
        insn_abc(OP_GETVARARGS, 0, 15, 0),
        insn_abc(OP_RETURN, 10, 5, 0),
    ];
    let proto = Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: true,
        flags: 0,
        typeinfo: None,
        code,
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("vararg_unpack_width".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    }
}

#[test]
fn width_is_exact_when_the_count_fits_the_frame() {
    assert_eq!(vararg_unpack_width(6, 14, 80), 14);
    assert_eq!(vararg_unpack_width(0, 16, 16), 16);
    // A count that cannot fit the declared frame is a misdecode: keep the
    // old conservative cap rather than naming phantom registers.
    assert_eq!(vararg_unpack_width(10, 200, 16), 10);
}

#[test]
fn getvarargs_names_all_fourteen_results() {
    let chunk = make_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let decl = out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("local ") && l.ends_with("= ..."))
        .unwrap_or_else(|| panic!("no `local ... = ...` destructure;\n{}", out));
    let names: Vec<&str> = decl["local ".len()..decl.len() - "= ...".len()]
        .split(',')
        .map(str::trim)
        .collect();
    assert_eq!(names.len(), 14, "GETVARARGS count=14 must name 14 registers;\n{}", out);

    // The returned registers R10..R13 are the 11th..14th results. Each must be
    // one of the destructured names, never an unbound fallback.
    let ret = out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("return "))
        .unwrap_or_else(|| panic!("no return statement;\n{}", out));
    let returned: Vec<&str> = ret["return ".len()..].split(',').map(str::trim).collect();
    assert_eq!(returned.len(), 4, "expected 4 returned values;\n{}", out);
    for (i, r) in returned.iter().enumerate() {
        assert_eq!(
            *r, names[10 + i],
            "result {} returns `{}`, not the destructured name `{}`;\n{}",
            11 + i, r, names[10 + i], out
        );
    }
}
