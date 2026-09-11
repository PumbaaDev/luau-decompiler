//! The probe source set: Luau programs chosen to force the compiler to emit
//! every opcode it can be made to emit.
//!
//! # Why a purpose-built set and not "a big script"
//!
//! No script uses everything. Measured on a 47-program corpus of ordinary Luau,
//! the richest single file exercises 28 of 83 canonical opcodes and the whole
//! corpus pooled reaches 66. The rest never appear by accident: `SUBRK` needs a
//! constant on the *left* of a subtraction, `ORK` needs `x or 5`, `GETGLOBAL`
//! needs a global the chunk also writes to (otherwise the import optimiser
//! turns it into `GETIMPORT`), `FORGPREP_NEXT` needs `pairs`/`next` specifically.
//! `LOADKX` and `JUMPX` are stranger still: they exist only to escape a signed
//! 16-bit field, so nothing under 32 768 constants or 32 768 instructions will
//! ever produce one.
//!
//! This set writes each of those constructs on purpose. It reaches 79 of the 83
//! canonical opcodes.
//!
//! # The four that cannot be forced, and why that is not a gap in the set
//!
//! `NOP`, `BREAK`, `NATIVECALL` and `COVERAGE` are absent because no compiler
//! emits them from source. `NOP` and `BREAK` are inserted at runtime by the
//! debugger; `NATIVECALL` is patched in by the native code generator at load
//! time; `COVERAGE` requires a compiler option no shipping configuration sets.
//! A fifth internal slot — a generic-for variant removed from upstream Luau —
//! has no canonical counterpart at all.
//!
//! These stay on the inference path. That is the honest outcome: the probe
//! collapses the residual ambiguity from 84-way to 5-way, which is most of the
//! value even for the slots it cannot pin.
//!
//! # Structural rules the set obeys
//!
//! **One construct family per proto.** A proto that fails to align loses every
//! opcode in it, so the set is written as many one-line functions rather than
//! a few large ones.
//!
//! **Every opcode in at least two files.** Each `p*` file has an `m*` mirror
//! that exercises the same opcodes through textually different source. If a
//! client compiler lowers one file's construct differently, the mirror still
//! carries the opcode.
//!
//! **No `getfenv`/`setfenv`** anywhere: either one disables import and builtin
//! optimisation for the whole chunk, which would silently delete `GETIMPORT`
//! and every `FASTCALL*` from the set.
//!
//! **No vector constructors.** A client that configures a vector library
//! compiles `SomeVector.new(a, b, c)` to a fastcall where upstream emits
//! `GETIMPORT` + `CALL`, which would reject the proto.
//!
//! **No `bit32`.** Some clients implement bitwise operations as native opcodes
//! that upstream Luau does not have, so a `bit32` call would lower to one
//! instruction on one side and three on the other. The set gets `FASTCALL2K`
//! and `FASTCALL3` from `string.sub` and `math.clamp` instead, which have no
//! such divergence.

use super::alignment::{canonical_to_internal, CANONICAL_OPCODE_COUNT};
use super::opcodes::LuauOpcode;
use super::types::{insn_op, Chunk};

/// Bump when the source set changes in a way that changes what it emits.
/// Recorded in every derived database entry so a thin or stale solution is
/// visible rather than silent.
pub const PROBE_SET_VERSION: u32 = 1;

/// How expensive a probe source is to ship and compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeTier {
    /// Small, fast, and enough for 77 of 83 opcodes. A few hundred bytes each.
    Core,
    /// Hundreds of kilobytes of generated source, for the two opcodes that
    /// only appear once a signed 16-bit field overflows. Optional: an
    /// environment that cannot compile these simply reports 77 instead of 79.
    Heavy,
}

impl ProbeTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeTier::Core => "core",
            ProbeTier::Heavy => "heavy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "core" => Some(ProbeTier::Core),
            "heavy" => Some(ProbeTier::Heavy),
            _ => None,
        }
    }
}

