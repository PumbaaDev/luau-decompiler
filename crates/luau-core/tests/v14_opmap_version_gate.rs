//! Live v14 chunks must match the measured opcode database.
//!
//! Roblox live v14 uses the same byte permutation as live v12 (verified by raw
//! byte reads: 0x52 MOVE, 0x6F LOADK, 0xBC NAMECALL, 0x9F CALL, 0x25 CALLFB,
//! 0xFB GETUPVAL, 0xD9 NEWCLOSURE, 0x12 CAPTURE) plus 0xEB = NOP. The database
//! used to hold only a v12 entry, and `lookup` rejects an entry measured at an
//! older bytecode version than the chunk, so every v14 script fell to the
//! per-file detector (40+ opcodes mis-assigned, AUX words walked as
//! instructions) or, in the library's default mode, was refused outright.
//!
//! The data test needs no game files. The decode test reads a real live v14
//! chunk from `LUAU_V14_LIVE_DIR` (default `<repo>/goal/v14_live`) and skips
//! when it is absent, as the other corpus-backed tests do.

use std::path::PathBuf;

const SMALL_V14: &str = "Shared.Modules.TrainEventSchedule.luac";

#[test]
fn bundled_database_has_a_measured_entry_for_v14() {
    let db = luau_core::bundled_opmap_db().expect("bundled opmap database must parse");
    let v14 = db
        .entries
        .iter()
        .find(|e| e.bytecode_version == 14)
        .expect("the bundled database must carry a measured v14 entry");
    assert!(v14.provenance.is_measured(), "the v14 entry must be a measured one");
    // Every byte the live v12 entry pins is pinned identically in v14.
    let v12 = db.entries.iter().find(|e| e.id == "roblox_v12_live").expect("v12 entry");
    for (byte, (&a, &b)) in v12.map.iter().zip(v14.map.iter()).enumerate() {
        if a != 255 {
            assert_eq!(a, b, "byte {byte:#04x} differs between the v12 and v14 entries");
        }
    }
}

#[test]
fn live_v14_chunk_decodes_in_the_default_library_mode() {
    let dir = std::env::var_os("LUAU_V14_LIVE_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../goal/v14_live")
    });
    let path = dir.join(SMALL_V14);
    let Ok(bc) = std::fs::read(&path) else {
        eprintln!("SKIP: {} not found", path.display());
        return;
    };
    assert_eq!(bc[0], 14, "fixture must be bytecode version 14");
    let src = luau_core::decompile(&bc)
        .unwrap_or_else(|e| panic!("live v14 must decode from the measured entry: {e:#}"));
    let lines = src.lines().count();
    // Potassium's decompile of this script is 35 lines; the detector garbage was 7.
    assert!(
        (25..=60).contains(&lines),
        "expected a structurally complete decode (~35 lines), got {lines}:\n{src}"
    );
}
