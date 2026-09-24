//! Malformed bytecode must come back as `Ok` or `Err`, never as a panic or a
//! process abort.
//!
//! `luau_core` is linked into hosts that decode untrusted uploads (the WASM
//! build, the HTTP server, the `luau-ffi` C ABI). A panic there unwinds into
//! the host, and an allocation failure is worse: Rust ABORTS the process and no
//! `catch_unwind` can stop it. Found by adversarial review of 4918703: setting
//! byte 0 of a live v12 script to 10 made a 431-byte input print
//! `memory allocation of 2305843009213693952 bytes failed` and kill the
//! process through `decompile`, `disassemble` and `info`. The cause was in the
//! parser: a proto with no code but with line info computed
//! `(size_code - 1) >> linegaplog2` in unsigned arithmetic, so the underflow
//! asked for ~2^61 bytes of line table. Every count the parser reads is now
//! bounded by the input that is left before anything is allocated.
//!
//! The reviewer's reproducers are derived from game bytecode, which this
//! repository does not commit, so the regression below rebuilds the same shape
//! by hand; the fuzz pass derives its inputs from the committed upstream
//! fixtures only.
//!
//! One test per concern, and no test here touches the environment: the v10+
//! gate is exercised exactly as a host with a clean environment sees it.

use luau_core::{DecompileOptions, V10PlusMode};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

/// A one-proto chunk whose only proto has NO instructions but DOES carry line
/// info: the shape that underflowed `size_code - 1`.
fn empty_proto_with_line_info(version: u8, linegaplog2: u8) -> Vec<u8> {
    let mut chunk = vec![version];
    if version >= 4 {
        chunk.push(3); // types version
    }
    chunk.push(0); // string table: 0 entries
    if version >= 5 {
        chunk.push(0); // userdata type remap terminator
    }
    chunk.push(1); // 1 proto
    let mut proto = vec![1, 0, 0, 0]; // max stack, params, upvalues, is_vararg
    if version >= 4 {
        proto.extend([0, 0]); // flags, type info size
    }
    proto.extend([0, 0, 0, 0, 0]); // size_code, size_k, size_p, line_defined, debug name
    proto.extend([1, linegaplog2]); // line info present; zero deltas, zero intervals
    proto.push(0); // no debug info
    if version == 11 {
        proto.push(0); // feedback vector: 0 slots
    }
    if version >= 12 {
        chunk.push(proto.len() as u8); // self-framing proto size
    }
    chunk.extend(proto);
    chunk.push(0); // main proto
    chunk
}

/// Every library entry point a host calls on untrusted bytes.
fn entry_points() -> Vec<(&'static str, fn(&[u8]))> {
    fn fallback(b: &[u8]) {
        let opts = DecompileOptions { v10plus: V10PlusMode::DetectorFallback, ..Default::default() };
        let _ = luau_core::decompile_with_options(b, &opts);
    }
    fn off(b: &[u8]) {
        let opts = DecompileOptions { v10plus: V10PlusMode::Off, ..Default::default() };
        let _ = luau_core::decompile_with_options(b, &opts);
    }
    vec![
        ("parse", |b| drop(luau_core::parser::parse(b))),
        ("decompile", |b| drop(luau_core::decompile(b))),
        ("decompile(DetectorFallback)", fallback),
        ("decompile(Off)", off),
        ("decompile_with_opmap(None)", |b| drop(luau_core::decompile_with_opmap(b, None))),
        ("disassemble(debug)", |b| drop(luau_core::disassemble(b, true))),
        ("disassemble", |b| drop(luau_core::disassemble(b, false))),
        ("disassemble_with_opmap(None)", |b| drop(luau_core::disassemble_with_opmap(b, None))),
        ("info", |b| drop(luau_core::info(b))),
    ]
}

/// Run `input` through every entry point; return a description of each panic.
fn panics_for(label: &str, input: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for (name, call) in entry_points() {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| call(input))) {
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            found.push(format!("{label} ({} bytes) -> {name}: {msg}", input.len()));
        }
    }
    found
}

#[test]
fn empty_proto_with_line_info_is_an_error_or_a_decode_not_an_abort() {
    for version in 3u8..=14 {
        for linegaplog2 in [0u8, 5, 24, 31] {
            let bytes = empty_proto_with_line_info(version, linegaplog2);
            let label = format!("v{version} empty proto, linegaplog2={linegaplog2}");
            let panics = panics_for(&label, &bytes);
            assert!(panics.is_empty(), "{}", panics.join("\n"));
        }
    }
    // The v10 shape the reviewer hit goes through the library's v10+ gate,
    // which now parses it cleanly and refuses it: no measured entry covers it.
    let err = luau_core::decompile(&empty_proto_with_line_info(10, 5))
        .expect_err("an unmeasured v10 chunk must be refused");
    assert!(format!("{err:#}").contains("refusing Luau bytecode v10"), "{err:#}");
}

