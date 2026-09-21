//! C ABI (FFI) surface for the Luau decompiler.
//!
//! This crate wraps the safe Rust API of [`luau_core`] in a small, stable
//! `extern "C"` layer so the decompiler can be called from C, C++, or any
//! language with a C FFI (Python `ctypes`/`cffi`, Go `cgo`, Node N-API, etc.).
//!
//! # Design rules
//!
//! Every exported function follows the same contract:
//!
//! * **Null and size guards.** A null input pointer (with a non-zero length),
//!   a null output pointer, or an implausibly large length is rejected before
//!   any work happens. A zero length is treated as an empty input, not a fault.
//! * **No unwinding across the boundary.** All Rust work runs inside
//!   [`std::panic::catch_unwind`]; a panic is converted into an error code
//!   instead of unwinding into C (which would be undefined behaviour).
//! * **Heap strings out, explicit free.** String results are returned as a
//!   heap-allocated, NUL-terminated C string via [`CString::into_raw`]. The
//!   caller owns it and must return it with [`luau_string_free`]; freeing it
//!   with libc `free` is undefined behaviour because the allocators differ.
//! * **No global mutable state** except a thread-local "last error" buffer.
//!
//! # Result codes
//!
//! Functions that produce a string return `0` ([`LUAU_OK`]) on success and a
//! non-zero code on failure; see the `LUAU_ERR_*` constants. On any non-zero
//! return, [`luau_last_error`] yields a human-readable description for the
//! calling thread.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::slice;
use std::sync::OnceLock;

/// Success.
pub const LUAU_OK: c_int = 0;
/// A required pointer argument was null (or a non-zero length with a null
/// data pointer).
pub const LUAU_ERR_NULL_ARG: c_int = 1;
/// The input length exceeded [`MAX_INPUT_LEN`].
pub const LUAU_ERR_INPUT_TOO_LARGE: c_int = 2;
/// The bytecode could not be parsed/decompiled (a normal, expected failure for
/// malformed or unsupported input).
pub const LUAU_ERR_DECODE: c_int = 3;
/// A panic was caught inside the library. This indicates a bug; the boundary
/// stays intact and no unwinding escapes into the caller.
pub const LUAU_ERR_PANIC: c_int = 4;
/// The produced text contained an interior NUL byte and cannot be represented
/// as a C string. Not expected for real decompiler output.
pub const LUAU_ERR_ENCODING: c_int = 5;

/// Upper bound on accepted input size (256 MiB). Far above any legitimate Luau
/// bytecode chunk; a larger `len` almost certainly means a caller bug (e.g. an
/// uninitialised or negative-turned-huge size) and is rejected up front.
pub const MAX_INPUT_LEN: usize = 256 * 1024 * 1024;

thread_local! {
    /// Per-thread last-error message. Set on every non-zero return; read by
    /// [`luau_last_error`]. The returned pointer stays valid until the next
    /// FFI call on the same thread overwrites it.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Record a message for [`luau_last_error`] on the current thread.
fn set_last_error(msg: impl Into<String>) {
    // Replace interior NULs so an error string is always representable.
    let cleaned = msg.into().replace('\0', " ");
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(cleaned).ok();
    });
}

/// Validate the raw (pointer, length) pair and turn it into a byte slice.
///
/// A zero length yields an empty slice regardless of the pointer, so callers may
/// pass a null pointer for empty input.
fn input_slice<'a>(bytecode: *const u8, len: usize) -> Result<&'a [u8], c_int> {
    if len == 0 {
        return Ok(&[]);
    }
    if bytecode.is_null() {
        set_last_error("bytecode pointer is null but length is non-zero");
        return Err(LUAU_ERR_NULL_ARG);
    }
    if len > MAX_INPUT_LEN {
        set_last_error(format!(
            "input length {len} exceeds maximum of {MAX_INPUT_LEN} bytes"
        ));
        return Err(LUAU_ERR_INPUT_TOO_LARGE);
    }
    // SAFETY: `bytecode` is non-null and, per the C caller's contract, points to
    // at least `len` initialised, readable bytes that stay valid for the call.
    // The returned borrow does not outlive the call (it feeds a closure that
    // completes before this function returns to C).
    Ok(unsafe { slice::from_raw_parts(bytecode, len) })
}

