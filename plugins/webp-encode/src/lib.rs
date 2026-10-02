use std::mem;
use std::slice;

const PLUGIN_ABI_VERSION: u32 = 4;
const VALUE_BUFFER_VIEW: u32 = 2;
const VALUE_I64: u32 = 3;
const VALUE_BYTES: u32 = 4;
const VALUE_DIAGNOSTIC: u32 = 255;
const MAX_DIMENSION: u32 = 16_383;

#[repr(C)]
#[derive(Clone, Copy)]
struct TimaValue {
    words: [u32; 8],
}

#[unsafe(no_mangle)]
pub extern "C" fn tima_abi_version() -> u32 {
    PLUGIN_ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn tima_reset() {}

#[unsafe(no_mangle)]
pub extern "C" fn tima_alloc(length: u32) -> u32 {
    let mut allocation = vec![0_u8; length as usize];
    let pointer = allocation.as_mut_ptr() as usize as u32;
    mem::forget(allocation);
    pointer
}

#[unsafe(no_mangle)]
pub extern "C" fn tima_transform(
    arguments_pointer: u32,
    argument_count: u32,
    result_pointer: u32,
) -> u32 {
    let result = unsafe { &mut *(result_pointer as usize as *mut TimaValue) };
    let encoded = match encode(arguments_pointer, argument_count) {
        Ok(encoded) => encoded,
        Err(message) => {
            diagnostic(result, message);
            return 0;
        }
    };
    let mut encoded = encoded;
    let pointer = encoded.as_mut_ptr() as usize as u32;
    let length = encoded.len() as u32;
    mem::forget(encoded);
    result.words = [VALUE_BYTES, pointer, length, 0, 0, 0, 0, 0];
    0
}

fn encode(arguments_pointer: u32, argument_count: u32) -> Result<Vec<u8>, String> {
    if argument_count != 2 {
        return Err("webp.encode expects two arguments".to_owned());
    }
    let arguments = unsafe {
        slice::from_raw_parts(
            arguments_pointer as usize as *const TimaValue,
            argument_count as usize,
        )
    };
    let buffer = arguments[0];
    if buffer.words[0] != VALUE_BUFFER_VIEW {
        return Err("webp.encode expects a BufferView first".to_owned());
    }
    if buffer.words[3] != 3 || buffer.words[6] != 4 {
        return Err("webp.encode requires Buffer shape [height, width, 4]".to_owned());
    }
    let quality = arguments[1];
    if quality.words[0] != VALUE_I64 || quality.words[3..].iter().any(|word| *word != 0) {
        return Err("webp.encode expects an i64 quality second".to_owned());
    }
    let mut quality_bytes = [0_u8; 8];
    quality_bytes[..4].copy_from_slice(&quality.words[1].to_le_bytes());
    quality_bytes[4..].copy_from_slice(&quality.words[2].to_le_bytes());
    let quality = i64::from_le_bytes(quality_bytes);
    if !(0..=100).contains(&quality) {
        return Err("webp.encode quality must be from 0 through 100".to_owned());
    }

    let height = buffer.words[4];
    let width = buffer.words[5];
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err("webp.encode dimensions must each be from 1 through 16383".to_owned());
    }
    let byte_length = buffer.words[2] as usize;
    let stride = buffer.words[7] as usize;
    let row_length = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| "webp.encode row byte length overflowed".to_owned())?;
    if stride < row_length {
        return Err("webp.encode image stride is invalid".to_owned());
    }
    let expected_length = (height as usize)
        .checked_mul(stride)
        .ok_or_else(|| "webp.encode image byte length overflowed".to_owned())?;
    if byte_length != expected_length {
        return Err("webp.encode image byte length is invalid".to_owned());
    }
    let input =
        unsafe { slice::from_raw_parts(buffer.words[1] as usize as *const u8, byte_length) };
    let packed_length = (height as usize)
        .checked_mul(row_length)
        .ok_or_else(|| "webp.encode packed byte length overflowed".to_owned())?;
    let mut packed = Vec::with_capacity(packed_length);
    for row in 0..height as usize {
        let start = row * stride;
        packed.extend_from_slice(&input[start..start + row_length]);
    }

    let input = webp_rust::ImageBuffer {
        width: width as usize,
        height: height as usize,
        rgba: packed,
    };
    let config = webp_rust::LossyEncodingConfig {
        quality: quality as f32,
        ..webp_rust::LossyEncodingConfig::default()
    };
    webp_rust::encode_lossy_with_config(&input, &config, None)
        .map_err(|error| format!("webp.encode could not encode image: {error}"))
}

fn diagnostic(result: &mut TimaValue, message: String) {
    let mut bytes = message.into_bytes();
    let pointer = bytes.as_mut_ptr() as usize as u32;
    let length = bytes.len() as u32;
    mem::forget(bytes);
    result.words = [VALUE_DIAGNOSTIC, pointer, length, 0, 0, 0, 0, 0];
}
