/*
 * luau_decompiler.h — C ABI for the Luau decompiler.
 *
 * A thin, stable C interface over the Rust `luau-core` library: decompile,
 * disassemble, inspect, and version-probe Luau bytecode from C, C++, or any
 * language with a C FFI.
 *
 * Link against the `luau_ffi` shared or static library built by
 * `cargo build --release -p luau-ffi`. See README.md for build and link
 * instructions.
 *
 * Conventions
 * -----------
 *  - Bytecode input is a (pointer, length) pair. A length of 0 is treated as
 *    empty input and the pointer may then be NULL.
 *  - Functions that produce text write a newly allocated, NUL-terminated C
 *    string to their `char** out` argument and return LUAU_OK (0). The caller
 *    owns that string and MUST release it with luau_string_free(). Do not use
 *    the C library free() — the allocators differ.
 *  - On failure these functions return a non-zero LUAU_ERR_* code, leave the
 *    out-pointer set to NULL, and record a message retrievable with
 *    luau_last_error() (per-thread).
 *  - No function unwinds or aborts across the boundary on bad input; a caught
 *    internal panic surfaces as LUAU_ERR_PANIC.
 */

#ifndef LUAU_DECOMPILER_H
#define LUAU_DECOMPILER_H

#include <stddef.h> /* size_t  */
#include <stdint.h> /* uint8_t */

#ifdef __cplusplus
extern "C" {
#endif

/* ---- Result codes ------------------------------------------------------- */

/** Success. */
#define LUAU_OK 0
/** A required pointer was NULL (or a non-zero length with a NULL data pointer). */
#define LUAU_ERR_NULL_ARG 1
/** Input length exceeded the accepted maximum (see LUAU_MAX_INPUT_LEN). */
#define LUAU_ERR_INPUT_TOO_LARGE 2
/** Bytecode could not be parsed or decompiled (malformed/unsupported input). */
#define LUAU_ERR_DECODE 3
/** An internal panic was caught; the boundary is intact but this is a bug. */
#define LUAU_ERR_PANIC 4
/** Produced text held an interior NUL byte and cannot be a C string. */
#define LUAU_ERR_ENCODING 5

/** Upper bound on accepted input size, in bytes (256 MiB). */
#define LUAU_MAX_INPUT_LEN (256u * 1024u * 1024u)

/* ---- Functions ---------------------------------------------------------- */

/**
 * Decompile Luau bytecode into Luau source.
 *
 * @param bytecode    Pointer to `len` bytes of Luau bytecode (may be NULL if
 *                    len == 0).
 * @param len         Number of bytes at `bytecode`.
 * @param out_source  On success, receives a heap-allocated, NUL-terminated
 *                    string owned by the caller (free with luau_string_free).
 *                    Set to NULL on failure. Must not be NULL.
 * @return LUAU_OK on success, otherwise a non-zero LUAU_ERR_* code.
 */
int luau_decompile(const uint8_t *bytecode, size_t len, char **out_source);

/**
 * Disassemble Luau bytecode into a readable instruction listing.
 *
 * @param show_debug  Non-zero to include debug info (names, line numbers);
 *                    zero to omit it.
 * @param out_text    See luau_decompile's out_source (caller frees).
 * @return LUAU_OK on success, otherwise a non-zero LUAU_ERR_* code.
 */
int luau_disassemble(const uint8_t *bytecode, size_t len, int show_debug,
                     char **out_text);

/**
 * Parse Luau bytecode and return structured metadata as a JSON object
 * (version, proto/string counts, main proto index, and a per-proto array).
 *
 * @param out_json  See luau_decompile's out_source (caller frees).
 * @return LUAU_OK on success, otherwise a non-zero LUAU_ERR_* code.
 */
int luau_info_json(const uint8_t *bytecode, size_t len, char **out_json);

/**
 * Return the Luau bytecode version byte (a small integer, currently 3-8), or
 * -1 if the input is empty, too large, unreadable, or not a recognised
 * version. Does not allocate.
 */
int luau_bytecode_version(const uint8_t *bytecode, size_t len);

/**
 * Return the last error message recorded on the calling thread, or NULL if
 * there is none.
 *
 * The returned pointer is owned by the library and stays valid only until the
 * next luau_* call on the same thread. Do NOT free it; copy it if you need to
 * keep it. Meaningful only right after a non-zero return (or a -1 from
 * luau_bytecode_version).
 */
const char *luau_last_error(void);

/**
 * Free a string previously returned by luau_decompile, luau_disassemble, or
 * luau_info_json. Passing NULL is a no-op. Passing any other pointer, or
 * freeing twice, is undefined behaviour.
 */
void luau_string_free(char *s);

/**
 * Return the decompiler version string (e.g. "0.2.0"). The pointer is static,
 * valid for the process lifetime, and must NOT be freed.
 */
const char *luau_version(void);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* LUAU_DECOMPILER_H */
