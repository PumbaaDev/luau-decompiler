//! Regression lock for v10-v14 bytecode container support (added 2026-09-21).
//!
//! The parser implements the Luau v10-v14 proto container (self-framing v12
//! protoSize + resync, v11 feedback-vector section, INTEGER/CLASS_SHAPE/VECTORD
//! constants). Because the lifter models Roblox opcode semantics — which diverge
//! from upstream Luau at opcode >= 84 — v10+ decoding is OPT-IN behind the
//! `LUAU_ALLOW_V10PLUS` env var; by default v10+ is rejected with a clear error
//! rather than emitting plausible-but-wrong code.
//!
//! Fixtures are real v12 chunks emitted by luau-compile with call-feedback OFF
//! (so they use only the shared opcode set <= 83 and decode correctly). This
//! whole file is one #[test] so the process-global env var is never toggled
//! while another test in this binary is reading it.

use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/v10plus");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("reading fixture {}: {}", p.display(), e))
}

#[test]
fn v12_default_rejects_optin_parses_to_eof() {
    let callheavy = fixture("callheavy_v12_fboff.luac");
    let loopf = fixture("loop_v12.luac");
    assert_eq!(callheavy[0], 0x0c, "fixture must be bytecode version 12");
    assert_eq!(loopf[0], 0x0c, "fixture must be bytecode version 12");

    // 1) DEFAULT (no opt-in): v12 must be rejected, and the message must name the
    //    version and the opt-in switch — never a silent garbage decode.
    std::env::remove_var("LUAU_ALLOW_V10PLUS");
    let err = luau_core::parser::parse(&callheavy)
        .expect_err("v12 must be rejected by default, not decoded to garbage");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("12") && msg.contains("LUAU_ALLOW_V10PLUS"),
        "default-reject message must name the version and the opt-in: {msg}"
    );

    // 2) OPT-IN: the container must parse fully. A successful parse implies the
    //    reader consumed every proto and read a valid mainid at exactly the right
    //    offset (a desync would fail before returning Ok), so this is the
    //    parse-to-EOF lock. Assert structural sanity too.
    std::env::set_var("LUAU_ALLOW_V10PLUS", "1");

    let chunk = luau_core::parser::parse(&callheavy)
        .expect("feedback-off v12 must parse when opted in");
    assert_eq!(chunk.version, 12);
    // callheavy.luau => main + `work` function => at least 2 protos.
    assert!(chunk.protos.len() >= 2, "expected >=2 protos, got {}", chunk.protos.len());
    assert!(
        (chunk.main_proto as usize) < chunk.protos.len(),
        "main proto index {} out of range (protos={})",
        chunk.main_proto,
        chunk.protos.len()
    );

    let loop_chunk = luau_core::parser::parse(&loopf).expect("v12 loop chunk must parse when opted in");
    assert_eq!(loop_chunk.version, 12);
    assert!(!loop_chunk.protos.is_empty());
    assert!((loop_chunk.main_proto as usize) < loop_chunk.protos.len());

    // 3) CALL-FEEDBACK normalisation: a v12 chunk compiled with call feedback ON
    //    uses CALLFB (opcode 87 = CALL + feedback aux). The parser must rewrite
    //    every CALLFB to CALL (21) so the Roblox-canonical lifter does not read 87
    //    as BNOT and desync into garbage. Assert no opcode 87 survives in any
    //    proto's code (the feedback aux becomes a NOP, opcode 0).
    let fbon = fixture("callheavy_v12_fbon.luac");
    assert_eq!(fbon[0], 0x0c, "fixture must be bytecode version 12");
    let fbon_chunk = luau_core::parser::parse(&fbon).expect("feedback-on v12 must parse when opted in");
    assert_eq!(fbon_chunk.version, 12);
    let leftover_callfb: usize = fbon_chunk
        .protos
        .iter()
        .flat_map(|p| p.code.iter())
        .filter(|w| (*w & 0xFF) as u8 == 87)
        .count();
    assert_eq!(
        leftover_callfb, 0,
        "CALLFB (opcode 87) must be normalised to CALL; {leftover_callfb} left"
    );
    // and it must contain at least one plain CALL (the calls it normalised)
    let call_count: usize = fbon_chunk
        .protos
        .iter()
        .flat_map(|p| p.code.iter())
        .filter(|w| (*w & 0xFF) as u8 == 21)
        .count();
    assert!(call_count > 0, "expected normalised CALL instructions, found none");

    std::env::remove_var("LUAU_ALLOW_V10PLUS");
}