/// Shared body for the "bytecode in, C string out" functions.
///
/// Handles argument validation, panic isolation, and ownership transfer of the
/// resulting string. `*out` is always initialised to null first, so the caller
/// never observes (or frees) an indeterminate pointer, even on error.
fn run_string_fn<F>(
    bytecode: *const u8,
    len: usize,
    out: *mut *mut c_char,
    f: F,
) -> c_int
where
    F: FnOnce(&[u8]) -> anyhow::Result<String>,
{
    if out.is_null() {
        set_last_error("output pointer is null");
        return LUAU_ERR_NULL_ARG;
    }
    // SAFETY: `out` is non-null; the C caller must supply a writable `char*`.
    unsafe { *out = ptr::null_mut() };

    let slice = match input_slice(bytecode, len) {
        Ok(s) => s,
        Err(code) => return code,
    };

    // `AssertUnwindSafe`: on a panic we discard the closure's captures and only
    // report an error code, so there is no observable broken invariant.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(slice)));

    match outcome {
        Ok(Ok(text)) => match CString::new(text) {
            Ok(cstr) => {
                // SAFETY: `out` was null-checked above and is writable.
                unsafe { *out = cstr.into_raw() };
                LUAU_OK
            }
            Err(_) => {
                set_last_error("output contained an interior NUL byte");
                LUAU_ERR_ENCODING
            }
        },
        Ok(Err(err)) => {
            set_last_error(format!("{err:#}"));
            LUAU_ERR_DECODE
        }
        Err(_) => {
            set_last_error("internal panic while processing bytecode");
            LUAU_ERR_PANIC
        }
    }
}

/// Decompile Luau bytecode into Luau source.
///
/// On success writes a newly allocated, NUL-terminated C string to `*out_source`
/// and returns [`LUAU_OK`]. The caller owns the string and must release it with
/// [`luau_string_free`]. On failure returns a non-zero `LUAU_ERR_*` code, leaves
/// `*out_source` null, and sets [`luau_last_error`].
#[no_mangle]
pub extern "C" fn luau_decompile(
    bytecode: *const u8,
    len: usize,
    out_source: *mut *mut c_char,
) -> c_int {
    run_string_fn(bytecode, len, out_source, luau_core::decompile)
}

/// Disassemble Luau bytecode into a readable instruction listing.
///
/// `show_debug` is a boolean flag: non-zero includes debug information
/// (local/upvalue names, line numbers) in the listing, zero omits it. Output
/// ownership and error handling match [`luau_decompile`].
#[no_mangle]
pub extern "C" fn luau_disassemble(
    bytecode: *const u8,
    len: usize,
    show_debug: c_int,
    out_text: *mut *mut c_char,
) -> c_int {
    run_string_fn(bytecode, len, out_text, move |bc| {
        luau_core::disassemble(bc, show_debug != 0)
    })
}

/// Parse Luau bytecode and return structured metadata as a JSON object.
///
/// The JSON mirrors `luau_core::BytecodeInfo` (version, proto/string counts,
/// main proto index, and a per-proto array). Output ownership and error
/// handling match [`luau_decompile`].
#[no_mangle]
pub extern "C" fn luau_info_json(
    bytecode: *const u8,
    len: usize,
    out_json: *mut *mut c_char,
) -> c_int {
    run_string_fn(bytecode, len, out_json, |bc| {
        let info = luau_core::info(bc)?;
        Ok(serde_json::to_string_pretty(&info)?)
    })
}

