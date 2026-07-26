pub mod alignment;
pub mod consensus;
pub mod fingerprint;
pub mod ground_truth;
pub mod known_shuffles;
pub mod opcodes;
pub mod opmap;
pub mod opmap_db;
pub mod probe;
#[cfg(test)]
pub mod test_fixtures;
pub mod types;

use anyhow::{bail, Context, Result};
use types::*;

/// Main entry point: parse raw Luau bytecode into a Chunk
pub fn parse(data: &[u8]) -> Result<Chunk> {
    let mut reader = BytecodeReader::new(data);
    reader.read_chunk()
}

/// Low-level bytecode reader with cursor tracking
struct BytecodeReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> BytecodeReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn read_byte(&mut self) -> Result<u8> {
        if self.offset >= self.data.len() {
            bail!("unexpected end of bytecode at offset {}", self.offset);
        }
        let b = self.data[self.offset];
        self.offset += 1;
        Ok(b)
    }

    fn read_u32(&mut self) -> Result<u32> {
        if self.offset + 4 > self.data.len() {
            bail!("unexpected end of bytecode reading u32 at offset {}", self.offset);
        }
        let val = u32::from_le_bytes([
            self.data[self.offset],
            self.data[self.offset + 1],
            self.data[self.offset + 2],
            self.data[self.offset + 3],
        ]);
        self.offset += 4;
        Ok(val)
    }

    fn read_f64(&mut self) -> Result<f64> {
        if self.offset + 8 > self.data.len() {
            bail!("unexpected end of bytecode reading f64 at offset {}", self.offset);
        }
        let val = f64::from_le_bytes([
            self.data[self.offset],
            self.data[self.offset + 1],
            self.data[self.offset + 2],
            self.data[self.offset + 3],
            self.data[self.offset + 4],
            self.data[self.offset + 5],
            self.data[self.offset + 6],
            self.data[self.offset + 7],
        ]);
        self.offset += 8;
        Ok(val)
    }

    fn read_f32(&mut self) -> Result<f32> {
        if self.offset + 4 > self.data.len() {
            bail!("unexpected end of bytecode reading f32 at offset {}", self.offset);
        }
        let val = f32::from_le_bytes([
            self.data[self.offset],
            self.data[self.offset + 1],
            self.data[self.offset + 2],
            self.data[self.offset + 3],
        ]);
        self.offset += 4;
        Ok(val)
    }

    /// Read a variable-length integer (LEB128-style encoding used by Luau)
    fn read_varint(&mut self) -> Result<u32> {
        let mut result: u32 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.read_byte()?;
            result |= ((byte & 0x7F) as u32) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 35 {
                bail!("varint too large at offset {}", self.offset);
            }
        }
        Ok(result)
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.offset + n > self.data.len() {
            bail!(
                "unexpected end of bytecode reading {} bytes at offset {}",
                n,
                self.offset
            );
        }
        let slice = &self.data[self.offset..self.offset + n];
        self.offset += n;
        Ok(slice)
    }

    /// Read the string table used by the bytecode
    fn read_string_table(&mut self) -> Result<Vec<String>> {
        let count = self.read_varint()? as usize;
        let mut strings = Vec::with_capacity(count);
        for _ in 0..count {
            let len = self.read_varint()? as usize;
            let bytes = self.read_bytes(len)?;
            // Luau strings can contain arbitrary bytes, but we try UTF-8
            let s = String::from_utf8_lossy(bytes).into_owned();
            strings.push(s);
        }
        Ok(strings)
    }

    /// Read a string reference (index into string table, 0 = none)
    fn read_string_ref(&mut self, strings: &[String]) -> Result<Option<String>> {
        let idx = self.read_varint()? as usize;
        if idx == 0 {
            Ok(None)
        } else {
            Ok(Some(
                strings
                    .get(idx - 1)
                    .cloned()
                    .unwrap_or_else(|| format!("<invalid_string_{}>", idx)),
            ))
        }
    }

    /// Read a single constant from the constant table
    fn read_constant(&mut self, strings: &[String], version: u8) -> Result<Constant> {
        let tag = self.read_byte()?;
        match tag {
            0 => Ok(Constant::Nil),
            1 => {
                let val = self.read_byte()?;
                Ok(Constant::Boolean(val != 0))
            }
            2 => {
                let val = self.read_f64()?;
                Ok(Constant::Number(val))
            }
            3 => {
                // String reference
                let idx = self.read_varint()? as usize;
                let s = strings
                    .get(idx.wrapping_sub(1))
                    .cloned()
                    .unwrap_or_else(|| format!("<invalid_string_{}>", idx));
                Ok(Constant::String(s))
            }
            4 => {
                // Import - encoded as a u32 with packed path
                let val = self.read_u32()?;
                Ok(Constant::Import(val))
            }
            5 => {
                // LBC_CONSTANT_TABLE — basic form: just a list of key constant
                // indices. Each field is initialized to nil / 0.0 at runtime,
                // and SETTABLEKS instructions fill in the values afterwards.
                let len = self.read_varint()? as usize;
                let mut entries = Vec::with_capacity(len);
                for _ in 0..len {
                    let key = self.read_varint()? as i32;
                    entries.push((key, None));
                }
                Ok(Constant::Table(entries))
            }
            6 => {
                // Closure - proto index
                let idx = self.read_varint()?;
                Ok(Constant::Closure(idx))
            }
            7 if version >= 5 => {
                // Vector constant (4 floats, but typically 3 used + padding)
                let x = self.read_f32()?;
                let y = self.read_f32()?;
                let z = self.read_f32()?;
                let w = self.read_f32()?;
                Ok(Constant::Vector(x, y, z, w))
            }
            8 if version >= 7 => {
                // LBC_CONSTANT_TABLE_WITH_CONSTANTS (bytecode v7+):
                // Compile-time-initialized table template. Per entry the
                // compiler writes a varint key constant index followed by
                // a *fixed* little-endian int32 value constant index
                // (or -1 for nil). The Luau VM code is in lvmload.cpp:
                //   int key = readVarInt(...);
                //   int32_t constantIdx = read<int32_t>(...);
                // Without handling the int32, the parser advances by the
                // wrong number of bytes and every subsequent constant /
                // proto member is misaligned — and all named-field table
                // literals decompile as empty `{}`.
                let len = self.read_varint()? as usize;
                let mut entries = Vec::with_capacity(len);
                for _ in 0..len {
                    let key = self.read_varint()? as i32;
                    let constant_idx = self.read_u32()? as i32; // int32, -1 == nil
                    let value = if constant_idx >= 0 { Some(constant_idx) } else { None };
                    entries.push((key, value));
                }
                Ok(Constant::Table(entries))
            }
            _ => {
                bail!(
                    "unknown constant tag {} at offset {} (version {})",
                    tag,
                    self.offset,
                    version
                );
            }
        }
    }

    /// Read a single function prototype
    fn read_proto(&mut self, strings: &[String], version: u8) -> Result<Proto> {
        let max_stack_size = self.read_byte()?;
        let num_params = self.read_byte()?;
        let num_upvalues = self.read_byte()?;
        let is_vararg = self.read_byte()? != 0;

        // Version 4+ has flags
        let flags = if version >= 4 {
            self.read_byte()?
        } else {
            0
        };

        // Type info (version 4+ with types)
        let typeinfo = if version >= 4 {
            let typesize = self.read_varint()? as usize;
            if typesize > 0 {
                Some(self.read_bytes(typesize)?.to_vec())
            } else {
                None
            }
        } else {
            None
        };

        // Instructions
        let size_code = self.read_varint()? as usize;
        let mut code = Vec::with_capacity(size_code);
        for _ in 0..size_code {
            code.push(self.read_u32()?);
        }

        // Constants
        let size_k = self.read_varint()? as usize;
        let mut constants = Vec::with_capacity(size_k);
        for _ in 0..size_k {
            constants.push(self.read_constant(strings, version)?);
        }

        // Child proto references
        let size_p = self.read_varint()? as usize;
        let mut child_protos = Vec::with_capacity(size_p);
        for _ in 0..size_p {
            child_protos.push(self.read_varint()?);
        }

        // Line defined
        let line_defined = self.read_varint()?;

        // Debug name
        let debug_name = self.read_string_ref(strings)?;

        // Line info (optional)
        let line_info = if self.read_byte()? != 0 {
            let linegaplog2 = self.read_byte()?;
            if linegaplog2 >= 32 {
                anyhow::bail!("invalid linegaplog2 value {} (must be < 32)", linegaplog2);
            }

            // Read line offsets (one per instruction, encoded as deltas)
            let mut intervals = Vec::with_capacity(size_code);
            for _ in 0..size_code {
                intervals.push(self.read_byte()? as i8 as i32);
            }

            // Read absolute line positions at intervals
            let interval_size = ((size_code - 1) >> linegaplog2 as usize) + 1;
            let mut abs_lines = Vec::with_capacity(interval_size);
            for _ in 0..interval_size {
                abs_lines.push(self.read_u32()? as i32);
            }

            // Reconstruct full line info
            let mut lines = Vec::with_capacity(size_code);
            let gap = 1usize << linegaplog2 as usize;
            let mut current_line = 0i32;
            for i in 0..size_code {
                if i % gap == 0 {
                    current_line = abs_lines[i / gap];
                }
                current_line += intervals[i];
                lines.push(current_line);
            }

            Some(LineInfo {
                line_gap_log2: linegaplog2,
                lines,
            })
        } else {
            None
        };

        // Debug info (optional)
        let debug_info = if self.read_byte()? != 0 {
            // Local variables
            let size_locals = self.read_varint()? as usize;
            let mut locals = Vec::with_capacity(size_locals);
            for _ in 0..size_locals {
                let name = self.read_string_ref(strings)?.unwrap_or_default();
                let start_pc = self.read_varint()?;
                let end_pc = self.read_varint()?;
                let reg = self.read_byte()?;
                locals.push(LocalVar {
                    name,
                    start_pc,
                    end_pc,
                    reg,
                });
            }

            // Upvalue names
            let size_upvalues = self.read_varint()? as usize;
            let mut upvalue_names = Vec::with_capacity(size_upvalues);
            for _ in 0..size_upvalues {
                upvalue_names
                    .push(self.read_string_ref(strings)?.unwrap_or_default());
            }

            Some(DebugInfo {
                locals,
                upvalue_names,
            })
        } else {
            None
        };

        Ok(Proto {
            max_stack_size,
            num_params,
            num_upvalues,
            is_vararg,
            flags,
            typeinfo,
            code,
            constants,
            child_protos,
            line_defined,
            debug_name,
            line_info,
            debug_info,
        })
    }

    /// Read the entire bytecode chunk
    fn read_chunk(&mut self) -> Result<Chunk> {
        // Version byte
        let version = self.read_byte().context("reading version byte")?;

        // Version 0 means the bytecode has a compilation error message
        if version == 0 {
            let len = self.read_varint()?;
            let msg = self.read_bytes(len as usize)?;
            let error_msg = String::from_utf8_lossy(msg);
            bail!("bytecode contains compilation error: {}", error_msg);
        }

        if !(3..=8).contains(&version) {
            bail!(
                "unsupported bytecode version {} (expected 3-8)",
                version
            );
        }

        log::debug!("Bytecode version: {}", version);

        // Types encoding version (version 4+)
        let types_version = if version >= 4 {
            self.read_byte()?
        } else {
            0
        };
        log::debug!("Types encoding version: {}", types_version);

        // String table
        let strings = self.read_string_table().context("reading string table")?;
        log::debug!("String table: {} entries", strings.len());

        // Userdata type remapping (version 5+? - skip if present)
        if version >= 5 {
            loop {
                let idx = self.read_byte()?;
                if idx == 0 {
                    break;
                }
                // Read and discard the string ref for this userdata type
                let _name = self.read_varint()?;
            }
        }

        // Function prototypes
        let proto_count = self.read_varint()? as usize;
        log::debug!("Proto count: {}", proto_count);

        let mut protos = Vec::with_capacity(proto_count);
        for i in 0..proto_count {
            let proto = self
                .read_proto(&strings, version)
                .with_context(|| format!("reading proto {}", i))?;
            protos.push(proto);
        }

        // Main function index
        let main_proto = self.read_varint()?;
        log::debug!("Main proto index: {}", main_proto);

        Ok(Chunk {
            version,
            types_version,
            strings,
            protos,
            main_proto,
        })
    }
}