/// One program in the set.
pub struct ProbeSource {
    pub name: &'static str,
    pub tier: ProbeTier,
    /// Canonical opcode NAMES this source is expected to force. Used as a
    /// canary: if a rebuilt canonical reference stops containing one of these,
    /// the compiler drifted or the wrong optimisation level was used, and the
    /// derivation should be distrusted rather than quietly under-cover.
    pub expects: &'static [&'static str],
    text: Option<&'static str>,
}

impl ProbeSource {
    /// The Luau source. Heavy-tier sources are generated on demand rather than
    /// stored, so that shipping two rare opcodes does not cost the repository
    /// half a megabyte of machine-written text.
    pub fn source(&self) -> String {
        match self.text {
            Some(t) => t.to_string(),
            None => generate_heavy(self.name),
        }
    }

    /// Filename this source should be written as.
    pub fn file_name(&self) -> String {
        format!("{}.luau", self.name)
    }
}

macro_rules! core_source {
    ($name:literal, [$($op:literal),* $(,)?]) => {
        ProbeSource {
            name: $name,
            tier: ProbeTier::Core,
            expects: &[$($op),*],
            text: Some(include_str!(concat!("../../probe/sources/", $name, ".luau"))),
        }
    };
}

/// The complete set, in a stable order.
pub fn probe_sources() -> &'static [ProbeSource] {
    &PROBE_SOURCES
}

/// Look one source up by name.
pub fn probe_source(name: &str) -> Option<&'static ProbeSource> {
    PROBE_SOURCES.iter().find(|s| s.name == name)
}