/// Return the Luau bytecode version byte (a small integer, currently 3-8), or
/// `-1` if the input is empty, too large, unreadable, or not a recognised
/// bytecode version. This never allocates and does not set an output string.
#[no_mangle]
pub extern "C" fn luau_bytecode_version(bytecode: *const u8, len: usize) -> c_int {
    let slice = match input_slice(bytecode, len) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| luau_core::bytecode_version(slice)));
    match outcome {
        Ok(Some(version)) => c_int::from(version),
        Ok(None) => -1,
        Err(_) => {
            set_last_error("internal panic while reading bytecode version");
            -1
        }
    }
}

/// Return the last error message recorded on the calling thread, or null if
/// there is none.
///
/// The returned pointer is owned by the library and remains valid until the next
/// FFI call on the same thread. Do **not** free it. Copy the string if you need
/// to keep it. Only meaningful immediately after a function returned a non-zero
/// code (or `luau_bytecode_version` returned `-1`).
#[no_mangle]
pub extern "C" fn luau_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(cstr) => cstr.as_ptr(),
        None => ptr::null(),
    })
}

/// Free a string returned by this library (from [`luau_decompile`],
/// [`luau_disassemble`], or [`luau_info_json`]).
///
/// Passing null is a no-op. Passing any pointer this library did not hand out,
/// or freeing the same pointer twice, is undefined behaviour.
#[no_mangle]
pub extern "C" fn luau_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    // SAFETY: `s` is non-null and was produced by `CString::into_raw` in one of
    // this crate's functions; per the documented contract each such pointer is
    // freed at most once. Reconstructing and dropping the `CString` frees it
    // with the matching Rust allocator.
    unsafe { drop(CString::from_raw(s)) };
}

/// Return the decompiler version string (e.g. "0.2.0").
///
/// The returned pointer is a static, NUL-terminated string owned by the library
/// and valid for the lifetime of the process. Do not free it.
#[no_mangle]
pub extern "C" fn luau_version() -> *const c_char {
    static VERSION: OnceLock<CString> = OnceLock::new();
    VERSION
        .get_or_init(|| CString::new(luau_core::VERSION).unwrap_or_default())
        .as_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_null_and_nonempty() {
        let ptr = luau_version();
        assert!(!ptr.is_null());
        // SAFETY: `luau_version` returns a valid static NUL-terminated string.
        let s = unsafe { std::ffi::CStr::from_ptr(ptr) };
        assert!(!s.to_bytes().is_empty());
    }

    #[test]
    fn null_output_pointer_is_rejected() {
        let data = [0u8; 4];
        let code = luau_decompile(data.as_ptr(), data.len(), ptr::null_mut());
        assert_eq!(code, LUAU_ERR_NULL_ARG);
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut out: *mut c_char = ptr::null_mut();
        // Non-null pointer with an impossible length: the guard must fire before
        // any dereference of the (deliberately tiny) backing buffer.
        let one = [0u8; 1];
        let code = luau_decompile(one.as_ptr(), MAX_INPUT_LEN + 1, &mut out);
        assert_eq!(code, LUAU_ERR_INPUT_TOO_LARGE);
        assert!(out.is_null());
    }

    #[test]
    fn garbage_bytecode_reports_decode_error_not_panic() {
        let junk = [0xFFu8; 32];
        let mut out: *mut c_char = ptr::null_mut();
        let code = luau_decompile(junk.as_ptr(), junk.len(), &mut out);
        assert_ne!(code, LUAU_OK);
        assert_ne!(code, LUAU_ERR_PANIC);
        assert!(out.is_null());
        assert!(!luau_last_error().is_null());
    }

    #[test]
    fn version_of_garbage_is_negative_one() {
        let junk = [0x00u8; 8];
        assert_eq!(luau_bytecode_version(junk.as_ptr(), junk.len()), -1);
    }

    #[test]
    fn string_free_handles_null() {
        luau_string_free(ptr::null_mut());
    }
}
