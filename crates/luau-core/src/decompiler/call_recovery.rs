//! Counts, per chunk, how many protos emitted FEWER calls than their bytecode
//! contains.
//!
//! ── WHY THIS EXISTS ─────────────────────────────────────────────────────
//! A dropped function body leaves no undefined name, no bad call and no
//! discarded value, so every existing semantic check scores it CLEAN. Measured
//! on the 1,143-file corpus: 8 protos emit ZERO calls where the bytecode has
//! three or more (`99_BeequipTypes::_CheckReqs` loses 20,
//! `277_DragManager::OnInputBegin` loses 15), and 27 more emit under half.
//! Twenty-nine files, none of them reported by anything.
//!
//! The comparison needs no ground truth, no model and no judgement: the
//! bytecode states how many calls a function makes and the AST states how many
//! were emitted. A function that makes twenty calls and emits none is broken by
//! inspection.
//!
//! ── COUNTING RULE, AND THE MISTAKE IT ENCODES ───────────────────────────
//! Only `LuauOpcode::Call` counts on the bytecode side. `obj:Method()` compiles
//! to NAMECALL FOLLOWED BY CALL - two instructions for one call expression -
//! and counting both inflates every method call by one, which manufactured a
//! ten-file "dropped call" result that was entirely a miscount. Every call,
//! plain or method, terminates in exactly one CALL.
//!
//! Closure bodies are excluded on the AST side because `Expr::Function` is a
//! separate proto, counted on its own.

use std::cell::RefCell;

thread_local! {
    /// (protos short of their call count, total calls missing)
    static DEFICIT: RefCell<(usize, usize)> = const { RefCell::new((0, 0)) };
    /// Proto indices already counted for this chunk.
    ///
    /// A proto can be lifted MORE THAN ONCE - `lv_0905_LocalCubs` lifts proto
    /// #0 three times, and each pass recorded the same deficit, so the header
    /// read "3 proto(s) emitted 30 fewer call(s)" for ONE function losing 10.
    /// That is a counting error, not three defects: it inflated both the proto
    /// count and the call count threefold and made a single-function problem
    /// look like a file-wide collapse. Counting each proto index once states
    /// what is actually true. The repeated lift is a separate matter and is
    /// left alone here - this module measures, it does not schedule.
    static COUNTED: RefCell<std::collections::HashSet<usize>> =
        RefCell::new(std::collections::HashSet::new());
}

/// Reset at the start of a chunk. Without this the counts accumulate across
/// files in a batch run and every file after the first reports the corpus.
pub fn reset() {
    DEFICIT.with(|d| *d.borrow_mut() = (0, 0));
    COUNTED.with(|c| c.borrow_mut().clear());
}

/// THRESHOLDED ON PURPOSE, to what has been verified by inspection.
///
/// Any-deficit reporting flags 95 files; the shape actually demonstrated -
/// a body that emits none or under half of its calls - is 29. The gap is
/// small shortfalls in large functions, e.g. `23_Invisicam::Update@395`
/// emitting 20 of 21, which is far likelier a counting edge (dead code, a
/// fused builtin, an unreachable path) than a dropped body, and has NOT been
/// shown to be a defect.
///
/// Reporting those as defects would assert 95 on the strength of evidence for
/// 29. They are UNADJUDICATED, not dismissed: raise this threshold and they
/// reappear.
/// `who` names the proto. It is not used for counting; it exists because the
/// header reports HOW MANY protos came up short and never WHICH, and every
/// rotation that tried to identify one by pairing counts across a print
/// supplied the identifier by assumption and got it wrong. Set
/// `LUAU_CALL_SHORT` and the proto names itself.
pub fn record(proto_index: usize, who: &str, bytecode_calls: usize, emitted_calls: usize) {
    if emitted_calls >= bytecode_calls {
        return;
    }
    let gutted = emitted_calls == 0 && bytecode_calls >= 3;
    let halved = bytecode_calls >= 4 && emitted_calls * 2 < bytecode_calls;
    if !(gutted || halved) {
        return;
    }
    if std::env::var("LUAU_CALL_SHORT").is_ok() {
        eprintln!(
            "CALL-SHORT  {}  bytecode={} emitted={} missing={}",
            who,
            bytecode_calls,
            emitted_calls,
            bytecode_calls - emitted_calls
        );
    }
    // One proto, one count - however many times it was lifted.
    if !COUNTED.with(|c| c.borrow_mut().insert(proto_index)) {
        return;
    }
    DEFICIT.with(|d| {
        let mut d = d.borrow_mut();
        d.0 += 1;
        d.1 += bytecode_calls - emitted_calls;
    });
}

/// `(protos_short, calls_missing)` for the chunk lifted so far.
pub fn snapshot() -> (usize, usize) {
    DEFICIT.with(|d| *d.borrow())
}
