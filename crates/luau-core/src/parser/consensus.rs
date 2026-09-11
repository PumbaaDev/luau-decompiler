//! Cross-script opcode-shuffle consensus.
//!
//! # Why this exists
//!
//! Roblox permutes the opcode numbering per client version, and
//! [`crate::parser::opmap`] infers that permutation from a single chunk. A
//! single chunk is weak evidence: a typical script exercises only 8-29 distinct
//! opcodes out of ~84, so most of the permutation is unconstrained by it (see
//! the `OBSERVABILITY_FLOOR` note in `decompile_with_opmap`). Measured against a
//! ground-truth corpus, solo detection recovers about 50% of the opcode bytes a
//! file actually uses, and zero files out of 47 are fully correct.
//!
//! But every script from one client version shares ONE permutation. Across
//! files the same shuffled byte recurs with the same true meaning, while the
//! detectors' *errors* vary from file to file. Pooling one vote per file and
//! taking the majority recovers about 70% of byte slots — a ~20 point gain from
//! evidence that is already being computed and thrown away.
//!
//! # Two rules that are not negotiable
//!
//! **1. Ballots must be prior-free.** A ballot is a file's SOLO detection
//! ([`OpcodeMap::detect`]), never a map that was itself produced under a prior.
//! Feeding merged maps back into the tally makes the first file's guess vote
//! once per subsequent file, which is auto-correlation, not evidence. That is
//! the mechanism behind the measured result that a naively shared cache scores
//! *worse* (~36-49%) than no cache at all (~50%), and why the damage is worst
//! when the smallest, least-evidenced file happens to go first.
//!
//! **2. One file, one vote — never weight by detector confidence.** Weighting
//! by [`OpcodeMap::heuristic_evidence`] measures ~63% against ~70% for plain
//! majority. That field counts how many detector passes re-confirmed a byte
//! across three near-identical re-runs of the same detectors on the same file,
//! so a detector that is confidently wrong three times outranks one that is
//! quietly right once. It is a repetition counter, not calibrated confidence.
//! Weighting by instruction count, mapped count or distinct-opcode count all
//! measured below unweighted majority too. Admit or reject a ballot; do not
//! weight it.
//!
//! # What this cannot do
//!
//! Roughly a quarter of byte slots are *dead*: no file's detector ever proposes
//! the true opcode for them, so no vote-counting scheme can recover them. Those
//! are overwhelmingly the rare opcodes (arithmetic-with-constant, the compare-
//! and-jump family, upvalue ops, `SETLIST`, `NEWTABLE`) that appear in only a
//! handful of files. Consensus cannot invent an answer nobody produced; closing
//! that gap needs new detectors, not better voting.

use std::collections::HashMap;

/// One file's independent opinion about the shuffle.
///
/// `map` must come from prior-free solo detection, and `present` must mark the
/// bytes that actually occur at true instruction positions in that file (see
/// [`OpcodeMap::present_byte_mask`](crate::parser::opmap::OpcodeMap::present_byte_mask)).
/// The `present` mask is what makes an *absence* distinguishable from an
/// *abstention*: a byte a file never contained is not a dissenting vote about
/// that byte, and must not count against it.
#[derive(Debug, Clone)]
pub struct Ballot {
    /// Content identity of the bytecode this ballot came from. Re-observing the
    /// same script must replace its ballot, not add a second one — otherwise a
    /// frequently re-decompiled script accumulates unbounded weight.
    pub key: u64,
    /// Solo heuristic map: shuffled byte -> canonical opcode, 255 = unmapped.
    pub map: [u8; 256],
    /// Bytes occurring at true instruction positions in this file.
    pub present: [bool; 256],
}

impl Ballot {
    pub fn new(key: u64, map: [u8; 256], present: [bool; 256]) -> Self {
        Self { key, map, present }
    }
}