static PROBE_SOURCES: &[ProbeSource] = &[
    core_source!(
        "p01_arith",
        [
            "ADD", "DIV", "DUPCLOSURE", "IDIV", "MOD", "MOVE", "MUL", "NEWTABLE", "POW",
            "PREPVARARGS", "RETURN", "SETLIST", "SUB",
        ]
    ),
    core_source!(
        "p02_arithk",
        [
            "ADDK", "DIVK", "DIVRK", "DUPCLOSURE", "IDIVK", "MODK", "MOVE", "MULK", "NEWTABLE",
            "POWK", "PREPVARARGS", "RETURN", "SETLIST", "SUBK", "SUBRK",
        ]
    ),
    core_source!(
        "p03_unary",
        [
            "DUPCLOSURE", "LENGTH", "MINUS", "MOVE", "NEWTABLE", "NOT", "PREPVARARGS",
            "RETURN", "SETLIST",
        ]
    ),
    core_source!(
        "p04_load",
        [
            "CONCAT", "DUPCLOSURE", "LOADB", "LOADK", "LOADN", "LOADNIL", "MOVE", "NEWTABLE",
            "PREPVARARGS", "RETURN", "SETLIST",
        ]
    ),
    core_source!(
        "p05_global",
        [
            "DUPCLOSURE", "GETGLOBAL", "MOVE", "NEWTABLE", "PREPVARARGS", "RETURN",
            "SETGLOBAL", "SETLIST",
        ]
    ),
    core_source!(
        "p06_branch",
        [
            "ADDK", "DUPCLOSURE", "GETTABLEKS", "JUMP", "JUMPBACK", "JUMPIF", "JUMPIFEQ",
            "JUMPIFLE", "JUMPIFLT", "JUMPIFNOT", "JUMPIFNOTEQ", "JUMPIFNOTLE", "JUMPIFNOTLT",
            "LOADN", "MOVE", "NEWTABLE", "PREPVARARGS", "RETURN", "SETLIST", "SETTABLEKS",
        ]
    ),
    core_source!(
        "p07_xeq",
        [
            "DUPCLOSURE", "JUMPXEQKB", "JUMPXEQKN", "JUMPXEQKNIL", "JUMPXEQKS", "LOADN",
            "MOVE", "NEWTABLE", "PREPVARARGS", "RETURN", "SETLIST", "SETTABLEKS",
        ]
    ),
    core_source!(
        "p08_logic",
        [
            "AND", "ANDK", "DUPCLOSURE", "MOVE", "NEWTABLE", "OR", "ORK", "PREPVARARGS",
            "RETURN", "SETLIST",
        ]
    ),
    core_source!(
        "p09_table",
        [
            "DUPCLOSURE", "DUPTABLE", "GETTABLE", "GETTABLEKS", "GETTABLEN", "LOADN", "MOVE",
            "NEWTABLE", "PREPVARARGS", "RETURN", "SETLIST", "SETTABLE", "SETTABLEKS",
            "SETTABLEN",
        ]
    ),
    core_source!(
        "p10_loop",
        [
            "ADD", "ADDK", "CALL", "DUPCLOSURE", "FORGLOOP", "FORGPREP", "FORGPREP_INEXT",
            "FORGPREP_NEXT", "FORNLOOP", "FORNPREP", "GETIMPORT", "GETTABLEKS", "LOADN",
            "LOADNIL", "MOVE", "NEWTABLE", "PREPVARARGS", "RETURN", "SETLIST",
        ]
    ),
    core_source!(
        "p11_closure",
        [
            "ADDK", "CAPTURE", "CLOSEUPVALS", "DUPCLOSURE", "FORNLOOP", "FORNPREP", "GETUPVAL",
            "LOADN", "MOVE", "NEWCLOSURE", "NEWTABLE", "PREPVARARGS", "RETURN", "SETLIST",
            "SETTABLE", "SETUPVAL",
        ]
    ),
    core_source!(
        "p12_call",
        [
            "CALL", "DUPCLOSURE", "GETVARARGS", "MOVE", "NAMECALL", "NEWTABLE", "PREPVARARGS",
            "RETURN", "SETLIST",
        ]
    ),
    core_source!(
        "p13_fastcall",
        [
            "CALL", "DUPCLOSURE", "FASTCALL", "FASTCALL1", "FASTCALL2", "FASTCALL2K",
            "FASTCALL3", "GETIMPORT", "GETVARARGS", "LOADK", "MOVE", "NEWTABLE", "PREPVARARGS",
            "RETURN", "SETLIST",
        ]
    ),
    core_source!(
        "m01_mirror_arith",
        [
            "ADD", "ADDK", "CONCAT", "DIV", "DIVK", "DIVRK", "DUPCLOSURE", "IDIV", "IDIVK",
            "LENGTH", "LOADB", "LOADK", "LOADN", "LOADNIL", "MINUS", "MOD", "MODK", "MOVE",
            "MUL", "MULK", "NEWTABLE", "NOT", "POW", "POWK", "PREPVARARGS", "RETURN",
            "SETLIST", "SUB", "SUBK", "SUBRK",
        ]
    ),
    core_source!(
        "m02_mirror_branch",
        [
            "ADDK", "DUPCLOSURE", "GETTABLEKS", "JUMP", "JUMPBACK", "JUMPIF", "JUMPIFEQ",
            "JUMPIFLE", "JUMPIFLT", "JUMPIFNOT", "JUMPIFNOTEQ", "JUMPIFNOTLE", "JUMPIFNOTLT",
            "JUMPXEQKB", "JUMPXEQKN", "JUMPXEQKNIL", "JUMPXEQKS", "LOADN", "MOVE", "NEWTABLE",
            "PREPVARARGS", "RETURN", "SETLIST", "SETTABLEKS",
        ]
    ),
    core_source!(
        "m03_mirror_table",
        [
            "AND", "ANDK", "DUPCLOSURE", "DUPTABLE", "GETTABLE", "GETTABLEKS", "GETTABLEN",
            "LOADN", "MOVE", "NEWTABLE", "OR", "ORK", "PREPVARARGS", "RETURN", "SETLIST",
            "SETTABLE", "SETTABLEKS", "SETTABLEN",
        ]
    ),
    core_source!(
        "m04_mirror_flow",
        [
            "ADD", "ADDK", "CALL", "CAPTURE", "CLOSEUPVALS", "DUPCLOSURE", "FASTCALL",
            "FASTCALL1", "FASTCALL2", "FASTCALL2K", "FASTCALL3", "FORGLOOP", "FORGPREP",
            "FORGPREP_INEXT", "FORGPREP_NEXT", "FORNLOOP", "FORNPREP", "GETGLOBAL",
            "GETIMPORT", "GETTABLEKS", "GETUPVAL", "GETVARARGS", "LOADK", "LOADN", "LOADNIL",
            "MOVE", "NAMECALL", "NEWCLOSURE", "NEWTABLE", "PREPVARARGS", "RETURN", "SETGLOBAL",
            "SETLIST", "SETTABLE", "SETUPVAL",
        ]
    ),
    ProbeSource {
        name: "h01_loadkx",
        tier: ProbeTier::Heavy,
        expects: &[
            "DUPCLOSURE", "LOADK", "LOADKX", "NEWTABLE", "PREPVARARGS", "RETURN", "SETLIST",
        ],
        text: None,
    },
    ProbeSource {
        name: "h02_jumpx",
        tier: ProbeTier::Heavy,
        expects: &[
            "ADD", "DUPCLOSURE", "JUMP", "JUMPIFNOT", "JUMPX", "LOADN", "PREPVARARGS", "RETURN",
        ],
        text: None,
    },
];

