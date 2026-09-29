// Deterministic ASCII P3 encoder for Histima registered-Wasm plugin ABI v2.

#include "tima_plugin.h"

static TimaU8 *write_decimal(TimaU8 *output, TimaU32 value) {
    TimaU32 divisor = 1;
    while (value / divisor >= 10) {
        divisor *= 10;
    }
    for (;;) {
        *output++ = (TimaU8)('0' + value / divisor);
        value %= divisor;
        if (divisor == 1) {
            return output;
        }
        divisor /= 10;
    }
}

__attribute__((export_name("tima_transform")))
TimaU32 tima_transform(
    TimaU32 arguments_pointer,
    TimaU32 argument_count,
    TimaU32 result_pointer
) {
    TimaValue *result = (TimaValue *)(TimaUPtr)result_pointer;
    if (argument_count != 1) {
        TIMA_FAIL(result, "ppm.encode expects one argument");
    }
    const TimaValue *image = (const TimaValue *)(TimaUPtr)arguments_pointer;
    if (image->words[0] != TIMA_VALUE_IMAGE_VIEW) {
        TIMA_FAIL(result, "ppm.encode expects an ImageView");
    }
    if (image->words[3] != TIMA_IMAGE_FORMAT_RGBA8) {
        TIMA_FAIL(result, "ppm.encode requires RGBA8 pixels");
    }

    TimaU32 input_pointer = image->words[1];
    TimaU32 input_length = image->words[2];
    TimaU32 width = image->words[4];
    TimaU32 height = image->words[5];
    TimaU32 stride = image->words[6];
    if (width == 0 || height == 0) {
        TIMA_FAIL(result, "image dimensions must be non-zero");
    }
    if (width > 0xffffffffu / 4 || stride < width * 4) {
        TIMA_FAIL(result, "image stride is invalid");
    }
    if (height > 0xffffffffu / stride || input_length != height * stride) {
        TIMA_FAIL(result, "image byte length is invalid");
    }
    if (height > 0xffffffffu / width) {
        TIMA_FAIL(result, "image dimensions overflow the plugin ABI");
    }
    TimaU32 pixels = width * height;
    if (pixels > (0xffffffffu - 32) / 12) {
        TIMA_FAIL(result, "encoded byte length overflows the plugin ABI");
    }
    TimaU32 capacity = 32 + pixels * 12;
    TimaU32 output_pointer = tima_alloc(capacity);
    if (output_pointer == 0) {
        TIMA_FAIL(result, "encoded image exceeds the plugin memory limit");
    }

    const TimaU8 *input = (const TimaU8 *)(TimaUPtr)input_pointer;
    TimaU8 *output = (TimaU8 *)(TimaUPtr)output_pointer;
    TimaU8 *at = output;
    *at++ = 'P';
    *at++ = '3';
    *at++ = '\n';
    at = write_decimal(at, width);
    *at++ = ' ';
    at = write_decimal(at, height);
    *at++ = '\n';
    *at++ = '2';
    *at++ = '5';
    *at++ = '5';
    *at++ = '\n';

    for (TimaU32 y = 0; y < height; y++) {
        for (TimaU32 x = 0; x < width; x++) {
            const TimaU8 *pixel = input + y * stride + x * 4;
            at = write_decimal(at, pixel[0]);
            *at++ = ' ';
            at = write_decimal(at, pixel[1]);
            *at++ = ' ';
            at = write_decimal(at, pixel[2]);
            *at++ = x + 1 == width ? '\n' : ' ';
        }
    }

    result->words[0] = TIMA_VALUE_BYTES;
    result->words[1] = output_pointer;
    result->words[2] = (TimaU32)(at - output);
    for (TimaU32 index = 3; index < 8; index++) {
        result->words[index] = 0;
    }
    return 0;
}