/// Publication gate for a resolved mapping.
///
/// The gate exists to keep a fractured vote from being *load-bearing*. Under a
/// locked prior a wrong entry costs two slots, not one: the byte is occupied so
/// no detector can move it, and the canonical opcode is claimed so the byte
/// that really holds that opcode cannot take it either. Withholding a mapping
/// costs nothing by comparison — the byte stays 255, per-file detectors run on
/// it unblocked, and bijection completion fills it exactly as it does when the
/// cache simply has a gap. This mirrors the "prefer UNMAPPED over WRONG" rule
/// the detector suite already applies to structurally-required opcodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsensusConfig {
    /// Publish nothing at all until this many ballots have been cast. Guards
    /// the cold start: with one ballot, "consensus" is one arbitrary file's
    /// guess wearing a consensus badge, which is precisely the failure being
    /// removed. Below the floor every file falls back to solo detection.
    pub min_ballots: usize,
    /// Absolute floor on votes for a mapping.
    pub min_votes: u32,
    /// Floor on `votes * 100 / support`, where support counts ballots in which
    /// the byte is PRESENT. Not a share of all ballots.
    pub min_share_pct: u32,
    /// Floor on `(votes - runner_up) * 100 / support`, where runner_up is the
    /// best-supported still-available rival opcode for the same byte.
    pub min_margin_pct: u32,
}

impl Default for ConsensusConfig {
    /// Plain unweighted majority with a cold-start floor: publish the winner
    /// whatever its share.
    ///
    /// The gates default to OFF because turning them on was measured and it
    /// LOSES, badly: `min_share_pct = 40` scores 60.5% against 70.4% ungated,
    /// and `min_margin_pct = 10` scores 66.5%. The reason is that withholding
    /// is not free after all. A withheld byte does not stay honestly unmapped —
    /// it falls through to bijection completion, which invents an answer from
    /// what this one chunk happens to contain. Even a 30%-share plurality drawn
    /// from dozens of files beats that invention. The gate's one real benefit,
    /// freeing the canonical opcode so the byte that truly holds it can be
    /// assigned, is far smaller than the loss.
    ///
    /// The knobs are kept because they are the right shape for a caller who can
    /// make withholding actually mean "leave unresolved", and because a future
    /// completion tier that declined to guess would change this trade. Do not
    /// turn them on again without re-measuring.
    ///
    /// `min_ballots` is the one gate that IS worth paying for, and 3 was too
    /// low. Measured end to end — pool the first K ballots in smallest-file-
    /// first arrival order, then score the map every file actually decodes with
    /// against ground truth, 800 present byte-slots per seed:
    ///
    /// ```text
    ///   K     seed 42  seed 1337  seed 424242  seed 55555   mean   vs solo
    ///   solo   55.88     56.62       57.00        58.38     56.97      —
    ///   3      50.00     54.50       56.88        60.25     55.41    -1.56
    ///   4      49.62     55.12       52.00        57.88     53.66    -3.31
    ///   5      58.75     61.88       60.88        63.62     61.28    +4.31
    ///   6      59.00     63.88       62.75        65.62     62.81    +5.84
    ///   7      60.50     65.38       65.12        68.00     64.75    +7.78
    /// ```
    ///
    /// Three or four ballots is not yet a consensus — it is a couple of tiny
    /// files' guesses, and publishing them LOCKS those guesses as a prior, which
    /// is strictly worse than letting each file detect for itself. From five
    /// ballots on, pooling wins on every seed and never looks back. Raising the
    /// floor further was considered and rejected: K = 5, 6, 7 are worth +4.3,
    /// +5.8 and +7.8 points, so a floor of 10 would throw away real evidence to
    /// avoid a cold zone that ends at four.
    fn default() -> Self {
        Self { min_ballots: 5, min_votes: 1, min_share_pct: 0, min_margin_pct: 0 }
    }
}

/// Outcome of resolving a ballot box, with the evidence that produced it.
///
/// The diagnostic fields are not decoration. A byte whose winner holds 8 of 30
/// against 13 rivals is a *measured detector gap*, and the vote table is the
/// only place that fact is observable — the old single-map cache erased it by
/// construction. That makes this the ranked worklist for which detectors are
/// worth writing next, which matters because new detectors are the only lever
/// left once consensus has been taken.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Published prior: shuffled byte -> canonical opcode, 255 = withheld.
    pub map: [u8; 256],
    /// Votes backing the published winner for each byte.
    pub votes_for: [u32; 256],
    /// Ballots in which each byte was present (the denominator).
    pub support: [u32; 256],
    /// Bytes where a winner existed but failed the gate.
    pub withheld: [bool; 256],
    /// Distinct opcodes proposed per byte — high means a fractured vote.
    pub rivals: [u32; 256],
    /// Ballots that took part.
    pub ballots: usize,
}

