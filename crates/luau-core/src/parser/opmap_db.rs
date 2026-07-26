//! A database of opcode permutations that were *measured* rather than guessed,
//! and the lookup that decides whether one of them applies to a given chunk.
//!
//! # What an entry is
//!
//! One client build's permutation, produced by [`super::alignment`] from two
//! compilations of source we wrote ourselves. Not a detector output, not a
//! consensus tally, not a hand-written table — those all belong on the
//! inference path and must never be promoted in here.
//!
//! # Read-only at decode time
//!
//! The pooled consensus store (`--opmap-cache`) is written by every decompile,
//! which is right for a tally: more readings make it better. This database is
//! the opposite. It is written only by an explicit `import`, so no decompile,
//! however wrong, can ever poison it. That also keeps the file small enough to
//! read with human eyes, which is why it is JSON and not a binary blob.
//!
//! # Matching is falsification, and abstention is a result
//!
//! See [`super::fingerprint`] for why a chunk cannot simply be hashed. The
//! lookup proposes candidates and tries to disprove them; when it cannot tell
//! two candidates apart it says so and the caller falls back to inference.
//! Every outcome has a name, because "why did my file not get an exact map"
//! must be answerable.
//!
//! # The asymmetry that sets every threshold
//!
//! A miss costs nothing — the decompiler behaves exactly as it did before this
//! module existed. A wrong match installs 84 LOCKED wrong mappings and produces
//! output that looks perfect and is entirely fiction. Every gate below is
//! calibrated on that asymmetry, and every one of them is cheap.

use super::fingerprint::ChunkFingerprint;
use super::ground_truth::{opcode_name_to_byte, serialize_ground_truth};
use super::opcodes::LuauOpcode;
use super::opmap::{OpcodeMap, WalkVerdict};
use super::types::Chunk;
use std::collections::BTreeMap;
use std::path::Path;

pub const FORMAT_TAG: &str = "luau-opmap-db";
pub const FORMAT_VERSION: u32 = 1;

/// Anchor agreements required before an entry is even walk-tested.
///
/// Lower than `consensus`'s threshold of 3 looks wrong until you notice the
/// other half: here, ANY anchor disagreement rejects the candidate outright,
/// where `consensus` tolerates several. Agreement is what discriminates
/// (a collision costs ~1/256 per anchor); disagreement in `consensus` is mostly
/// detector noise, but this gate is applied to a *measured* map, where a
/// disagreement can only mean a different build.
///
/// Two is also exactly the number of anchors a short script reliably yields.
/// Raising it would silently exclude small files, and small files are not the
/// risk: the walk and corroboration gates below are what actually separate two
/// candidate builds, and they scale with how much of the chunk there is to
/// check.
pub const DB_MIN_ANCHOR_AGREE: u32 = 2;

/// Distinct opcode bytes a chunk must actually execute before an automatic
/// match is allowed. Below this there is too little for the walk to falsify,
/// and almost nothing to gain from getting it exactly right.
pub const DB_MIN_PRESENT_BYTES: usize = 6;

/// Raw agreements required between the chunk's own structural reading and a
/// candidate's permutation.
///
/// AGREEMENTS, not the net `agreements - 3 x conflicts` score. The net score
/// ranks candidates against each other and is almost always negative even for
/// the right answer, because solo structural detection is only around 60%
/// accurate — measured on real chunks, the TRUE build scored `agree=17
/// conflict=13` (net -22) and `agree=10 conflict=11` (net -23). An absolute
/// floor on the net score could never be met by anything.
///
/// Raw agreement separates the classes cleanly. Measured over 47 programs, each
/// tested against its own build's entry and against a foreign build's entry:
///
/// | | agreements | walked cleanly |
/// |---|---|---|
/// | true build  | 4 .. 18 (median 9) | 47/47 |
/// | foreign build | 0 .. 1 (median 0) | 0/47 |
///
/// Three is above every foreign observation by a factor of three and at or
/// below every true one. Note that the walk gate above was independently
/// perfect on this data — a foreign permutation could not decode a single
/// file — so this is a second, redundant line of defence rather than the only
/// one.
pub const DB_MIN_CORROBORATION: u32 = 3;

/// How far the best candidate must beat the runner-up on the net score. Below
/// this the answer is "I cannot tell", never "probably this one". Relative by
/// construction, which is the only way this score means anything.
pub const DB_SCORE_MARGIN: i32 = 6;

/// How the lifter should treat the three opcodes some clients repurpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnarySem {
    /// Drop the operator and pass the operand through. This is what the
    /// decompiler has always done for Roblox bytecode, and it stays the default
    /// so that any path which does not explicitly know better is unchanged.
    #[default]
    Passthrough,
    /// Lift a real unary operator.
    Operator,
}

impl UnarySem {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnarySem::Passthrough => "passthrough",
            UnarySem::Operator => "operator",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "passthrough" => Some(UnarySem::Passthrough),
            "operator" => Some(UnarySem::Operator),
            _ => None,
        }
    }
}

