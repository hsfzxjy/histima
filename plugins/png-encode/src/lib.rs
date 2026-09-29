use std::mem;
use std::slice;

const PLUGIN_ABI_VERSION: u32 = 3;
const VALUE_IMAGE_VIEW: u32 = 2;
const VALUE_I64: u32 = 3;
const VALUE_BYTES: u32 = 4;
const VALUE_DIAGNOSTIC: u32 = 255;
const IMAGE_FORMAT_RGBA8: u32 = 1;

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

fn encode(arguments_pointer: u32, argument_count: u32) -> Result<Vec<u8>, &'static str> {
    if argument_count != 2 {
        return Err("png.encode expects two arguments");
    }
    let arguments = unsafe {
        slice::from_raw_parts(
            arguments_pointer as usize as *const TimaValue,
            argument_count as usize,
        )
    };
    let image = arguments[0];
    if image.words[0] != VALUE_IMAGE_VIEW {
        return Err("png.encode expects an ImageView first");
    }
    if image.words[3] != IMAGE_FORMAT_RGBA8 {
        return Err("png.encode requires RGBA8 pixels");
    }
    if arguments[1].words[0] != VALUE_I64 || arguments[1].words[3..].iter().any(|word| *word != 0) {
        return Err("png.encode expects an i64 compression second");
    }
    let mut compression_bytes = [0_u8; 8];
    compression_bytes[..4].copy_from_slice(&arguments[1].words[1].to_le_bytes());
    compression_bytes[4..].copy_from_slice(&arguments[1].words[2].to_le_bytes());
    let compression = i64::from_le_bytes(compression_bytes);
    if !(1..=9).contains(&compression) {
        return Err("png.encode compression must be from 1 through 9");
    }

    let byte_length = image.words[2] as usize;
    let width = image.words[4];
    let height = image.words[5];
    let stride = image.words[6] as usize;
    if width == 0 || height == 0 {
        return Err("png.encode image dimensions must be non-zero");
    }
    let row_length = (width as usize)
        .checked_mul(4)
        .ok_or("png.encode row length overflowed")?;
    if stride < row_length {
        return Err("png.encode image stride is invalid");
    }
    let expected_length = (height as usize)
        .checked_mul(stride)
        .ok_or("png.encode image byte length overflowed")?;
    if byte_length != expected_length {
        return Err("png.encode image byte length is invalid");
    }
    let input = unsafe { slice::from_raw_parts(image.words[1] as usize as *const u8, byte_length) };
    let packed_length = (height as usize)
        .checked_mul(row_length)
        .ok_or("png.encode packed byte length overflowed")?;
    let mut packed_storage = Vec::new();
    let pixels = if stride == row_length {
        input
    } else {
        packed_storage.reserve(packed_length);
        for row in 0..height as usize {
            let start = row * stride;
            packed_storage.extend_from_slice(&input[start..start + row_length]);
        }
        packed_storage.as_slice()
    };

    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_deflate_compression(png::DeflateCompression::Level(compression as u8));
        encoder.set_filter(png::Filter::Paeth);
        let mut writer = encoder
            .write_header()
            .map_err(|_| "png.encode could not write the PNG header")?;
        writer
            .write_image_data(pixels)
            .map_err(|_| "png.encode could not write image data")?;
        writer
            .finish()
            .map_err(|_| "png.encode could not finish the PNG")?;
    }
    Ok(encoded)
}

fn diagnostic(result: &mut TimaValue, message: &'static str) {
    result.words = [
        VALUE_DIAGNOSTIC,
        message.as_ptr() as usize as u32,
        message.len() as u32,
        0,
        0,
        0,
        0,
        0,
    ];
}
