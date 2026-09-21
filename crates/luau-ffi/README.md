# luau-ffi

A C ABI (FFI) over the Luau decompiler core (`luau-core`). It lets you
decompile, disassemble, inspect, and version-probe Luau bytecode from C, C++,
or any language with a C foreign-function interface (Python `ctypes`/`cffi`,
Go `cgo`, Node N-API, Ruby FFI, and so on).

This crate exists to answer the request "is there any way to call this from C?"
— yes, through the functions declared in
[`include/luau_decompiler.h`](include/luau_decompiler.h).

## API

| Function | Purpose |
|---|---|
| `int luau_decompile(const uint8_t *bytecode, size_t len, char **out_source)` | Decompile bytecode to Luau source. |
| `int luau_disassemble(const uint8_t *bytecode, size_t len, int show_debug, char **out_text)` | Disassemble bytecode to an instruction listing. |
| `int luau_info_json(const uint8_t *bytecode, size_t len, char **out_json)` | Structured metadata as a JSON object. |
| `int luau_bytecode_version(const uint8_t *bytecode, size_t len)` | Bytecode version byte, or `-1`. |
| `const char *luau_last_error(void)` | Last error message for the current thread (do not free). |
| `void luau_string_free(char *s)` | Free a string returned by this library. |
| `const char *luau_version(void)` | Decompiler version string (static, do not free). |

### Contract

- String-producing functions return `0` (`LUAU_OK`) on success and write a
  heap-allocated, NUL-terminated string to their `char**` out-argument. **You
  own that string and must free it with `luau_string_free`** — not the C
  library `free`, because the allocators differ.
- On failure they return a non-zero `LUAU_ERR_*` code, set the out-pointer to
  `NULL`, and record a message you can read with `luau_last_error()`.
- `luau_last_error()` returns a pointer valid only until the next `luau_*` call
  on the same thread. Copy it if you need to keep it.
- Inputs are null- and size-guarded, and panics are caught at the boundary —
  bad bytecode yields an error code, never a crash or an unwind into your code.

## Building the library

From the workspace root:

```sh
cargo build --release -p luau-ffi
```

This produces both a shared library and a static archive under
`target/release/`. The artifact base name is `luau_ffi` (Cargo turns the
package's hyphen into an underscore):

| Platform | Shared library | Static library |
|---|---|---|
| Linux | `libluau_ffi.so` | `libluau_ffi.a` |
| macOS | `libluau_ffi.dylib` | `libluau_ffi.a` |
| Windows (MSVC) | `luau_ffi.dll` (+ import lib `luau_ffi.dll.lib`) | `luau_ffi.lib` |

Drop `--release` for an unoptimised debug build under `target/debug/`.

## Compiling the C example

The example lives in [`examples/decompile.c`](examples/decompile.c). Paths below
are written relative to that file; adjust `-I` (header dir) and `-L` (library
dir) to wherever you keep them.

### Linux (gcc/clang)

Link the shared library:

```sh
cc examples/decompile.c -I include -L target/release \
   -lluau_ffi -lpthread -ldl -lm -o decompile

# Make the .so discoverable at runtime (or install it to a system path):
LD_LIBRARY_PATH=target/release ./decompile path/to/script.luac
```

### macOS (clang)

```sh
cc examples/decompile.c -I include -L target/release \
   -lluau_ffi -o decompile

DYLD_LIBRARY_PATH=target/release ./decompile path/to/script.luac
```

### Windows (MSVC, `cl`)

Using the Developer Command Prompt, link against the DLL's import library:

```bat
cl examples\decompile.c /I include /Fe:decompile.exe ^
   /link /LIBPATH:target\release luau_ffi.dll.lib

REM Ensure luau_ffi.dll is on PATH or next to decompile.exe when you run it.
decompile.exe path\to\script.luac
```

### Static linking

To link the static archive instead of the shared library, you must also link
the native system libraries the Rust standard library depends on. The exact set
is platform- and toolchain-specific; print it with:

```sh
cargo rustc --release -p luau-ffi --crate-type staticlib -- --print native-static-libs
```

Then pass those libraries after `-lluau_ffi` (Linux/macOS) or add them to the
`/link` line (Windows). Linking the shared library is simpler and is the
recommended default.

## Using from other languages

Any C-FFI-capable language can bind these symbols. The essentials are the same
everywhere: pass the bytecode as a `(pointer, length)` pair, receive a `char*`
you must pass back to `luau_string_free`, and check the integer return code
against `0`.

## Safety notes

- Free every returned string exactly once, with `luau_string_free`.
- Do not free the pointers from `luau_last_error()` or `luau_version()`.
- A returned string from one call must be freed before you rely on
  `luau_last_error()` staying put across further calls on that thread.