/// Per-opcode lifting semantics for the three slots clients are known to
/// repurpose (`NOT`, `MINUS`, `LENGTH`).
///
/// Deliberately three fields and not one "this map is exact" flag. An exact map
/// tells you which byte is `LENGTH`; it does not tell you that the client's
/// compiler emits that byte for `#x`. Only an observation does, and the probe
/// makes exactly that observation. Defaulting to `Passthrough` means a database
/// entry that does not record it leaves behaviour untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UnarySemantics {
    pub not: UnarySem,
    pub minus: UnarySem,
    pub length: UnarySem,
}

impl UnarySemantics {
    /// Canonical Luau, where all three really are operators.
    pub fn canonical() -> Self {
        UnarySemantics {
            not: UnarySem::Operator,
            minus: UnarySem::Operator,
            length: UnarySem::Operator,
        }
    }

    pub fn all_passthrough(&self) -> bool {
        *self == UnarySemantics::default()
    }
}

/// Where an entry came from. Mandatory, because it is the only thing standing
/// between a measured permutation and a hand-written one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// `"probe-align"` for a derived map; anything else is not measured.
    pub method: String,
    pub producer: Option<String>,
    pub probe_set_version: Option<u32>,
    pub probe_programs: Option<u32>,
    pub notes: Option<String>,
}

impl Provenance {
    pub fn is_measured(&self) -> bool {
        self.method == "probe-align"
    }
}

impl Default for Provenance {
    fn default() -> Self {
        Provenance {
            method: "unspecified".to_string(),
            producer: None,
            probe_set_version: None,
            probe_programs: None,
            notes: None,
        }
    }
}

/// One build's permutation.
#[derive(Debug, Clone)]
pub struct DbEntry {
    pub id: String,
    pub bytecode_version: u8,
    pub build_label: Option<String>,
    pub provenance: Provenance,
    pub semantics: UnarySemantics,
    /// shuffled byte -> internal opcode, 255 = unpinned.
    pub map: [u8; 256],
}

impl DbEntry {
    pub fn pinned(&self) -> usize {
        self.map.iter().filter(|&&v| v != 255).count()
    }

    /// Canonical opcodes this entry never pinned, by name.
    pub fn unpinned_names(&self) -> Vec<&'static str> {
        let mut have = [false; 256];
        for &v in self.map.iter() {
            if v != 255 {
                have[v as usize] = true;
            }
        }
        (0..super::alignment::CANONICAL_OPCODE_COUNT as u8)
            .filter_map(|c| {
                let internal = super::alignment::canonical_to_internal(c)?;
                if have[internal as usize] {
                    None
                } else {
                    super::alignment::canonical_opcode_name(c)
                }
            })
            .collect()
    }

    /// Is the map a partial bijection? Anything else was not derived by
    /// alignment and must not be treated as exact.
    pub fn is_bijective(&self) -> bool {
        super::alignment::is_partial_bijection(&self.map)
    }
}

/// The database.
#[derive(Debug, Clone, Default)]
pub struct OpmapDb {
    pub entries: Vec<DbEntry>,
}

/// What a lookup concluded. Abstention is as much a result as a hit.
#[derive(Debug, Clone)]
pub enum DbLookup {
    Hit {
        entry_id: String,
        map: [u8; 256],
        semantics: UnarySemantics,
        pinned: usize,
    },
    /// Nothing passed the header and anchor gates.
    NoCandidates { best_anchor_agree: u32 },
    /// Candidates passed the anchors but none could actually decode the chunk.
    AllFailedWalk {
        candidates: Vec<(String, WalkVerdict)>,
    },
    /// Two or more candidates fit and disagree. Refusing is the answer.
    Ambiguous {
        candidates: Vec<String>,
        best_score: i32,
        runner_up_score: i32,
    },
    /// An entry fits, but the chunk's own reading corroborates it too weakly
    /// to be sure. Distinct from `NoCandidates`, because "nothing looked like
    /// this build" and "something did but not convincingly" call for different
    /// next steps.
    LowConfidence { best_id: String, agreements: u32 },
    /// The chunk executes too few distinct opcodes to falsify anything.
    TooLittleEvidence { present_bytes: usize },
    /// Canonical Luau: there is no permutation to look up.
    NotShuffled,
    /// No database was configured, or it is empty.
    NoDatabase,
}

