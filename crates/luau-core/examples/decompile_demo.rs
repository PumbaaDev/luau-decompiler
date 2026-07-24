//! End-to-end decompiler demo — no external toolchain required.
//!
//! The `luau-decompiler` CLI consumes *standard* Luau bytecode (v3–8), such as
//! the output of the official `luau-compile --binary` or a Luau runtime's
//! `string.dump`. To keep this example fully self-contained, we hand-assemble a
//! tiny bytecode chunk in memory instead of shelling out to a compiler, then
//! run it through the same lifter the CLI uses and print the recovered source.
//!
//! Run it with:
//!
//! ```sh
//! cargo run -p luau-core --example decompile_demo
//! ```
//!
//! The chunk below is the compiled form of:
//!
//! ```lua
//! for i = 1, 10 do
//!     local t = { run = function() return i end }
//!     t.run()
//! end
//! ```

use luau_core::decompiler::{decompile_proto, DecompileContext};
use luau_core::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (mirror `LuauOpcode as u8`).
const OP_LOADN: u8 = 4;
const OP_GETUPVAL: u8 = 9;
const OP_GETTABLEKS: u8 = 15;
const OP_SETTABLEKS: u8 = 16;
const OP_NEWCLOSURE: u8 = 19;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_NEWTABLE: u8 = 53;
const OP_FORNPREP: u8 = 56;
const OP_FORNLOOP: u8 = 57;
const OP_CAPTURE: u8 = 70;

/// Encode an A/D-format instruction (op, register A, signed 16-bit D).
fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

/// Encode an A/B/C-format instruction.
fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn main() {
    // Main proto: the `for` loop that builds `{ run = <closure> }` and calls it.
    let main_code = vec![
        insn_ad(OP_LOADN, 0, 10),         // limit = 10
        insn_ad(OP_LOADN, 1, 1),          // step  = 1
        insn_ad(OP_LOADN, 2, 1),          // start = 1
        insn_ad(OP_FORNPREP, 0, 10),      // skip body if range is empty
        insn_abc(OP_NEWTABLE, 3, 0, 0),   // t = {}
        0,                                // NEWTABLE AUX word
        insn_ad(OP_NEWCLOSURE, 4, 0),     // closure = <child proto 0>
        insn_abc(OP_CAPTURE, 1, 2, 0),    // capture loop var `i` as an upvalue
        insn_abc(OP_SETTABLEKS, 4, 3, 0), // t.run = closure   (AUX -> K0 "run")
        0,                                // SETTABLEKS AUX word
        insn_abc(OP_GETTABLEKS, 5, 3, 0), // r5 = t.run        (AUX -> K0 "run")
        0,                                // GETTABLEKS AUX word
        insn_abc(OP_CALL, 5, 1, 1),       // t.run()
        insn_ad(OP_FORNLOOP, 0, -10),     // loop back to the body
        insn_abc(OP_RETURN, 0, 1, 0),     // return
    ];

    let main_proto = Proto {
        max_stack_size: 32,
        num_params: 0,
        num_upvalues: 4,
        is_vararg: true,
        flags: 0,
        typeinfo: None,
        code: main_code,
        constants: vec![Constant::String("run".to_string())],
        child_protos: vec![1], // references the closure proto below
        line_defined: 1,
        debug_name: Some("main".to_string()),
        line_info: None,
        debug_info: None,
    };

    // Child proto: `function() return i end`, returning its captured upvalue.
    let child_proto = Proto {
        max_stack_size: 2,
        num_params: 0,
        num_upvalues: 1,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code: vec![
            insn_abc(OP_GETUPVAL, 0, 0, 0),
            insn_abc(OP_RETURN, 0, 2, 0),
        ],
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("closure".to_string()),
        line_info: None,
        debug_info: None,
    };

    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["run".to_string()],
        protos: vec![main_proto, child_proto],
        main_proto: 0,
    };

    // Lift the main proto back into Luau source.
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    println!("-- Recovered Luau source:\n");
    println!("{source}");
}