#[test]
fn lying_counts_are_errors_not_huge_allocations() {
    // A v6 chunk whose string count claims 2^32-1 entries in a 7-byte input,
    // and one whose proto count does the same. Each would pre-allocate tens of
    // gigabytes if the count were trusted.
    let huge = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F];
    let mut strings = vec![6u8, 3];
    strings.extend(huge);
    let mut protos = vec![6u8, 3, 0, 0];
    protos.extend(huge);
    for (label, bytes) in [("string count", strings), ("proto count", protos)] {
        let panics = panics_for(label, &bytes);
        assert!(panics.is_empty(), "{}", panics.join("\n"));
        assert!(luau_core::parser::parse(&bytes).is_err(), "{label}: must be an end-of-input error");
    }
}

/// A NAMECALL whose A operand is 255 writes its self slot to register 256.
/// The lifter's operand guard covered A, B and C but not A+1, so it indexed
/// one past the register file and panicked. Found by the gate's fuzz run on a
/// corrupted game script; rebuilt here from the committed upstream fixtures by
/// patching the A byte of every NAMECALL (canonical opcode 20) they contain.
#[test]
fn namecall_into_the_last_register_is_not_an_index_panic() {
    const NAMECALL: u32 = 20;
    let mut patched = 0;
    for (name, bytes) in committed_fixtures() {
        // The v12 fixtures do not parse without the v10+ policy; the v6 probe
        // fixtures carry the method calls.
        let Ok(chunk) = luau_core::parser::parse(&bytes) else { continue };
        for proto in &chunk.protos {
            for (pc, pair) in proto.code.windows(2).enumerate() {
                if pair[0] & 0xFF != NAMECALL {
                    continue;
                }
                let needle: Vec<u8> = pair.iter().flat_map(|w| w.to_le_bytes()).collect();
                let Some(at) = bytes.windows(8).position(|w| w == needle.as_slice()) else {
                    continue;
                };
                let mut b = bytes.clone();
                b[at + 1] = 255; // the A operand
                let panics = panics_for(&format!("{name}: NAMECALL at pc {pc} with A=255"), &b);
                assert!(panics.is_empty(), "{}", panics.join("\n"));
                patched += 1;
            }
        }
    }
    assert!(patched > 0, "the committed fixtures must contain a NAMECALL to patch");
}

fn committed_fixtures() -> Vec<(String, Vec<u8>)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    for dir in ["tests/fixtures/v10plus", "probe/fixtures"] {
        let mut files: Vec<PathBuf> = std::fs::read_dir(root.join(dir))
            .unwrap_or_else(|e| panic!("{dir}: {e}"))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "luac"))
            .collect();
        files.sort();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            out.push((name, std::fs::read(&f).unwrap()));
        }
    }
    assert!(out.len() >= 6, "expected the committed fixtures, found {}", out.len());
    out
}

/// xorshift64*: deterministic, so a failure names an input that reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Truncations, version relabels, byte flips and inflated varint counts of
/// every committed fixture, plus random bytes behind every version byte.
fn fuzz_inputs() -> Vec<(String, Vec<u8>)> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut inputs = Vec::new();
    for (name, bytes) in committed_fixtures() {
        let len = bytes.len();
        let step = (len / 48).max(1);
        let mut cuts: Vec<usize> = (0..len).step_by(step).collect();
        cuts.extend(len.saturating_sub(12)..len);
        for cut in cuts {
            inputs.push((format!("{name} truncated to {cut}"), bytes[..cut].to_vec()));
        }
        for v in (0u8..=15).chain([255]) {
            let mut b = bytes.clone();
            b[0] = v;
            inputs.push((format!("{name} relabelled v{v}"), b));
        }
        for i in 0..160 {
            let mut b = bytes.clone();
            // Keep byte 0 on odd rounds so the real version's layout is fuzzed;
            // relabel to a v10+ header on even rounds, the reviewer's route.
            if i % 2 == 0 {
                b[0] = [10u8, 11, 12, 13, 14][rng.below(5)];
            }
            for _ in 0..1 + rng.below(4) {
                let at = 1 + rng.below(len - 1);
                b[at] = match rng.below(4) {
                    0 => 0xFF,
                    1 => 0x80,
                    2 => 0x00,
                    _ => rng.next() as u8,
                };
            }
            inputs.push((format!("{name} mutation {i}"), b));
        }
        for i in 0..40 {
            // Overwrite a run with a maximal 5-byte varint: some count or
            // length somewhere now claims 2^32-1.
            let mut b = bytes.clone();
            let at = 1 + rng.below(len.saturating_sub(6).max(1));
            let end = (at + 5).min(len);
            b[at..end].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F][..end - at]);
            inputs.push((format!("{name} inflated count {i}"), b));
        }
    }
    for i in 0..60 {
        let n = 1 + rng.below(600);
        let mut b: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        b[0] = 3 + (i % 12) as u8;
        inputs.push((format!("random bytes {i} (v{})", b[0]), b));
    }
    inputs
}

#[test]
fn fuzzed_fixtures_never_panic_or_abort() {
    let inputs = fuzz_inputs();
    let mut panics = Vec::new();
    for (label, bytes) in &inputs {
        panics.extend(panics_for(label, bytes));
    }
    eprintln!("fuzzed {} inputs through {} entry points", inputs.len(), entry_points().len());
    assert!(panics.is_empty(), "{} panic(s):\n{}", panics.len(), panics.join("\n"));
}