impl DbLookup {
    /// A one-line explanation, for the decompiled file's header.
    pub fn describe(&self) -> String {
        match self {
            DbLookup::Hit {
                entry_id, pinned, ..
            } => format!(
                "database entry \"{}\" - {} opcodes pinned exactly",
                entry_id, pinned
            ),
            DbLookup::NoCandidates { best_anchor_agree } => format!(
                "no entry matched (best anchor agreement was {})",
                best_anchor_agree
            ),
            DbLookup::AllFailedWalk { candidates } => format!(
                "{} candidate(s) passed the anchors but none could decode this chunk: {}",
                candidates.len(),
                candidates
                    .iter()
                    .map(|(id, v)| format!("{} ({})", id, v.describe()))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            DbLookup::Ambiguous {
                candidates,
                best_score,
                runner_up_score,
            } => format!(
                "refused: {} candidates fit and are too close to separate ({} vs {}): {}",
                candidates.len(),
                best_score,
                runner_up_score,
                candidates.join(", ")
            ),
            DbLookup::LowConfidence {
                best_id,
                agreements,
            } => format!(
                "refused: closest entry \"{}\" was corroborated on only {} bytes (floor is {})",
                best_id, agreements, DB_MIN_CORROBORATION
            ),
            DbLookup::TooLittleEvidence { present_bytes } => format!(
                "too little evidence: chunk executes only {} distinct opcodes",
                present_bytes
            ),
            DbLookup::NotShuffled => "chunk carries no opcode shuffle".to_string(),
            DbLookup::NoDatabase => "no database configured".to_string(),
        }
    }

    pub fn hit(&self) -> Option<(&str, &[u8; 256], UnarySemantics)> {
        match self {
            DbLookup::Hit {
                entry_id,
                map,
                semantics,
                ..
            } => Some((entry_id.as_str(), map, *semantics)),
            _ => None,
        }
    }
}

impl OpmapDb {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&DbEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Parse a database. Returns the database and any per-entry warnings.
    ///
    /// A malformed FILE or an unknown `format_version` is an error: a v1 reader
    /// must refuse a v2 file rather than half-read it. A malformed ENTRY is
    /// skipped with a warning, so one bad record cannot take the file down.
    pub fn parse(json: &str) -> anyhow::Result<(Self, Vec<String>)> {
        let root: serde_json::Value = serde_json::from_str(json)?;
        let obj = root
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("database root is not a JSON object"))?;

        if let Some(tag) = obj.get("format").and_then(|v| v.as_str()) {
            if tag != FORMAT_TAG {
                anyhow::bail!("not an opmap database (format is \"{}\")", tag);
            }
        }
        let version = obj
            .get("format_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(FORMAT_VERSION as u64);
        if version > FORMAT_VERSION as u64 {
            anyhow::bail!(
                "database format_version {} is newer than this build understands ({})",
                version,
                FORMAT_VERSION
            );
        }

        let mut warnings = Vec::new();
        let mut entries = Vec::new();
        let list = obj
            .get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for (i, raw) in list.iter().enumerate() {
            match parse_entry(raw) {
                Ok(e) => {
                    if entries.iter().any(|x: &DbEntry| x.id == e.id) {
                        warnings.push(format!("entry {}: duplicate id \"{}\", skipped", i, e.id));
                        continue;
                    }
                    entries.push(e);
                }
                Err(why) => warnings.push(format!("entry {}: {}, skipped", i, why)),
            }
        }
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok((OpmapDb { entries }, warnings))
    }

    pub fn load(path: &Path) -> anyhow::Result<(Self, Vec<String>)> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {}", path.display(), e))?;
        Self::parse(&text)
    }

    /// Load a database, treating a missing file as an empty one.
    pub fn load_or_empty(path: &Path) -> anyhow::Result<(Self, Vec<String>)> {
        if !path.exists() {
            return Ok((OpmapDb::default(), Vec::new()));
        }
        Self::load(path)
    }

