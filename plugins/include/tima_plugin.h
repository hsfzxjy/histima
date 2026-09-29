#ifndef TIMA_PLUGIN_H
#define TIMA_PLUGIN_H

typedef unsigned char TimaU8;
typedef unsigned int TimaU32;
typedef __UINTPTR_TYPE__ TimaUPtr;

enum {
    TIMA_PLUGIN_ABI_VERSION = 3,
    TIMA_VALUE_BYTES_VIEW = 1,
    TIMA_VALUE_IMAGE_VIEW = 2,
    TIMA_VALUE_I64 = 3,
    TIMA_VALUE_BYTES = 4,
    TIMA_VALUE_IMAGE = 5,
    TIMA_VALUE_DIAGNOSTIC = 255,
    TIMA_IMAGE_FORMAT_RGBA8 = 1,
    TIMA_WASM_PAGE_SIZE = 65536,
};

typedef struct {
    TimaU32 words[8];
} TimaValue;

extern TimaU8 __heap_base;
static TimaU32 tima_arena_cursor;

__attribute__((export_name("tima_abi_version")))
TimaU32 tima_abi_version(void) {
    return TIMA_PLUGIN_ABI_VERSION;
}

static TimaU32 tima_align_up(TimaU32 value, TimaU32 alignment) {
    return (value + alignment - 1) & ~(alignment - 1);
}

__attribute__((export_name("tima_reset")))
void tima_reset(void) {
    tima_arena_cursor = tima_align_up((TimaU32)(TimaUPtr)&__heap_base, 16);
}

__attribute__((export_name("tima_alloc")))
TimaU32 tima_alloc(TimaU32 length) {
    if (tima_arena_cursor == 0) {
        tima_reset();
    }
    TimaU32 start = tima_align_up(tima_arena_cursor, 16);
    if (length > 0xffffffffu - start) {
        return 0;
    }
    TimaU32 end = start + length;
    TimaU32 pages = __builtin_wasm_memory_size(0);
    TimaU32 required_pages =
        end / TIMA_WASM_PAGE_SIZE + (end % TIMA_WASM_PAGE_SIZE != 0);
    if (required_pages > pages) {
        TimaU32 growth = required_pages - pages;
        if (__builtin_wasm_memory_grow(0, growth) == (TimaU32)-1) {
            return 0;
        }
    }
    tima_arena_cursor = end;
    return start;
}

static void tima_diagnostic(
    TimaValue *result,
    const char *message,
    TimaU32 length
) {
    result->words[0] = TIMA_VALUE_DIAGNOSTIC;
    result->words[1] = (TimaU32)(TimaUPtr)message;
    result->words[2] = length;
    for (TimaU32 index = 3; index < 8; index++) {
        result->words[index] = 0;
    }
}

#define TIMA_FAIL(result, literal) do { \
    tima_diagnostic((result), (literal), sizeof(literal) - 1); \
    return 0; \
} while (0)

#endif