/// Distinct string constants needed before `LOADK`'s signed 16-bit D field
/// overflows and the compiler must fall back to `LOADKX`.
const LOADKX_CONSTANTS: usize = 33_000;
/// Instructions a forward jump must span before `JUMP`'s signed 16-bit D field
/// overflows and the compiler must fall back to `JUMPX`.
const JUMPX_ROWS: usize = 3_300;
const JUMPX_TERMS_PER_ROW: usize = 10;

/// Generate a heavy-tier source. Deterministic: the same name always produces
/// byte-identical text, so a reference build and a client build agree.
fn generate_heavy(name: &str) -> String {
    match name {
        "h01_loadkx" => generate_loadkx(),
        "h02_jumpx" => generate_jumpx(),
        _ => String::new(),
    }
}

/// A table of more distinct string constants than a 16-bit constant index can
/// address. Nothing else forces `LOADKX`.
fn generate_loadkx() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(LOADKX_CONSTANTS * 9 + 64);
    out.push_str("-- generated: forces LOADKX by overflowing the 16-bit constant index\n");
    out.push_str("local function h_loadkx()\n\treturn {\n");
    let mut written = 0usize;
    'outer: for a in ALPHABET {
        for b in ALPHABET {
            for c in ALPHABET {
                for d in ALPHABET {
                    if written % 20 == 0 {
                        out.push_str("\t\t");
                    }
                    out.push('"');
                    out.push('k');
                    out.push(*a as char);
                    out.push(*b as char);
                    out.push(*c as char);
                    out.push(*d as char);
                    out.push_str("\",");
                    written += 1;
                    if written % 20 == 0 {
                        out.push('\n');
                    }
                    if written >= LOADKX_CONSTANTS {
                        break 'outer;
                    }
                }
            }
        }
    }
    if written % 20 != 0 {
        out.push('\n');
    }
    out.push_str("\t}\nend\nreturn h_loadkx\n");
    out
}

/// A conditional body longer than a 16-bit jump offset can reach. Nothing else
/// forces `JUMPX`.
fn generate_jumpx() -> String {
    let mut out = String::with_capacity(JUMPX_ROWS * 48 + 128);
    out.push_str("-- generated: forces JUMPX by overflowing the 16-bit jump offset\n");
    out.push_str("local function h_jumpx(flag, x, y)\n\tlocal s = 0\n\tif flag then\n");
    for _ in 0..JUMPX_ROWS {
        out.push_str("\t\ts = s");
        for t in 0..JUMPX_TERMS_PER_ROW {
            out.push_str(if t % 2 == 0 { " + x" } else { " + y" });
        }
        out.push('\n');
    }
    out.push_str("\tend\n\treturn s\nend\nreturn h_jumpx\n");
    out
}

