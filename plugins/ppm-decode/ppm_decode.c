// ASCII P3 decoder for Histima registered-Wasm plugin ABI v3.

#include "tima_plugin.h"

typedef struct {
    const TimaU8 *at;
    const TimaU8 *end;
} Cursor;

static int whitespace(TimaU8 value) {
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

static int unsigned_token(Cursor *cursor, TimaU32 *result) {
    skip_trivia(cursor);
    if (cursor->at == cursor->end || *cursor->at < '0' || *cursor->at > '9') {
        return 0;
    }
    TimaU32 value = 0;
    while (cursor->at < cursor->end && *cursor->at >= '0' && *cursor->at <= '9') {
        TimaU32 digit = (TimaU32)(*cursor->at++ - '0');
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

__attribute__((export_name("tima_transform")))
TimaU32 tima_transform(
    TimaU32 arguments_pointer,
    TimaU32 argument_count,
    TimaU32 result_pointer
) {
    TimaValue *result = (TimaValue *)(TimaUPtr)result_pointer;
    if (argument_count != 1) {
        TIMA_FAIL(result, "ppm.decode expects one argument");
    }
    const TimaValue *argument = (const TimaValue *)(TimaUPtr)arguments_pointer;
    if (argument->words[0] != TIMA_VALUE_BYTES_VIEW) {
        TIMA_FAIL(result, "ppm.decode expects a BytesView");
    }

    TimaU32 input_pointer = argument->words[1];
    TimaU32 input_length = argument->words[2];
    Cursor cursor = {
        (const TimaU8 *)(TimaUPtr)input_pointer,
        (const TimaU8 *)(TimaUPtr)(input_pointer + input_length),
    };

    skip_trivia(&cursor);
    if ((TimaU32)(cursor.end - cursor.at) < 2 ||
        cursor.at[0] != 'P' || cursor.at[1] != '3') {
        TIMA_FAIL(result, "expected an ASCII P3 header");
    }
    cursor.at += 2;
    if (cursor.at < cursor.end && !whitespace(*cursor.at) && *cursor.at != '#') {
        TIMA_FAIL(result, "expected whitespace after the P3 header");
    }

    TimaU32 width;
    TimaU32 height;
    TimaU32 maximum;
    if (!unsigned_token(&cursor, &width) || !unsigned_token(&cursor, &height) ||
        !unsigned_token(&cursor, &maximum)) {
        TIMA_FAIL(result, "header is incomplete or invalid");
    }
    if (width == 0 || height == 0) {
        TIMA_FAIL(result, "image dimensions must be non-zero");
    }
    if (maximum != 255) {
        TIMA_FAIL(result, "maximum channel value must be 255");
    }
    if (height > 0xffffffffu / width) {
        TIMA_FAIL(result, "image dimensions overflow the plugin ABI");
    }
    TimaU32 pixels = width * height;
    if (pixels > 0xffffffffu / 4) {
        TIMA_FAIL(result, "RGBA byte length overflows the plugin ABI");
    }
    TimaU32 byte_length = pixels * 4;
    TimaU32 output_pointer = tima_alloc(byte_length);
    if (output_pointer == 0) {
        TIMA_FAIL(result, "image exceeds the plugin memory limit");
    }
    TimaU8 *output = (TimaU8 *)(TimaUPtr)output_pointer;
    for (TimaU32 pixel = 0; pixel < pixels; pixel++) {
        TimaU32 red;
        TimaU32 green;
        TimaU32 blue;
        if (!unsigned_token(&cursor, &red) || !unsigned_token(&cursor, &green) ||
            !unsigned_token(&cursor, &blue)) {
            TIMA_FAIL(result, "channel sample count is incomplete");
        }
        if (red > 255 || green > 255 || blue > 255) {
            TIMA_FAIL(result, "channel sample must be from 0 through 255");
        }
        output[pixel * 4] = (TimaU8)red;
        output[pixel * 4 + 1] = (TimaU8)green;
        output[pixel * 4 + 2] = (TimaU8)blue;
        output[pixel * 4 + 3] = 255;
    }
    skip_trivia(&cursor);
    if (cursor.at != cursor.end) {
        TIMA_FAIL(result, "channel sample count exceeds the image dimensions");
    }

    result->words[0] = TIMA_VALUE_IMAGE;
    result->words[1] = output_pointer;
    result->words[2] = byte_length;
    result->words[3] = TIMA_IMAGE_FORMAT_RGBA8;
    result->words[4] = width;
    result->words[5] = height;
    result->words[6] = width * 4;
    result->words[7] = 0;
    return 0;
}
