// Deterministic RGBA8 nearest-neighbor resize for registered-Wasm ABI v4.

#include "tima_plugin.h"

typedef unsigned long long TimaU64;

static int positive_dimension(const TimaValue *value, TimaU32 *result) {
    if (value->words[0] != TIMA_VALUE_I64 || value->words[2] != 0 ||
        value->words[1] == 0) {
        return 0;
    }
    *result = value->words[1];
    return 1;
}

__attribute__((export_name("tima_transform")))
TimaU32 tima_transform(
    TimaU32 arguments_pointer,
    TimaU32 argument_count,
    TimaU32 result_pointer
) {
    TimaValue *result = (TimaValue *)(TimaUPtr)result_pointer;
    if (argument_count != 3) {
        TIMA_FAIL(result, "rgba.resize_nearest expects three arguments");
    }
    const TimaValue *arguments = (const TimaValue *)(TimaUPtr)arguments_pointer;
    const TimaValue *input = &arguments[0];
    if (input->words[0] != TIMA_VALUE_BUFFER_VIEW || input->words[3] != 3 ||
        input->words[6] != 4) {
        TIMA_FAIL(result, "rgba.resize_nearest expects a rank-3 RGBA8 BufferView");
    }

    TimaU32 source_height = input->words[4];
    TimaU32 source_width = input->words[5];
    TimaU32 source_stride = input->words[7];
    if (source_height == 0 || source_width == 0 ||
        source_width > 0xffffffffu / 4 || source_stride < source_width * 4 ||
        source_height > 0xffffffffu / source_stride ||
        input->words[2] != source_height * source_stride) {
        TIMA_FAIL(result, "rgba.resize_nearest received invalid Buffer metadata");
    }

    TimaU32 output_width;
    TimaU32 output_height;
    if (!positive_dimension(&arguments[1], &output_width)) {
        TIMA_FAIL(result, "rgba.resize_nearest width must be from 1 through 4294967295");
    }
    if (!positive_dimension(&arguments[2], &output_height)) {
        TIMA_FAIL(result, "rgba.resize_nearest height must be from 1 through 4294967295");
    }
    if (output_width > 0xffffffffu / 4) {
        TIMA_FAIL(result, "rgba.resize_nearest output row is too large");
    }
    TimaU32 output_stride = output_width * 4;
    if (output_height > 0xffffffffu / output_stride) {
        TIMA_FAIL(result, "rgba.resize_nearest output is too large");
    }
    TimaU32 output_length = output_height * output_stride;
    TimaU32 output_pointer = tima_alloc(output_length);
    if (output_pointer == 0) {
        TIMA_FAIL(result, "rgba.resize_nearest output exceeds the plugin memory limit");
    }

    const TimaU8 *source = (const TimaU8 *)(TimaUPtr)input->words[1];
    TimaU8 *output = (TimaU8 *)(TimaUPtr)output_pointer;
    for (TimaU32 y = 0; y < output_height; y++) {
        TimaU32 source_y = (TimaU32)(((TimaU64)y * source_height) / output_height);
        for (TimaU32 x = 0; x < output_width; x++) {
            TimaU32 source_x = (TimaU32)(((TimaU64)x * source_width) / output_width);
            const TimaU8 *source_pixel =
                source + source_y * source_stride + source_x * 4;
            TimaU8 *output_pixel = output + y * output_stride + x * 4;
            output_pixel[0] = source_pixel[0];
            output_pixel[1] = source_pixel[1];
            output_pixel[2] = source_pixel[2];
            output_pixel[3] = source_pixel[3];
        }
    }

    result->words[0] = TIMA_VALUE_BUFFER;
    result->words[1] = output_pointer;
    result->words[2] = output_length;
    result->words[3] = 3;
    result->words[4] = output_height;
    result->words[5] = output_width;
    result->words[6] = 4;
    result->words[7] = output_stride;
    return 0;
}