    /// Stable, diffable output: entries sorted by id, mappings sorted by byte.
    pub fn to_json(&self) -> String {
        let entries: Vec<serde_json::Value> = self.entries.iter().map(entry_json).collect();
        let doc = serde_json::json!({
            "format": FORMAT_TAG,
            "format_version": FORMAT_VERSION,
            "entries": entries,
        });
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string()) + "\n"
    }

    /// Write via a temporary file and a rename, so an interrupted write cannot
    /// leave a half-database behind.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.to_json())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Add an entry, refusing anything that could not have come from a real
    /// derivation.
    pub fn insert(&mut self, entry: DbEntry, force: bool) -> anyhow::Result<()> {
        if entry.id.trim().is_empty() {
            anyhow::bail!("entry has no id");
        }
        if !entry.is_bijective() {
            anyhow::bail!(
                "entry \"{}\" maps two bytes to the same opcode - a permutation cannot do \
                 that, so this map was not derived by alignment",
                entry.id
            );
        }
        if entry.provenance.method.trim().is_empty()
            || entry.provenance.method == "unspecified"
        {
            anyhow::bail!(
                "entry \"{}\" has no provenance.method - every entry must say how it was \
                 produced",
                entry.id
            );
        }
        if !entry.semantics.all_passthrough() && !entry.provenance.is_measured() {
            anyhow::bail!(
                "entry \"{}\" declares unary semantics but was not produced by probe-align. \
                 Those fields change how the lifter reads NOT/MINUS/LENGTH and are only \
                 meaningful as a direct observation of the client's compiler.",
                entry.id
            );
        }
        if let Some(pos) = self.entries.iter().position(|e| e.id == entry.id) {
            if !force {
                anyhow::bail!("entry \"{}\" already exists (pass --force to replace)", entry.id);
            }
            self.entries[pos] = entry;
        } else {
            self.entries.push(entry);
        }
        self.entries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(())
    }

    /// Does any entry in this database apply to `chunk`?
    ///
    /// Pure: performs no I/O and installs nothing. See the module header for
    /// why each gate is where it is.
    pub fn lookup(&self, chunk: &Chunk) -> DbLookup {
        if self.entries.is_empty() {
            return DbLookup::NoDatabase;
        }
        let Some(fp) = ChunkFingerprint::from_chunk(chunk) else {
            return DbLookup::NotShuffled;
        };

        // Gate 1: header + anchors. Cheap, and a Tier-A disagreement is a
        // genuine "different build" signal rather than detector noise.
        let mut best_anchor_agree = 0u32;
        let mut candidates: Vec<&DbEntry> = Vec::new();
        for e in &self.entries {
            if e.bytecode_version != chunk.version {
                continue;
            }
            if fp.tier_a_conflicts(&e.map) > 0 {
                continue;
            }
            // Agreements gate, conflicts do NOT. Across the wider anchor set a
            // solo structural reading disagrees with the TRUE build on several
            // anchors as a matter of course - the weaker ones are only 80-95%
            // reliable. Rejecting on those would reject the right answer.
            // Agreement is what carries information: two unrelated builds
            // colliding on one costs about 1 in 256.
            let (agree, _conflict) = fp.anchor_agreements(&e.map);
            best_anchor_agree = best_anchor_agree.max(agree);
            if agree < DB_MIN_ANCHOR_AGREE {
                continue;
            }
            candidates.push(e);
        }
        if candidates.is_empty() {
            return DbLookup::NoCandidates { best_anchor_agree };
        }

        // Gate 2: can the candidate actually decode this chunk? A wrong
        // permutation usually either lacks a byte the chunk executes or
        // mis-skips an AUX word and runs off the end of a prototype.
        let mut fitted: Vec<(&DbEntry, usize)> = Vec::new();
        let mut failures: Vec<(String, WalkVerdict)> = Vec::new();
        for e in candidates {
            let report = OpcodeMap::walk_verify(chunk, &e.map);
            if report.verdict.is_clean() {
                fitted.push((e, report.present_bytes()));
            } else {
                failures.push((e.id.clone(), report.verdict));
            }
        }
        if fitted.is_empty() {
            return DbLookup::AllFailedWalk {
                candidates: failures,
            };
        }

        // Gate 3: enough of the chunk to be worth being exact about.
        let present = fitted.iter().map(|(_, p)| *p).max().unwrap_or(0);
        if present < DB_MIN_PRESENT_BYTES {
            return DbLookup::TooLittleEvidence {
                present_bytes: present,
            };
        }

        // Gate 4: corroborate against the chunk's own structural reading.
        // Ranked on the net score, floored on raw agreements - see
        // DB_MIN_CORROBORATION for why those have to be two different things.
        let mut scored: Vec<(&DbEntry, i32, u32)> = fitted
            .iter()
            .map(|(e, _)| {
                let (agree, conflict) = fp.corroboration(&e.map);
                (*e, agree as i32 - 3 * conflict as i32, agree)
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.id.cmp(&b.0.id)));

        let (best, best_score, best_agree) = scored[0];
        if best_agree < DB_MIN_CORROBORATION {
            return DbLookup::LowConfidence {
                best_id: best.id.clone(),
                agreements: best_agree,
            };
        }
        if scored.len() > 1 {
            let (runner_up, runner_score, _) = scored[1];
            if best_score - runner_score < DB_SCORE_MARGIN && runner_up.map != best.map {
                return DbLookup::Ambiguous {
                    candidates: scored.iter().map(|(e, _, _)| e.id.clone()).collect(),
                    best_score,
                    runner_up_score: runner_score,
                };
            }
        }

        DbLookup::Hit {
            entry_id: best.id.clone(),
            map: best.map,
            semantics: best.semantics,
            pinned: best.pinned(),
        }
    }

    /// Force a specific entry, still verifying that it fits.
    ///
    /// Verification failure is an error rather than a silent fallback: the
    /// caller named this entry and must be told it does not apply.
    pub fn lookup_by_id(&self, chunk: &Chunk, id: &str) -> anyhow::Result<DbLookup> {
        let entry = self
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("no database entry with id \"{}\"", id))?;
        if entry.bytecode_version != chunk.version {
            anyhow::bail!(
                "entry \"{}\" is for bytecode v{}, this chunk is v{}",
                id,
                entry.bytecode_version,
                chunk.version
            );
        }
        let report = OpcodeMap::walk_verify(chunk, &entry.map);
        if !report.verdict.is_clean() {
            anyhow::bail!(
                "entry \"{}\" cannot decode this chunk: {}",
                id,
                report.verdict.describe()
            );
        }
        Ok(DbLookup::Hit {
            entry_id: entry.id.clone(),
            map: entry.map,
            semantics: entry.semantics,
            pinned: entry.pinned(),
        })
    }
}