impl Resolved {
    /// An all-255 map, i.e. "no opinion". Callers should pass `None` rather
    /// than an empty prior when this is true.
    pub fn is_empty(&self) -> bool {
        self.map.iter().all(|&v| v == 255)
    }

    pub fn published(&self) -> usize {
        self.map.iter().filter(|&&v| v != 255).count()
    }
}

/// Resolve a ballot box into a prior map.
///
/// Pure function of the ballot *set*: votes are accumulated by unweighted
/// addition and ties broken on `(count, byte, opcode)`, so the result does not
/// depend on the order ballots arrived in. That property is the direct fix for
/// the measured 13-point spread between processing the same corpus
/// alphabetically versus smallest-file-first — under the old last-write-wins
/// cache the arrival order changed the answer.
///
/// Assignment is greedy bipartite matching over `(votes, byte, opcode)` triples
/// sorted by votes descending, which is the same algorithm as
/// [`crate::build_consensus_map`] and is the best-measured variant. A pair is
/// taken only when both its byte and its opcode are still free, so the result
/// is always a partial bijection.
pub fn resolve(ballots: &[Ballot], cfg: &ConsensusConfig) -> Resolved {
    let mut out = Resolved {
        map: [255u8; 256],
        votes_for: [0u32; 256],
        support: [0u32; 256],
        withheld: [false; 256],
        rivals: [0u32; 256],
        ballots: ballots.len(),
    };
    if ballots.len() < cfg.min_ballots {
        return out;
    }

    // Tally. A ballot votes for a byte only when that byte actually occurs in
    // its file: an opinion about a byte the file never executed is unfounded,
    // and counting it would also break the share ratio by letting votes exceed
    // support.
    let mut votes = vec![[0u32; 256]; 256];
    for b in ballots {
        for byte in 0..256usize {
            if !b.present[byte] {
                continue;
            }
            out.support[byte] += 1;
            let op = b.map[byte];
            if op != 255 {
                votes[byte][op as usize] += 1;
            }
        }
    }
    for byte in 0..256usize {
        out.rivals[byte] = votes[byte].iter().filter(|&&c| c > 0).count() as u32;
    }

    // Priority list, ranked by RAW VOTE COUNT.
    //
    // Ranking by agreement *share* instead is the obvious refinement — 5 of 5
    // files agreeing looks more certain than 13 of 47 — and it was tried and it
    // loses: smoothed share scores 68.4% against 70.4% for raw counts, and
    // drops fully-correct files from 5 of 47 to 4. Raw count is not really
    // measuring popularity here; a byte occurring in many files has had many
    // independent chances to be called correctly, and that breadth turns out to
    // carry more signal than the local agreement rate does.
    //
    // Deterministic total order — votes, then byte, then opcode — so the result
    // is a pure function of the ballot SET and not of arrival order.
    let mut triples: Vec<(u32, u8, u8)> = Vec::new();
    for byte in 0..256usize {
        for op in 0..256usize {
            let c = votes[byte][op];
            if c > 0 {
                triples.push((c, byte as u8, op as u8));
            }
        }
    }
    triples.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));

    let mut byte_assigned = [false; 256];
    let mut opcode_assigned = [false; 256];
    for &(count, byte, op) in &triples {
        let bi = byte as usize;
        let oi = op as usize;
        if byte_assigned[bi] || opcode_assigned[oi] {
            continue;
        }
        let support = out.support[bi];
        if support == 0 {
            continue;
        }
        // Runner-up among opcodes still available at this point. Rivals that
        // have already been claimed by a better-supported byte are not real
        // competition for this slot any more.
        let runner = (0..256usize)
            .filter(|&o| o != oi && !opcode_assigned[o])
            .map(|o| votes[bi][o])
            .max()
            .unwrap_or(0);

        // The six comparison branches (27..=32) are the one family where a lone
        // vote is worth less than no vote. They are mutually confusable by
        // construction — same encoding, same operand shape, differing only in
        // which way the test runs — so a single file's reading of one is close
        // to a coin flip. Worse, pinning one wrongly denies that opcode to the
        // byte that really holds it, and the family fills from a fixed
        // elimination order, so one bad pin cascades through the rest of it.
        //
        // Requiring a second, independent file to agree before pinning a member
        // of this family measured as a strict improvement: pooled round trip
        // 54 -> 56 over seven permutations, with no file lost on any seed.
        // Members that fail the gate are withheld rather than guessed, which
        // leaves the opcode free for a better-supported byte to claim.
        let is_comparison_branch = (27..=32).contains(&oi);
        let family_floor = if is_comparison_branch { 2 } else { cfg.min_votes };

        let passes = count >= cfg.min_votes
            && count >= family_floor
            && count as u64 * 100 >= support as u64 * cfg.min_share_pct as u64
            && (count.saturating_sub(runner)) as u64 * 100
                >= support as u64 * cfg.min_margin_pct as u64;

        if !passes {
            // Withhold the whole byte. Deliberately does NOT claim the opcode:
            // leaving it free is the entire point, so the byte that really
            // holds it can still be assigned.
            byte_assigned[bi] = true;
            out.withheld[bi] = true;
            continue;
        }

        out.map[bi] = op;
        out.votes_for[bi] = count;
        byte_assigned[bi] = true;
        opcode_assigned[oi] = true;
    }

    out
}

