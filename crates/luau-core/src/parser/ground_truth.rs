//! Ground-truth opcode mappings supplied by the an executor probe script.
//!
//! The an executor executor can call `lift_closure(loadstring(known_source))` to
//! compile a script whose bytecode we already know the canonical meaning of,
//! then diff the shuffled-byte stream against the canonical one byte-by-byte
//! at each instruction position. That yields a ground-truth `shuffled_byte ->
//! canonical_opcode` table that is *strictly* more authoritative than any
//! heuristic detector, so it is applied before detection and LOCKED.
//!
//! Storage format (JSON):
//!   { "0x48": "JUMPBACK", "0x6E": "FORGLOOP", ... }
//!
//! Only keys of the form "0xNN" (hex byte) are accepted. Only canonical
//! opcode names from [`LuauOpcode::name`] are accepted.

use super::opcodes::LuauOpcode;
use std::collections::HashMap;
use std::path::Path;

/// Parse a JSON blob into a `[u8; 256]` ground-truth map (255 = unmapped).
///
/// Malformed entries are silently skipped — the file is written by a live
/// probe script and partial data is still useful. Returns `None` only if the
/// JSON itself fails to parse. Accepts either a flat `{hex: name}` map or
/// a richer `{"mappings": {hex: name}, ...}` envelope for forward
/// compatibility.
pub fn parse_ground_truth_json(json: &str) -> Option<[u8; 256]> {
    // Accept both shapes: flat map, or envelope with "mappings" key.
    let flat: Result<HashMap<String, String>, _> = serde_json::from_str(json);
    let map_entries: HashMap<String, String> = if let Ok(m) = flat {
        m
    } else {
        // Try envelope form.
        #[derive(serde::Deserialize)]
        struct Envelope {
            #[serde(default)]
            mappings: HashMap<String, String>,
        }
        let env: Envelope = serde_json::from_str(json).ok()?;
        env.mappings
    };

    let mut map = [255u8; 256];
    for (k, v) in map_entries {
        let Some(byte) = parse_hex_byte(&k) else { continue };
        let Some(canon) = opcode_name_to_byte(&v) else { continue };
        map[byte as usize] = canon;
    }
    Some(map)
}

/// Parse "0xNN" / "0xnn" / "NN" as a u8.
fn parse_hex_byte(s: &str) -> Option<u8> {
    let t = s.trim();
    let stripped = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u8::from_str_radix(stripped, 16).ok()
}

/// Map a canonical opcode name (uppercase, as produced by
/// [`LuauOpcode::name`]) back to its canonical byte value. Returns `None`
/// for `"UNKNOWN"` and for anything unrecognised.
pub fn opcode_name_to_byte(name: &str) -> Option<u8> {
    let up = name.trim().to_ascii_uppercase();
    if up == "UNKNOWN" {
        return None;
    }
    for b in 0u16..LuauOpcode::MAX_OPCODE as u16 {
        let op = LuauOpcode::from_u8(b as u8);
        if op == LuauOpcode::Unknown {
            continue;
        }
        if op.name() == up.as_str() {
            return Some(b as u8);
        }
    }
    None
}

/// Load a ground-truth map from disk. Missing files return `None`.
pub fn load_ground_truth(path: &Path) -> Option<[u8; 256]> {
    let data = std::fs::read_to_string(path).ok()?;
    parse_ground_truth_json(&data)
}

/// Serialize a `[u8; 256]` ground-truth map back to pretty JSON.
pub fn serialize_ground_truth(map: &[u8; 256]) -> String {
    // Sort by shuffled byte for deterministic output.
    let mut entries: Vec<(u8, u8)> = map
        .iter()
        .enumerate()
        .filter(|(_, &v)| v != 255)
        .map(|(i, &v)| (i as u8, v))
        .collect();
    entries.sort_by_key(|&(s, _)| s);

    let mut obj = serde_json::Map::new();
    for (shuffled, canon) in entries {
        let name = LuauOpcode::from_u8(canon).name();
        obj.insert(
            format!("0x{:02X}", shuffled),
            serde_json::Value::String(name.to_string()),
        );
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(obj))
        .unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flat_json_roundtrip() {
        let json = r#"{ "0x48": "JUMPBACK", "0x6E": "FORGLOOP" }"#;
        let map = parse_ground_truth_json(json).expect("parse");
        assert_eq!(map[0x48], LuauOpcode::JumpBack as u8);
        assert_eq!(map[0x6E], LuauOpcode::ForGLoop as u8);
        assert_eq!(map[0x00], 255, "unfilled slots stay at sentinel");
    }

    #[test]
    fn parse_envelope_json() {
        let json = r#"{ "mappings": { "0x21": "CALL" }, "client_version": "v123" }"#;
        let map = parse_ground_truth_json(json).expect("parse");
        assert_eq!(map[0x21], LuauOpcode::Call as u8);
    }

    #[test]
    fn skip_unknown_opcode_names() {
        let json = r#"{ "0x21": "CALL", "0x22": "NOTAREALOP", "0x23": "UNKNOWN" }"#;
        let map = parse_ground_truth_json(json).expect("parse");
        assert_eq!(map[0x21], LuauOpcode::Call as u8);
        assert_eq!(map[0x22], 255, "bad name skipped");
        assert_eq!(map[0x23], 255, "UNKNOWN skipped");
    }

    #[test]
    fn accept_raw_hex_without_prefix() {
        let json = r#"{ "48": "JUMPBACK" }"#;
        let map = parse_ground_truth_json(json).expect("parse");
        assert_eq!(map[0x48], LuauOpcode::JumpBack as u8);
    }

    #[test]
    fn serialize_roundtrip() {
        let mut m = [255u8; 256];
        m[0x48] = LuauOpcode::JumpBack as u8;
        m[0x21] = LuauOpcode::Call as u8;
        let json = serialize_ground_truth(&m);
        let parsed = parse_ground_truth_json(&json).expect("parse");
        assert_eq!(parsed, m);
    }

    #[test]
    fn malformed_json_returns_none() {
        assert!(parse_ground_truth_json("{{not json").is_none());
    }

    #[test]
    fn opcode_name_to_byte_known() {
        assert_eq!(opcode_name_to_byte("CALL"), Some(LuauOpcode::Call as u8));
        assert_eq!(
            opcode_name_to_byte("forgloop"),
            Some(LuauOpcode::ForGLoop as u8)
        );
        assert_eq!(opcode_name_to_byte(" JUMPBACK "), Some(24));
        assert_eq!(opcode_name_to_byte("NOTAREALOP"), None);
        assert_eq!(opcode_name_to_byte("UNKNOWN"), None);
    }
}