/// Which canonical opcodes actually occur in a canonical reference chunk.
///
/// Walks the chunk under the canonical layout, skipping AUX words. Returns
/// `None` if the chunk is not canonical Luau — which is itself the answer to
/// "did I hand `probe align` the right file?".
pub fn observed_canonical_opcodes(chunk: &Chunk) -> Option<[bool; CANONICAL_OPCODE_COUNT]> {
    let mut seen = [false; CANONICAL_OPCODE_COUNT];
    for proto in &chunk.protos {
        let code = &proto.code;
        let mut i = 0usize;
        while i < code.len() {
            let canonical_op = insn_op(code[i]);
            let internal = canonical_to_internal(canonical_op)?;
            if (canonical_op as usize) < CANONICAL_OPCODE_COUNT {
                seen[canonical_op as usize] = true;
            }
            if LuauOpcode::from_u8(internal).has_aux() {
                if i + 1 >= code.len() {
                    return None;
                }
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    Some(seen)
}

/// Expected opcodes that a canonical reference did NOT contain.
///
/// A non-empty result means the reference was built differently from the one
/// the set was designed against — a different optimisation level, or a drifted
/// compiler. The derivation is still safe (it can only under-cover), but the
/// caller should be told rather than left to wonder why coverage is low.
pub fn missing_expected(source: &ProbeSource, canonical: &Chunk) -> Vec<&'static str> {
    let Some(seen) = observed_canonical_opcodes(canonical) else {
        return source.expects.to_vec();
    };
    source
        .expects
        .iter()
        .copied()
        .filter(|name| {
            !(0..CANONICAL_OPCODE_COUNT as u8).any(|c| {
                seen[c as usize] && super::alignment::canonical_opcode_name(c) == Some(name)
            })
        })
        .collect()
}

/// The manifest, as JSON. Written by `probe emit` so an external runner knows
/// what it is compiling and what each file is for.
pub fn manifest_json(tier: Option<ProbeTier>) -> String {
    let files: Vec<serde_json::Value> = probe_sources()
        .iter()
        .filter(|s| tier.map(|t| s.tier == t).unwrap_or(true))
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "file": s.file_name(),
                "tier": s.tier.as_str(),
                "expects": s.expects,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "format": "luau-opmap-probe-set",
        "probe_set_version": PROBE_SET_VERSION,
        "bytecode_version": 6,
        "compiler": { "optimization_level": 1, "debug_level": 1 },
        "files": files,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_source_is_non_empty_and_named() {
        for s in probe_sources() {
            assert!(!s.name.is_empty());
            let text = s.source();
            assert!(
                text.len() > 32,
                "{} produced only {} bytes",
                s.name,
                text.len()
            );
            assert!(!s.expects.is_empty(), "{} declares no expectations", s.name);
        }
    }

    #[test]
    fn source_names_are_unique() {
        let mut seen = HashSet::new();
        for s in probe_sources() {
            assert!(seen.insert(s.name), "duplicate probe source {}", s.name);
        }
    }

    #[test]
    fn every_expected_name_is_a_real_canonical_opcode() {
        let canonical: HashSet<&str> = (0..CANONICAL_OPCODE_COUNT as u8)
            .filter_map(super::super::alignment::canonical_opcode_name)
            .collect();
        for s in probe_sources() {
            for name in s.expects {
                assert!(
                    canonical.contains(name),
                    "{} expects {}, which is not a canonical opcode",
                    s.name,
                    name
                );
            }
        }
    }

    /// The whole point of the set: between them the sources must claim every
    /// opcode that can be forced. If this drops, coverage was lost.
    #[test]
    fn set_claims_seventy_nine_of_the_eighty_three_canonical_opcodes() {
        let claimed: HashSet<&str> = probe_sources()
            .iter()
            .flat_map(|s| s.expects.iter().copied())
            .collect();
        assert_eq!(claimed.len(), 79, "claimed: {:?}", {
            let mut v: Vec<_> = claimed.iter().collect();
            v.sort();
            v
        });

        let all: HashSet<&str> = (0..CANONICAL_OPCODE_COUNT as u8)
            .filter_map(super::super::alignment::canonical_opcode_name)
            .collect();
        let mut unclaimed: Vec<&str> = all.difference(&claimed).copied().collect();
        unclaimed.sort();
        assert_eq!(
            unclaimed,
            vec!["BREAK", "COVERAGE", "NATIVECALL", "NOP"],
            "only the four opcodes no compiler emits may be unclaimed"
        );
    }

    /// Redundancy rule: a client compiler that lowers one file differently must
    /// not be able to take an opcode with it.
    #[test]
    fn every_core_opcode_is_claimed_by_at_least_two_sources() {
        let mut count: std::collections::HashMap<&str, usize> = Default::default();
        for s in probe_sources() {
            for name in s.expects {
                *count.entry(name).or_default() += 1;
            }
        }
        let singles: Vec<&str> = count
            .iter()
            .filter(|(_, &n)| n < 2)
            .map(|(&k, _)| k)
            .collect();
        let mut singles = singles;
        singles.sort();
        // Only the heavy tier is allowed to be single-sourced: each of its two
        // opcodes costs hundreds of kilobytes, so a mirror is not worth it.
        assert_eq!(singles, vec!["JUMPX", "LOADKX"], "unmirrored opcodes");
    }

    #[test]
    fn probe_sources_avoid_constructs_that_break_alignment() {
        for s in probe_sources() {
            let text = s.source();
            for banned in ["getfenv", "setfenv", "bit32", "Vector3", "vector."] {
                assert!(
                    !text.contains(banned),
                    "{} uses {}, which makes client and reference codegen diverge",
                    s.name,
                    banned
                );
            }
        }
    }

    #[test]
    fn heavy_sources_are_deterministic_and_large_enough() {
        let a = probe_source("h01_loadkx").unwrap().source();
        let b = probe_source("h01_loadkx").unwrap().source();
        assert_eq!(a, b, "generation must be reproducible");
        assert_eq!(
            a.matches('"').count() / 2,
            LOADKX_CONSTANTS,
            "must exceed the 16-bit constant index"
        );
        assert!(LOADKX_CONSTANTS > 32_767);

        let j = probe_source("h02_jumpx").unwrap().source();
        assert_eq!(j, probe_source("h02_jumpx").unwrap().source());
        assert!(
            JUMPX_ROWS * JUMPX_TERMS_PER_ROW > 32_767,
            "must exceed the 16-bit jump offset"
        );
        assert_eq!(j.lines().filter(|l| l.starts_with("\t\ts = s")).count(), JUMPX_ROWS);
    }

    #[test]
    fn manifest_lists_the_requested_tier_only() {
        let core = manifest_json(Some(ProbeTier::Core));
        assert!(core.contains("p01_arith"));
        assert!(!core.contains("h01_loadkx"));
        let all = manifest_json(None);
        assert!(all.contains("h01_loadkx"));
        assert!(all.contains("p01_arith"));
        // Must be valid JSON.
        let v: serde_json::Value = serde_json::from_str(&all).expect("manifest is JSON");
        assert_eq!(v["probe_set_version"], PROBE_SET_VERSION);
    }

    #[test]
    fn tier_round_trips_through_its_string_form() {
        for t in [ProbeTier::Core, ProbeTier::Heavy] {
            assert_eq!(ProbeTier::parse(t.as_str()), Some(t));
        }
        assert_eq!(ProbeTier::parse("nonsense"), None);
    }
}
