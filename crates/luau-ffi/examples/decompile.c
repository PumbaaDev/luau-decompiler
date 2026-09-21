/*
 * decompile.c — minimal C example for the Luau decompiler C API.
 *
 * Reads a compiled Luau bytecode file, decompiles it, prints the recovered
 * source to stdout, and frees the result.
 *
 * Build (after `cargo build --release -p luau-ffi`), e.g. on Linux:
 *
 *   cc decompile.c -I ../include -L ../../../target/release \
 *      -lluau_ffi -lpthread -ldl -lm -o decompile
 *
 * See ../README.md for macOS and Windows link flags.
 *
 * Usage:
 *
 *   ./decompile path/to/script.luac
 */

#include <stdio.h>
#include <stdlib.h>

#include "luau_decompiler.h"

/* Read an entire file into a heap buffer. Returns the buffer (caller frees) and
 * writes its size to *out_len, or NULL on failure. */
static uint8_t *read_file(const char *path, size_t *out_len) {
    FILE *f = fopen(path, "rb");
    if (f == NULL) {
        fprintf(stderr, "error: cannot open '%s'\n", path);
        return NULL;
    }
    if (fseek(f, 0, SEEK_END) != 0) {
        fclose(f);
        return NULL;
    }
    long size = ftell(f);
    if (size < 0) {
        fclose(f);
        return NULL;
    }
    rewind(f);

    uint8_t *buf = (uint8_t *)malloc((size_t)size);
    if (buf == NULL && size > 0) {
        fclose(f);
        fprintf(stderr, "error: out of memory\n");
        return NULL;
    }
    size_t read = fread(buf, 1, (size_t)size, f);
    fclose(f);
    if (read != (size_t)size) {
        free(buf);
        fprintf(stderr, "error: short read on '%s'\n", path);
        return NULL;
    }
    *out_len = read;
    return buf;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s path/to/script.luac\n", argv[0]);
        return 2;
    }

    printf("-- luau-decompiler C API v%s\n", luau_version());

    size_t len = 0;
    uint8_t *bytecode = read_file(argv[1], &len);
    if (bytecode == NULL) {
        return 1;
    }

    char *source = NULL;
    int rc = luau_decompile(bytecode, len, &source);
    free(bytecode);

    if (rc != LUAU_OK) {
        const char *msg = luau_last_error();
        fprintf(stderr, "decompile failed (code %d): %s\n", rc,
                msg != NULL ? msg : "(no message)");
        return 1;
    }

    fputs(source, stdout);
    luau_string_free(source);
    return 0;
}
