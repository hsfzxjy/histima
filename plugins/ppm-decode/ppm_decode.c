// Histima registered-Wasm plugin ABI v1 reference codec.
//
// This module deliberately imports nothing: it has no WASI or ambient host
// access. The host copies one immutable BytesView into linear memory and
// adopts the returned image metadata and pixels into final immutable storage.

typedef unsigned char u8;
typedef unsigned int u32;
typedef __UINTPTR_TYPE__ uptr;

enum {
    RESULT_IMAGE = 0,
    RESULT_DIAGNOSTIC = 1,
    IMAGE_FORMAT_RGBA8 = 1,
    WASM_PAGE_SIZE = 65536,
};

extern u8 __heap_base;
static u32 arena_cursor;

__attribute__((export_name("tima_abi_version")))
u32 tima_abi_version(void) {
    return 1;
}

static u32 align_up(u32 value, u32 alignment) {
    return (value + alignment - 1) & ~(alignment - 1);
}

__attribute__((export_name("tima_reset")))
void tima_reset(void) {
    arena_cursor = align_up((u32)(uptr)&__heap_base, 16);
}

__attribute__((export_name("tima_alloc")))
u32 tima_alloc(u32 length) {
    if (arena_cursor == 0) {
        tima_reset();
    }
    u32 start = align_up(arena_cursor, 16);
    if (length > 0xffffffffu - start) {
        return 0;
    }
    u32 end = start + length;
    u32 pages = __builtin_wasm_memory_size(0);
    u32 required_pages = end / WASM_PAGE_SIZE + (end % WASM_PAGE_SIZE != 0);
    if (required_pages > pages) {
        u32 growth = required_pages - pages;
        if (__builtin_wasm_memory_grow(0, growth) == (u32)-1) {
            return 0;
        }
    }
    arena_cursor = end;
    return start;
}

typedef struct {
    const u8 *at;
    const u8 *end;
} Cursor;

static int whitespace(u8 value) {
    return value == ' ' || value == '\t' || value == '\r' || value == '\n' ||
           value == '\f' || value == '\v';
}

static void skip_trivia(Cursor *cursor) {
    for (;;) {
        while (cursor->at < cursor->end && whitespace(*cursor->at)) {
            cursor->at++;
        }
        if (cursor->at == cursor->end || *cursor->at != '#') {
            return;
        }
        while (cursor->at < cursor->end && *cursor->at != '\n') {
            cursor->at++;
        }
    }
}

static int unsigned_token(Cursor *cursor, u32 *result) {
    skip_trivia(cursor);
    if (cursor->at == cursor->end || *cursor->at < '0' || *cursor->at > '9') {
        return 0;
    }
    u32 value = 0;
    while (cursor->at < cursor->end && *cursor->at >= '0' && *cursor->at <= '9') {
        u32 digit = (u32)(*cursor->at++ - '0');
        if (value > (0xffffffffu - digit) / 10) {
            return 0;
        }
        value = value * 10 + digit;
    }
    if (cursor->at < cursor->end && !whitespace(*cursor->at) && *cursor->at != '#') {
        return 0;
    }
    *result = value;
    return 1;
}

static void diagnostic(u32 *result, const char *message, u32 length) {
    result[0] = RESULT_DIAGNOSTIC;
    result[1] = (u32)(uptr)message;
    result[2] = length;
    result[3] = 0;
    result[4] = 0;
    result[5] = 0;
    result[6] = 0;
    result[7] = 0;
}

#define FAIL(result, literal) do { diagnostic((result), (literal), sizeof(literal) - 1); return 0; } while (0)

__attribute__((export_name("tima_transform")))
u32 tima_transform(u32 input_pointer, u32 input_length, u32 result_pointer) {
    Cursor cursor = {
        (const u8 *)(uptr)input_pointer,
        (const u8 *)(uptr)(input_pointer + input_length),
    };
    u32 *result = (u32 *)(uptr)result_pointer;

    skip_trivia(&cursor);
    if ((u32)(cursor.end - cursor.at) < 2 || cursor.at[0] != 'P' || cursor.at[1] != '3') {
        FAIL(result, "expected an ASCII P3 header");
    }
    cursor.at += 2;
    if (cursor.at < cursor.end && !whitespace(*cursor.at) && *cursor.at != '#') {
        FAIL(result, "expected whitespace after the P3 header");
    }

    u32 width;
    u32 height;
    u32 maximum;
    if (!unsigned_token(&cursor, &width) || !unsigned_token(&cursor, &height) ||
        !unsigned_token(&cursor, &maximum)) {
        FAIL(result, "header is incomplete or invalid");
    }
    if (width == 0 || height == 0) {
        FAIL(result, "image dimensions must be non-zero");
    }
    if (maximum != 255) {
        FAIL(result, "maximum channel value must be 255");
    }
    if (height > 0xffffffffu / width) {
        FAIL(result, "image dimensions overflow the plugin ABI");
    }
    u32 pixels = width * height;
    if (pixels > 0xffffffffu / 4) {
        FAIL(result, "RGBA byte length overflows the plugin ABI");
    }
    u32 byte_length = pixels * 4;
    u32 output_pointer = tima_alloc(byte_length);
    if (output_pointer == 0) {
        FAIL(result, "image exceeds the plugin memory limit");
    }
    u8 *output = (u8 *)(uptr)output_pointer;
    for (u32 pixel = 0; pixel < pixels; pixel++) {
        u32 red;
        u32 green;
        u32 blue;
        if (!unsigned_token(&cursor, &red) || !unsigned_token(&cursor, &green) ||
            !unsigned_token(&cursor, &blue)) {
            FAIL(result, "channel sample count is incomplete");
        }
        if (red > 255 || green > 255 || blue > 255) {
            FAIL(result, "channel sample must be from 0 through 255");
        }
        output[pixel * 4] = (u8)red;
        output[pixel * 4 + 1] = (u8)green;
        output[pixel * 4 + 2] = (u8)blue;
        output[pixel * 4 + 3] = 255;
    }
    skip_trivia(&cursor);
    if (cursor.at != cursor.end) {
        FAIL(result, "channel sample count exceeds the image dimensions");
    }

    result[0] = RESULT_IMAGE;
    result[1] = output_pointer;
    result[2] = byte_length;
    result[3] = IMAGE_FORMAT_RGBA8;
    result[4] = width;
    result[5] = height;
    result[6] = width * 4;
    result[7] = 0;
    return 0;
}