/// Read one entry out of JSON.
fn parse_entry(raw: &serde_json::Value) -> Result<DbEntry, String> {
    let obj = raw.as_object().ok_or("not an object")?;
    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("missing id")?
        .to_string();
    let bytecode_version = obj
        .get("bytecode_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(6) as u8;
    let build_label = obj
        .get("build")
        .and_then(|b| b.get("label"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let prov = obj.get("provenance").and_then(|v| v.as_object());
    let provenance = Provenance {
        method: prov
            .and_then(|p| p.get("method"))
            .and_then(|v| v.as_str())
            .unwrap_or("unspecified")
            .to_string(),
        producer: prov
            .and_then(|p| p.get("producer"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        probe_set_version: prov
            .and_then(|p| p.get("probe_set_version"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        probe_programs: prov
            .and_then(|p| p.get("probe_programs"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        notes: prov
            .and_then(|p| p.get("notes"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
    };

    let sem = obj.get("semantics").and_then(|v| v.as_object());
    let read_sem = |key: &str| -> UnarySem {
        sem.and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .and_then(UnarySem::parse)
            .unwrap_or_default()
    };
    let semantics = UnarySemantics {
        not: read_sem("unary_not"),
        minus: read_sem("unary_minus"),
        length: read_sem("unary_length"),
    };

    let mappings = obj
        .get("mappings")
        .and_then(|v| v.as_object())
        .ok_or("missing mappings")?;
    let mut map = [255u8; 256];
    let mut pinned = 0usize;
    for (k, v) in mappings {
        let Some(byte) = parse_hex_byte(k) else { continue };
        let Some(name) = v.as_str() else { continue };
        let Some(internal) = opcode_name_to_byte(name) else {
            continue;
        };
        map[byte as usize] = internal;
        pinned += 1;
    }
    if pinned == 0 {
        return Err("mappings are empty or unreadable".to_string());
    }

    Ok(DbEntry {
        id,
        bytecode_version,
        build_label,
        provenance,
        semantics,
        map,
    })
}

fn parse_hex_byte(s: &str) -> Option<u8> {
    let t = s.trim();
    let stripped = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u8::from_str_radix(stripped, 16).ok()
}

fn entry_json(e: &DbEntry) -> serde_json::Value {
    // Reuse the ground-truth serializer so there is one encoding of a map in
    // the crate, then splice it in as an object.
    let mappings: serde_json::Value = serde_json::from_str(&serialize_ground_truth(&e.map))
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));

    let mut prov = serde_json::Map::new();
    prov.insert(
        "method".into(),
        serde_json::Value::String(e.provenance.method.clone()),
    );
    if let Some(ref p) = e.provenance.producer {
        prov.insert("producer".into(), serde_json::Value::String(p.clone()));
    }
    if let Some(v) = e.provenance.probe_set_version {
        prov.insert("probe_set_version".into(), serde_json::json!(v));
    }
    if let Some(v) = e.provenance.probe_programs {
        prov.insert("probe_programs".into(), serde_json::json!(v));
    }
    if let Some(ref n) = e.provenance.notes {
        prov.insert("notes".into(), serde_json::Value::String(n.clone()));
    }

    let mut out = serde_json::Map::new();
    out.insert("id".into(), serde_json::Value::String(e.id.clone()));
    out.insert("bytecode_version".into(), serde_json::json!(e.bytecode_version));
    if let Some(ref label) = e.build_label {
        out.insert(
            "build".into(),
            serde_json::json!({ "label": label }),
        );
    }
    out.insert("provenance".into(), serde_json::Value::Object(prov));
    out.insert(
        "coverage".into(),
        serde_json::json!({
            "pinned": e.pinned(),
            "unpinned": e.unpinned_names(),
        }),
    );
    out.insert(
        "semantics".into(),
        serde_json::json!({
            "unary_not": e.semantics.not.as_str(),
            "unary_minus": e.semantics.minus.as_str(),
            "unary_length": e.semantics.length.as_str(),
        }),
    );
    out.insert("mappings".into(), mappings);
    serde_json::Value::Object(out)
}

/// Build an entry from a `probe align --out` document.
pub fn entry_from_probe_report(
    json: &str,
    id_override: Option<&str>,
    build_label: Option<&str>,
) -> anyhow::Result<DbEntry> {
    let root: serde_json::Value = serde_json::from_str(json)?;
    let obj = root
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("probe report root is not a JSON object"))?;

    // Accept a bare `{hex: NAME}` map too, so a hand-assembled file can be
    // imported — but such a file has no provenance and `insert` will refuse it
    // unless the caller supplies one.
    let mut wrapped = obj.clone();
    if !wrapped.contains_key("mappings") {
        wrapped = serde_json::Map::from_iter([(
            "mappings".to_string(),
            serde_json::Value::Object(obj.clone()),
        )]);
    }
    if let Some(id) = id_override {
        wrapped.insert("id".into(), serde_json::Value::String(id.to_string()));
    }
    if !wrapped.contains_key("id") {
        anyhow::bail!("probe report has no id and none was supplied");
    }
    if let Some(label) = build_label {
        wrapped.insert(
            "build".into(),
            serde_json::json!({ "label": label }),
        );
    }
    parse_entry(&serde_json::Value::Object(wrapped)).map_err(|e| anyhow::anyhow!(e))
}

/// A human-readable dump of one entry, for `opmap-db show`.
pub fn describe_entry(e: &DbEntry) -> String {
    let mut by_opcode: BTreeMap<&'static str, u8> = BTreeMap::new();
    for (b, &internal) in e.map.iter().enumerate() {
        if internal == 255 {
            continue;
        }
        by_opcode.insert(LuauOpcode::from_u8(internal).name(), b as u8);
    }
    let mut out = String::new();
    out.push_str(&format!("id                {}\n", e.id));
    out.push_str(&format!("bytecode version  {}\n", e.bytecode_version));
    if let Some(ref l) = e.build_label {
        out.push_str(&format!("build             {}\n", l));
    }
    out.push_str(&format!("provenance        {}\n", e.provenance.method));
    if let Some(ref p) = e.provenance.producer {
        out.push_str(&format!("produced by       {}\n", p));
    }
    out.push_str(&format!("pinned            {}\n", e.pinned()));
    out.push_str(&format!(
        "unary semantics   not={} minus={} length={}\n",
        e.semantics.not.as_str(),
        e.semantics.minus.as_str(),
        e.semantics.length.as_str()
    ));
    let unpinned = e.unpinned_names();
    if !unpinned.is_empty() {
        out.push_str(&format!("unpinned          {}\n", unpinned.join(" ")));
    }
    out.push('\n');
    for (name, byte) in by_opcode {
        out.push_str(&format!("  {:<16} 0x{:02X}\n", name, byte));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::test_fixtures as fx;

    fn entry_with(id: &str, perm: fn(u8) -> u8) -> DbEntry {
        DbEntry {
            id: id.to_string(),
            bytecode_version: 6,
            build_label: None,
            provenance: Provenance {
                method: "probe-align".to_string(),
                producer: Some("test".to_string()),
                probe_set_version: Some(1),
                probe_programs: Some(19),
                notes: None,
            },
            semantics: UnarySemantics::canonical(),
            map: fx::exact_map(perm),
        }
    }

    /// A real program, permuted: what a chunk from that build looks like.
    fn build_chunk(perm: fn(u8) -> u8) -> Chunk {
        fx::permute(&fx::canonical(fx::M04_MIRROR_FLOW), perm)
    }

    fn db(entries: Vec<DbEntry>) -> OpmapDb {
        OpmapDb { entries }
    }

    const MOVE: u8 = 6;
    const GETTABLEKS: u8 = 15;

    // -- format --

    #[test]
    fn write_read_write_is_byte_stable() {
        let d = db(vec![
            entry_with("build_a", fx::perm_a),
            entry_with("build_b", fx::perm_b),
        ]);
        let once = d.to_json();
        let (reparsed, warnings) = OpmapDb::parse(&once).expect("parses");
        assert!(warnings.is_empty(), "{:?}", warnings);
        assert_eq!(once, reparsed.to_json());
        assert_eq!(reparsed.entries.len(), 2);
        assert_eq!(reparsed.get("build_a").unwrap().map, fx::exact_map(fx::perm_a));
    }

    #[test]
    fn a_newer_format_version_is_refused_not_half_read() {
        let json = r#"{"format":"luau-opmap-db","format_version":99,"entries":[]}"#;
        let err = OpmapDb::parse(json).unwrap_err().to_string();
        assert!(err.contains("newer than this build"), "{}", err);
    }

    #[test]
    fn a_file_that_is_not_a_database_is_refused() {
        let json = r#"{"format":"something-else","entries":[]}"#;
        assert!(OpmapDb::parse(json).is_err());
        assert!(OpmapDb::parse("[1,2,3]").is_err());
        assert!(OpmapDb::parse("{{not json").is_err());
    }

    #[test]
    fn a_malformed_entry_is_skipped_and_its_siblings_survive() {
        let d = db(vec![entry_with("good", fx::perm_a)]);
        let mut v: serde_json::Value = serde_json::from_str(&d.to_json()).unwrap();
        v["entries"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "id": "bad" })); // no mappings
        let (parsed, warnings) = OpmapDb::parse(&v.to_string()).expect("file still readable");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].id, "good");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("mappings"));
    }

    #[test]
    fn a_non_bijective_map_is_refused() {
        let mut e = entry_with("broken", fx::perm_a);
        e.map[0x01] = MOVE;
        e.map[0x02] = MOVE;
        let mut d = OpmapDb::default();
        let err = d.insert(e, false).unwrap_err().to_string();
        assert!(err.contains("two bytes to the same opcode"), "{}", err);
    }

    #[test]
    fn duplicate_ids_need_force() {
        let mut d = OpmapDb::default();
        d.insert(entry_with("x", fx::perm_a), false).unwrap();
        assert!(d.insert(entry_with("x", fx::perm_b), false).is_err());
        d.insert(entry_with("x", fx::perm_b), true).expect("force replaces");
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.get("x").unwrap().map, fx::exact_map(fx::perm_b));
    }

    #[test]
    fn provenance_is_mandatory() {
        let mut e = entry_with("x", fx::perm_a);
        e.provenance = Provenance::default();
        e.semantics = UnarySemantics::default();
        let mut d = OpmapDb::default();
        assert!(d.insert(e, false).unwrap_err().to_string().contains("provenance"));
    }

    /// The one gate that protects the lifter change: unary semantics may only
    /// come from a real observation of the client's compiler.
    #[test]
    fn unary_semantics_require_a_measured_provenance() {
        let mut e = entry_with("hand_written", fx::perm_a);
        e.provenance.method = "manual".to_string();
        e.semantics = UnarySemantics::canonical();
        let mut d = OpmapDb::default();
        let err = d.insert(e, false).unwrap_err().to_string();
        assert!(err.contains("probe-align"), "{}", err);

        let mut e2 = entry_with("hand_written", fx::perm_a);
        e2.provenance.method = "manual".to_string();
        e2.semantics = UnarySemantics::default();
        d.insert(e2, false)
            .expect("a map with no semantics claim is importable");
    }

    #[test]
    fn roblox_extension_names_survive_a_round_trip() {
        let mut e = entry_with("ext", fx::perm_a);
        // Use a byte no canonical opcode occupies, so the map stays bijective.
        let free = (0..=255u8)
            .find(|&b| e.map[b as usize] == 255)
            .expect("a free byte exists");
        e.map[free as usize] = LuauOpcode::RbxExt101 as u8;
        let mut d = OpmapDb::default();
        d.insert(e, false).unwrap();
        let (back, _) = OpmapDb::parse(&d.to_json()).unwrap();
        assert_eq!(
            back.get("ext").unwrap().map[free as usize],
            LuauOpcode::RbxExt101 as u8
        );
    }

    // -- lookup --

    #[test]
    fn picks_the_entry_that_matches_the_chunk() {
        let d = db(vec![
            entry_with("build_a", fx::perm_a),
            entry_with("build_b", fx::perm_b),
        ]);
        match d.lookup(&build_chunk(fx::perm_a)) {
            DbLookup::Hit { ref entry_id, .. } => assert_eq!(entry_id, "build_a"),
            other => panic!("expected a hit, got {}", other.describe()),
        }
        match d.lookup(&build_chunk(fx::perm_b)) {
            DbLookup::Hit { ref entry_id, .. } => assert_eq!(entry_id, "build_b"),
            other => panic!("expected a hit, got {}", other.describe()),
        }
    }

    /// Stability: a different program from the same build must select the same
    /// entry. This is the property a plain hash key could never provide.
    #[test]
    fn a_different_program_from_one_build_selects_the_same_entry() {
        let d = db(vec![
            entry_with("build_a", fx::perm_a),
            entry_with("build_b", fx::perm_b),
        ]);
        let other = fx::permute(&fx::canonical(fx::M02_MIRROR_BRANCH), fx::perm_a);
        match d.lookup(&other) {
            DbLookup::Hit { ref entry_id, .. } => assert_eq!(entry_id, "build_a"),
            o => panic!("expected build_a, got {}", o.describe()),
        }
    }

    #[test]
    fn an_empty_database_changes_nothing() {
        let d = OpmapDb::default();
        assert!(matches!(d.lookup(&build_chunk(fx::perm_a)), DbLookup::NoDatabase));
    }

    #[test]
    fn a_chunk_from_an_unknown_build_gets_no_match() {
        let d = db(vec![
            entry_with("build_a", fx::perm_a),
            entry_with("build_b", fx::perm_b),
        ]);
        let r = d.lookup(&build_chunk(fx::perm_c));
        assert!(
            !matches!(r, DbLookup::Hit { .. }),
            "must not match a build it has never seen: {}",
            r.describe()
        );
    }

    #[test]
    fn a_wrong_bytecode_version_is_never_matched() {
        let mut e = entry_with("v5_build", fx::perm_a);
        e.bytecode_version = 5;
        let d = db(vec![e]);
        let r = d.lookup(&build_chunk(fx::perm_a));
        assert!(matches!(r, DbLookup::NoCandidates { .. }), "{}", r.describe());
    }

    #[test]
    fn two_entries_with_identical_maps_do_not_cause_an_abstention() {
        let d = db(vec![
            entry_with("dup_one", fx::perm_a),
            entry_with("dup_two", fx::perm_a),
        ]);
        match d.lookup(&build_chunk(fx::perm_a)) {
            DbLookup::Hit { ref entry_id, .. } => assert_eq!(entry_id, "dup_one"),
            other => panic!("expected a hit, got {}", other.describe()),
        }
    }

    #[test]
    fn a_tiny_chunk_abstains_rather_than_guessing() {
        let mut tiny = build_chunk(fx::perm_a);
        tiny.protos.truncate(1);
        tiny.protos[0].code.truncate(2);
        tiny.main_proto = 0;
        let d = db(vec![entry_with("build_a", fx::perm_a)]);
        let r = d.lookup(&tiny);
        assert!(
            !matches!(r, DbLookup::Hit { .. }),
            "a two-instruction chunk must not authorise an exact decode: {}",
            r.describe()
        );
    }

    #[test]
    fn canonical_bytecode_is_never_matched() {
        let c = fx::canonical(fx::M04_MIRROR_FLOW);
        let d = db(vec![entry_with("build_a", fx::perm_a)]);
        assert!(matches!(d.lookup(&c), DbLookup::NotShuffled));
    }

    #[test]
    fn walk_failure_is_reported_with_the_reason() {
        let mut e = entry_with("holey", fx::perm_a);
        e.map[fx::perm_a(GETTABLEKS) as usize] = 255;
        let d = db(vec![e]);
        match d.lookup(&build_chunk(fx::perm_a)) {
            DbLookup::AllFailedWalk { ref candidates } => {
                assert_eq!(candidates.len(), 1);
                assert!(matches!(candidates[0].1, WalkVerdict::UnmappedByte { .. }));
            }
            other => panic!("expected a walk failure, got {}", other.describe()),
        }
    }

    #[test]
    fn lookup_by_id_errors_instead_of_falling_back() {
        let d = db(vec![
            entry_with("build_a", fx::perm_a),
            entry_with("build_b", fx::perm_b),
        ]);
        let err = d
            .lookup_by_id(&build_chunk(fx::perm_a), "build_b")
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot decode"), "{}", err);
        assert!(d.lookup_by_id(&build_chunk(fx::perm_a), "build_a").is_ok());
        assert!(d.lookup_by_id(&build_chunk(fx::perm_a), "nonexistent").is_err());
    }

    /// A hit must carry the entry's semantics through unchanged, because that
    /// is what authorises the lifter to stop passing NOT/MINUS/LENGTH through.
    #[test]
    fn a_hit_carries_the_entrys_semantics() {
        let mut e = entry_with("build_a", fx::perm_a);
        e.semantics = UnarySemantics {
            not: UnarySem::Operator,
            minus: UnarySem::Passthrough,
            length: UnarySem::Operator,
        };
        let d = db(vec![e]);
        match d.lookup(&build_chunk(fx::perm_a)) {
            DbLookup::Hit { semantics, .. } => {
                assert_eq!(semantics.not, UnarySem::Operator);
                assert_eq!(semantics.minus, UnarySem::Passthrough);
                assert_eq!(semantics.length, UnarySem::Operator);
            }
            o => panic!("{}", o.describe()),
        }
    }

    #[test]
    fn every_outcome_explains_itself() {
        let outcomes = [
            DbLookup::NoDatabase,
            DbLookup::NotShuffled,
            DbLookup::NoCandidates { best_anchor_agree: 2 },
            DbLookup::TooLittleEvidence { present_bytes: 3 },
            DbLookup::LowConfidence { best_id: "x".into(), agreements: 1 },
            DbLookup::AllFailedWalk {
                candidates: vec![("x".into(), WalkVerdict::OverranProto { proto: 0 })],
            },
            DbLookup::Ambiguous {
                candidates: vec!["a".into(), "b".into()],
                best_score: 10,
                runner_up_score: 9,
            },
        ];
        for o in outcomes {
            assert!(!o.describe().is_empty());
            assert!(o.hit().is_none());
        }
    }

    // -- semantics defaults --

    #[test]
    fn unary_semantics_default_to_todays_behaviour() {
        let d = UnarySemantics::default();
        assert_eq!(d.not, UnarySem::Passthrough);
        assert_eq!(d.minus, UnarySem::Passthrough);
        assert_eq!(d.length, UnarySem::Passthrough);
        assert!(d.all_passthrough());
        assert!(!UnarySemantics::canonical().all_passthrough());
    }

    #[test]
    fn an_entry_without_a_semantics_block_stays_passthrough() {
        let json = serde_json::json!({
            "format": FORMAT_TAG,
            "format_version": FORMAT_VERSION,
            "entries": [{
                "id": "no_semantics",
                "bytecode_version": 6,
                "provenance": { "method": "probe-align" },
                "mappings": { "0x10": "MOVE", "0x11": "RETURN" }
            }]
        });
        let (d, _) = OpmapDb::parse(&json.to_string()).unwrap();
        assert!(d.get("no_semantics").unwrap().semantics.all_passthrough());
    }

    #[test]
    fn probe_report_imports_with_its_provenance_intact() {
        let report = serde_json::json!({
            "format": "luau-opmap-probe",
            "id": "seed42",
            "provenance": { "method": "probe-align", "probe_programs": 19 },
            "semantics": { "unary_not": "operator", "unary_minus": "operator",
                           "unary_length": "operator" },
            "mappings": { "0x10": "MOVE", "0x11": "RETURN", "0x12": "ADD" }
        });
        let e = entry_from_probe_report(&report.to_string(), None, Some("label")).unwrap();
        assert_eq!(e.id, "seed42");
        assert!(e.provenance.is_measured());
        assert_eq!(e.provenance.probe_programs, Some(19));
        assert_eq!(e.build_label.as_deref(), Some("label"));
        assert_eq!(e.semantics, UnarySemantics::canonical());
        assert_eq!(e.pinned(), 3);
        let mut d = OpmapDb::default();
        d.insert(e, false).expect("a measured report imports");
    }

    #[test]
    fn a_bare_hex_map_imports_but_carries_no_provenance() {
        let bare = r#"{ "0x10": "MOVE", "0x11": "RETURN" }"#;
        let e = entry_from_probe_report(bare, Some("hand"), None).unwrap();
        assert_eq!(e.id, "hand");
        assert!(!e.provenance.is_measured());
        let mut d = OpmapDb::default();
        assert!(
            d.insert(e, false).is_err(),
            "a map with no stated provenance must not enter the database silently"
        );
    }

    #[test]
    fn describe_entry_names_bytes_and_gaps() {
        let mut e = entry_with("shown", fx::perm_a);
        e.map[fx::perm_a(MOVE) as usize] = 255;
        let text = describe_entry(&e);
        assert!(text.contains("id                shown"));
        assert!(text.contains("RETURN"));
        assert!(text.contains("MOVE"), "unpinned list should still name MOVE");
    }
}