/// A ballot box keyed by content identity.
///
/// Keying by content hash is what makes re-observation idempotent. Production
/// re-decompiles the same scripts on every run against a cache that survives
/// restarts, so an append-per-decompile tally would let one frequently-rerun
/// script outvote the corpus. Replacing by key keeps one file to one vote no
/// matter how often it is seen.
#[derive(Debug, Clone, Default)]
pub struct BallotBox {
    by_key: HashMap<u64, Ballot>,
}

impl BallotBox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cast or replace a ballot.
    pub fn cast(&mut self, ballot: Ballot) {
        self.by_key.insert(ballot.key, ballot);
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn contains(&self, key: u64) -> bool {
        self.by_key.contains_key(&key)
    }

    /// Ballots in a deterministic order. `resolve` is order-independent, so
    /// this is for reproducible serialization and diagnostics.
    pub fn ballots(&self) -> Vec<Ballot> {
        let mut v: Vec<Ballot> = self.by_key.values().cloned().collect();
        v.sort_by_key(|b| b.key);
        v
    }

    pub fn resolve(&self, cfg: &ConsensusConfig) -> Resolved {
        resolve(&self.ballots(), cfg)
    }

    /// Resolve using only the ballots that appear to describe the SAME opcode
    /// permutation as `probe`.
    ///
    /// A tally is only meaningful within one shuffle. Roblox re-permutes the
    /// opcode numbering on every client release, so a store that outlives a
    /// client update will hold ballots from two different permutations; pooling
    /// them yields a map correct for neither. This is not a hypothetical:
    /// pooling two measured shuffles drops byte accuracy to ~33%, which is
    /// *worse than using no store at all*, whereas the same store filtered by
    /// this predicate scores the same as a single-shuffle store.
    ///
    /// Nothing is deleted and no bucket state is kept — the filter is applied
    /// per query, so a store spanning many client versions stays useful for all
    /// of them and a ballot can never end up permanently in the wrong group.
    pub fn resolve_for(&self, probe: &Ballot, cfg: &ConsensusConfig) -> Resolved {
        let compatible: Vec<Ballot> = self
            .ballots()
            .into_iter()
            .filter(|b| same_shuffle(&b.map, &probe.map))
            .collect();
        resolve(&compatible, cfg)
    }
}

/// Opcodes used to tell one client shuffle from another.
///
/// These are the opcodes whose detection is both near-universal (almost every
/// script contains them) and near-unanimous (files that detect them agree on
/// the byte). `Return` and `PrepVarargs` are detected in every file of the
/// measured corpus and agree unanimously; `GetImport` and `Call` are close
/// behind. Comparing whole maps instead would be far weaker, because it would
/// average the ~50%-accurate bulk of the map into the decision — the mistake
/// that makes whole-map agreement ratios mis-cluster.
pub const ANCHOR_OPCODES: [u8; 11] = [
    22, // Return       — present and unanimous in every measured file
    65, // PrepVarargs  — likewise
    12, // GetImport
    21, // Call
    6,  // Move
    5,  // LoadK
    20, // NameCall
    24, // JumpBack
    56, // ForNPrep
    57, // ForNLoop
    59, // ForGLoop
];

/// How many anchors must land on the same byte before two maps are taken to be
/// readings of the same permutation.
///
/// An ABSOLUTE count, deliberately not a share of the overlap. Anchor
/// *disagreement* is mostly detector error, which is common — the weaker
/// anchors are only ~80-95% reliable, so a genuine peer routinely disagrees on
/// several. Anchor *agreement* across different permutations, by contrast,
/// requires the same opcode to land on the same byte by chance, about 1/256 per
/// anchor. So agreements discriminate and disagreements do not, and scoring the
/// ratio just throws away real peers: a share-based rule measured 3.3 points
/// worse because it rejected 2-3 of every 47 genuine ballots.
const MIN_ANCHOR_AGREE: u32 = 3;

/// Do two maps look like readings of the same opcode permutation?
///
/// Compares only where both maps have an opinion about the same anchor opcode.
/// Two readings of one shuffle agree on the anchors nearly always; two readings
/// of *different* shuffles would have to collide on the same byte for the same
/// opcode by chance, which is ~1/256 per anchor, so requiring two agreements
/// makes a false match vanishingly unlikely.
///
/// Returns `false` when the anchors barely overlap. That is deliberate: with no
/// evidence the safe answer is to keep the ballot out of the tally, because an
/// under-evidenced file falling back to solo detection is a known ~50% outcome
/// whereas contamination is a measured ~33% one.
pub fn same_shuffle(a: &[u8; 256], b: &[u8; 256]) -> bool {
    let mut agree = 0u32;
    for &op in ANCHOR_OPCODES.iter() {
        let ab = a.iter().position(|&v| v == op);
        let bb = b.iter().position(|&v| v == op);
        if let (Some(x), Some(y)) = (ab, bb) {
            if x == y {
                agree += 1;
                if agree >= MIN_ANCHOR_AGREE {
                    return true;
                }
            }
        }
    }
    false
}

/// Content identity for a bytecode blob (FNV-1a 64).
///
/// Used only to deduplicate ballots, never for integrity, so a non-cryptographic
/// hash is the right tool and avoids a dependency.
pub fn content_key(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

// ── Serialization ────────────────────────────────────────────────────────────
//
// One JSON object per line, append-only, last line wins for a given key. Chosen
// so a crashed or concurrent writer can only ever cost the tail of the file,
// never corrupt what is already there, and so a malformed line can be skipped
// instead of taking the whole store down.
//
// Ballots are stored rather than the derived tally: the ballots are the
// evidence, the tally is an opinion about it. Keeping the raw votes means a
// change to the resolver costs zero re-decompiles, and it is what allows a
// mis-bucketed file to be re-partitioned later instead of being unrecoverable.

/// Encode one ballot as a single JSON line (no trailing newline).
pub fn encode_ballot(b: &Ballot) -> String {
    let mut m = String::with_capacity(512);
    for v in b.map.iter() {
        m.push_str(&format!("{:02x}", v));
    }
    let mut p = String::with_capacity(64);
    for chunk in b.present.chunks(4) {
        let mut nib = 0u8;
        for (i, &bit) in chunk.iter().enumerate() {
            if bit {
                nib |= 1 << i;
            }
        }
        p.push_str(&format!("{:x}", nib));
    }
    format!("{{\"k\":\"{:016x}\",\"m\":\"{}\",\"p\":\"{}\"}}", b.key, m, p)
}

/// Decode one JSON line. Returns `None` for anything unparseable — callers skip
/// bad lines rather than failing the whole store.
pub fn decode_ballot(line: &str) -> Option<Ballot> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let field = |name: &str| -> Option<&str> {
        let pat = format!("\"{}\":\"", name);
        let start = line.find(&pat)? + pat.len();
        let rest = &line[start..];
        let end = rest.find('"')?;
        Some(&rest[..end])
    };
    let k = u64::from_str_radix(field("k")?, 16).ok()?;
    let ms = field("m")?;
    if ms.len() != 512 {
        return None;
    }
    let mut map = [255u8; 256];
    for i in 0..256 {
        map[i] = u8::from_str_radix(&ms[i * 2..i * 2 + 2], 16).ok()?;
    }
    let ps = field("p")?;
    if ps.len() != 64 {
        return None;
    }
    let mut present = [false; 256];
    for (i, ch) in ps.chars().enumerate() {
        let nib = ch.to_digit(16)? as u8;
        for bit in 0..4 {
            present[i * 4 + bit] = nib & (1 << bit) != 0;
        }
    }
    Some(Ballot { key: k, map, present })
}

/// Parse a whole store. Malformed lines are skipped; later lines replace
/// earlier ones with the same key.
pub fn decode_book(text: &str) -> BallotBox {
    let mut book = BallotBox::new();
    for line in text.lines() {
        if let Some(b) = decode_ballot(line) {
            book.cast(b);
        }
    }
    book
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ballot(key: u64, pairs: &[(u8, u8)]) -> Ballot {
        let mut map = [255u8; 256];
        let mut present = [false; 256];
        for &(byte, op) in pairs {
            map[byte as usize] = op;
            present[byte as usize] = true;
        }
        Ballot::new(key, map, present)
    }

    fn open_cfg() -> ConsensusConfig {
        ConsensusConfig { min_ballots: 1, min_votes: 1, min_share_pct: 0, min_margin_pct: 0 }
    }

    #[test]
    fn majority_wins_over_scattered_rivals() {
        // Byte 0x35 is truly LOADN(4): two files agree, one dissents.
        let bs = vec![
            ballot(1, &[(0x35, 4)]),
            ballot(2, &[(0x35, 4)]),
            ballot(3, &[(0x35, 9)]),
        ];
        let r = resolve(&bs, &open_cfg());
        assert_eq!(r.map[0x35], 4, "majority opcode must win");
        assert_eq!(r.votes_for[0x35], 2);
        assert_eq!(r.support[0x35], 3);
        assert_eq!(r.rivals[0x35], 2);
    }

    #[test]
    fn re_casting_the_same_file_is_one_vote() {
        // Production re-decompiles the same scripts on every run. Without
        // content keying, one script could outvote the whole corpus.
        let mut book = BallotBox::new();
        for _ in 0..5 {
            book.cast(ballot(7, &[(0x35, 9)]));
        }
        book.cast(ballot(8, &[(0x35, 4)]));
        book.cast(ballot(9, &[(0x35, 4)]));
        assert_eq!(book.len(), 3, "same key must replace, not accumulate");
        let r = book.resolve(&open_cfg());
        assert_eq!(r.map[0x35], 4);
        assert_eq!(r.votes_for[0x35], 2);
    }

    #[test]
    fn resolution_is_independent_of_ballot_order() {
        // The direct regression test for the order-dependence of the old
        // last-write-wins cache.
        let a = ballot(1, &[(0x10, 6), (0x20, 22), (0x30, 4)]);
        let b = ballot(2, &[(0x10, 6), (0x20, 21), (0x30, 4)]);
        let c = ballot(3, &[(0x10, 7), (0x20, 22), (0x30, 4)]);
        let cfg = open_cfg();
        let base = resolve(&[a.clone(), b.clone(), c.clone()], &cfg).map;
        for order in [
            [c.clone(), a.clone(), b.clone()],
            [b.clone(), c.clone(), a.clone()],
            [b.clone(), a.clone(), c.clone()],
        ] {
            assert_eq!(resolve(&order, &cfg).map, base, "order must not change the map");
        }
    }

    #[test]
    fn absent_byte_is_not_a_dissenting_vote() {
        // A file that never contained the byte must not dilute its share; only
        // files where it is PRESENT form the denominator.
        let mut lone = ballot(1, &[(0x0C, 33)]);
        lone.present[0x0C] = true;
        let mut silent = ballot(2, &[(0x40, 22)]);
        silent.present[0x0C] = false;
        let cfg = ConsensusConfig { min_ballots: 1, min_votes: 1, min_share_pct: 90, min_margin_pct: 0 };
        let r = resolve(&[lone, silent], &cfg);
        assert_eq!(r.support[0x0C], 1, "only present ballots count toward support");
        assert_eq!(r.map[0x0C], 33, "unanimous among files that contain it");
    }

    #[test]
    fn fractured_vote_is_withheld_and_leaves_the_opcode_free() {
        // Modelled on the measured pathological byte: present in many files,
        // a dozen rival guesses, plurality far below half — and the true
        // opcode never proposed for it. Publishing that plurality would cost
        // two slots, because it also locks the canonical opcode away from the
        // byte that really holds it.
        let mut bs = Vec::new();
        for i in 0..4u64 {
            bs.push(ballot(i, &[(0x0C, 40)])); // plurality: 4 of 10
        }
        for i in 4..10u64 {
            bs.push(ballot(i, &[(0x0C, (50 + i) as u8)])); // six distinct rivals
        }
        // A different byte where opcode 40 has solid support.
        for (i, b) in bs.iter_mut().enumerate() {
            if i < 6 {
                b.map[0x77] = 40;
                b.present[0x77] = true;
            }
        }
        let cfg = ConsensusConfig { min_ballots: 1, min_votes: 1, min_share_pct: 50, min_margin_pct: 0 };
        let r = resolve(&bs, &cfg);
        assert!(r.withheld[0x0C], "40% plurality must not be published");
        assert_eq!(r.map[0x0C], 255, "withheld byte stays unmapped");
        assert_eq!(r.map[0x77], 40, "opcode stays available for its real byte");
    }

    #[test]
    fn cold_start_publishes_nothing() {
        // One ballot is not a consensus; it is one file's guess. Below the
        // floor every file must fall back to solo detection.
        let cfg = ConsensusConfig { min_ballots: 5, ..ConsensusConfig::default() };
        let bs: Vec<Ballot> = (0..4u64).map(|i| ballot(i, &[(0x35, 4)])).collect();
        let r = resolve(&bs, &cfg);
        assert!(r.is_empty(), "must publish nothing below the ballot floor");
        assert_eq!(r.published(), 0);
    }

    #[test]
    fn default_ballot_floor_clears_the_measured_cold_zone() {
        // Not a style preference — 3 and 4 ballots were MEASURED to decode
        // worse than no store at all (-1.6 and -3.3 points of byte accuracy
        // across four permutation seeds), because publishing a two-or-three
        // file majority locks those files' guesses in as a prior. Five is the
        // first count that wins on every seed. Lowering this back below five
        // reintroduces a known regression.
        let cfg = ConsensusConfig::default();
        assert!(cfg.min_ballots >= 5, "cold-start floor must clear K = 4");
        let four: Vec<Ballot> = (0..4u64).map(|i| ballot(i, &[(0x35, 4)])).collect();
        assert!(resolve(&four, &cfg).is_empty(), "four ballots must publish nothing");
        let five: Vec<Ballot> = (0..5u64).map(|i| ballot(i, &[(0x35, 4)])).collect();
        assert_eq!(resolve(&five, &cfg).map[0x35], 4, "five ballots must publish");
    }

    #[test]
    fn result_is_always_a_partial_bijection() {
        // Two bytes both claiming opcode 4; only the better-supported one may
        // take it, or the map would decode two distinct bytes as one opcode.
        let bs = vec![
            ballot(1, &[(0x10, 4), (0x20, 4)]),
            ballot(2, &[(0x10, 4), (0x20, 4)]),
            ballot(3, &[(0x10, 4)]),
        ];
        let r = resolve(&bs, &open_cfg());
        let mut seen = [false; 256];
        for &v in r.map.iter() {
            if v != 255 {
                assert!(!seen[v as usize], "opcode {} assigned twice", v);
                seen[v as usize] = true;
            }
        }
        assert_eq!(r.map[0x10], 4, "better-supported byte takes the opcode");
    }

    #[test]
    fn no_votes_yields_no_opinion() {
        let bs = vec![ballot(1, &[]), ballot(2, &[]), ballot(3, &[])];
        let r = resolve(&bs, &open_cfg());
        assert!(r.is_empty());
    }

    #[test]
    fn ballot_round_trips_through_the_wire_format() {
        let mut b = ballot(0xdead_beef_1234_5678, &[(0x00, 0), (0x35, 4), (0xFF, 94)]);
        b.present[0x01] = true; // present but unmapped
        let line = encode_ballot(&b);
        let back = decode_ballot(&line).expect("must decode");
        assert_eq!(back.key, b.key);
        assert_eq!(back.map, b.map);
        assert_eq!(back.present, b.present);
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let good = encode_ballot(&ballot(1, &[(0x35, 4)]));
        let text = format!("\n{}\nnot json\n{{\"k\":\"zz\"}}\n{{}}\n", good);
        let book = decode_book(&text);
        assert_eq!(book.len(), 1, "one good ballot survives a corrupt store");
        assert!(book.contains(1));
    }

    #[test]
    fn later_line_replaces_earlier_for_same_key() {
        let old = encode_ballot(&ballot(42, &[(0x35, 9)]));
        let new = encode_ballot(&ballot(42, &[(0x35, 4)]));
        let book = decode_book(&format!("{}\n{}\n", old, new));
        assert_eq!(book.len(), 1);
        assert_eq!(book.ballots()[0].map[0x35], 4, "last line wins");
    }

    /// Two readings of one shuffle agree on the anchors; two readings of
    /// different shuffles do not.
    #[test]
    fn same_shuffle_separates_two_permutations() {
        let mut x = [255u8; 256];
        x[0x82] = 22; // Return
        x[0x11] = 65; // PrepVarargs
        x[0x40] = 21; // Call
        x[0x55] = 6; // Move
        let mut y = x;
        y[0x40] = 255;
        y[0x41] = 21; // one weak anchor mis-detected — still the same shuffle
        let mut z = [255u8; 256];
        z[0x07] = 22; // wholly different permutation
        z[0x9C] = 65;
        z[0x40] = 21; // one coincidental collision
        z[0x56] = 6;

        assert!(same_shuffle(&x, &y), "detector error on a weak anchor is not a shuffle change");
        assert!(!same_shuffle(&x, &z), "one coincidence is not a shuffle match");
        assert!(same_shuffle(&x, &x));
    }

    #[test]
    fn too_little_anchor_evidence_is_not_a_match() {
        // With no shared evidence the safe answer is to stay out of the tally:
        // falling back to solo is a known ~50% outcome, contamination is a
        // measured ~33% one.
        let mut a = [255u8; 256];
        a[0x82] = 22;
        a[0x11] = 65;
        let mut b = [255u8; 256];
        b[0x82] = 22;
        b[0x11] = 65;
        assert!(!same_shuffle(&a, &b), "two anchors is not enough evidence");
        assert!(!same_shuffle(&[255u8; 256], &[255u8; 256]));
    }

    /// The measured catastrophe: pooling two client versions scores worse than
    /// using no store at all. Filtering by the probe must exclude the foreign
    /// ballots entirely.
    #[test]
    fn foreign_shuffle_ballots_are_excluded_from_the_tally() {
        let anchor = |m: &mut [u8; 256], ret: u8, pv: u8, imp: u8| {
            m[ret as usize] = 22;
            m[pv as usize] = 65;
            m[imp as usize] = 12;
        };
        let mut book = BallotBox::new();
        // Shuffle A: three files agree byte 0x30 is opcode 4.
        for i in 0..3u64 {
            let mut m = [255u8; 256];
            anchor(&mut m, 0x82, 0x11, 0x63);
            m[0x30] = 4;
            let mut p = [false; 256];
            for (b, &v) in m.iter().enumerate() {
                p[b] = v != 255;
            }
            book.cast(Ballot::new(i, m, p));
        }
        // Shuffle B: five files — a MAJORITY of the store — say 0x30 is 9.
        for i in 10..15u64 {
            let mut m = [255u8; 256];
            anchor(&mut m, 0x07, 0x9C, 0x22);
            m[0x30] = 9;
            let mut p = [false; 256];
            for (b, &v) in m.iter().enumerate() {
                p[b] = v != 255;
            }
            book.cast(Ballot::new(i, m, p));
        }
        let mut probe = [255u8; 256];
        anchor(&mut probe, 0x82, 0x11, 0x63);
        let probe = Ballot::new(999, probe, [true; 256]);

        let cfg = ConsensusConfig { min_ballots: 1, ..ConsensusConfig::default() };
        let unfiltered = book.resolve(&cfg);
        assert_eq!(unfiltered.map[0x30], 9, "unfiltered, the foreign majority wins");

        let filtered = book.resolve_for(&probe, &cfg);
        assert_eq!(filtered.map[0x30], 4, "filtered, only same-shuffle ballots count");
        assert_eq!(filtered.ballots, 3);
    }

    #[test]
    fn content_key_is_stable_and_discriminating() {
        assert_eq!(content_key(b"abc"), content_key(b"abc"));
        assert_ne!(content_key(b"abc"), content_key(b"abd"));
        assert_ne!(content_key(b""), content_key(b"\0"));
    }
}
