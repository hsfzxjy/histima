use std::io::Cursor;
use std::mem;
use std::slice;

const PLUGIN_ABI_VERSION: u32 = 3;
const VALUE_BYTES_VIEW: u32 = 1;
const VALUE_IMAGE: u32 = 5;
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
    let decoded = match decode(arguments_pointer, argument_count) {
        Ok(decoded) => decoded,
        Err(message) => {
            diagnostic(result, message);
            return 0;
        }
    };
    let DecodedImage {
        width,
        height,
        stride,
        mut pixels,
    } = decoded;
    let pointer = pixels.as_mut_ptr() as usize as u32;
    let length = pixels.len() as u32;
    mem::forget(pixels);
    result.words = [
        VALUE_IMAGE,
        pointer,
        length,
        IMAGE_FORMAT_RGBA8,
        width,
        height,
        stride,
        0,
    ];
    0
}

struct DecodedImage {
    width: u32,
    height: u32,
    stride: u32,
    pixels: Vec<u8>,
}

fn decode(arguments_pointer: u32, argument_count: u32) -> Result<DecodedImage, String> {
    if argument_count != 1 {
        return Err("png.decode expects one argument".to_owned());
    }
    let arguments = unsafe {
        slice::from_raw_parts(
            arguments_pointer as usize as *const TimaValue,
            argument_count as usize,
        )
    };
    let bytes = arguments[0];
    if bytes.words[0] != VALUE_BYTES_VIEW || bytes.words[3..].iter().any(|word| *word != 0) {
        return Err("png.decode expects a BytesView".to_owned());
    }
    let input = unsafe {
        slice::from_raw_parts(
            bytes.words[1] as usize as *const u8,
            bytes.words[2] as usize,
        )
    };

    let mut decoder = png::Decoder::new(Cursor::new(input));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("png.decode could not read PNG metadata: {error}"))?;
    if reader.info().animation_control.is_some() {
        return Err("png.decode does not support animated PNG images".to_owned());
    }
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| "png.decode image dimensions exceed this runtime".to_owned())?;
    let mut decoded = vec![0; buffer_size];
    let output = reader
        .next_frame(&mut decoded)
        .map_err(|error| format!("png.decode could not decode image data: {error}"))?;
    if output.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "png.decode produced unsupported {:?} channel depth",
            output.bit_depth
        ));
    }

    let width = output.width as usize;
    let height = output.height as usize;
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| "png.decode pixel count overflows usize".to_owned())?;
    let rgba_length = pixels
        .checked_mul(4)
        .ok_or_else(|| "png.decode RGBA byte length overflows usize".to_owned())?;
    let channels = output.color_type.samples();
    let expected_line = width
        .checked_mul(channels)
        .ok_or_else(|| "png.decode row byte length overflows usize".to_owned())?;
    if output.line_size != expected_line {
        return Err("png.decode returned an inconsistent row layout".to_owned());
    }

    let decoded = &decoded[..output.buffer_size()];
    let mut rgba = Vec::with_capacity(rgba_length);
    for pixel in decoded.chunks_exact(channels) {
        match output.color_type {
            png::ColorType::Grayscale => {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]);
            }
            png::ColorType::GrayscaleAlpha => {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            png::ColorType::Rgb => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
            png::ColorType::Rgba => rgba.extend_from_slice(pixel),
            png::ColorType::Indexed => {
                return Err("png.decode palette expansion did not produce RGB pixels".to_owned());
            }
        }
    }
    if rgba.len() != rgba_length {
        return Err("png.decode returned an incomplete pixel buffer".to_owned());
    }
    let stride = output
        .width
        .checked_mul(4)
        .ok_or_else(|| "png.decode RGBA stride overflows u32".to_owned())?;
    Ok(DecodedImage {
        width: output.width,
        height: output.height,
        stride,
        pixels: rgba,
    })
}

fn diagnostic(result: &mut TimaValue, message: String) {
    let mut bytes = message.into_bytes();
    let pointer = bytes.as_mut_ptr() as usize as u32;
    let length = bytes.len() as u32;
    mem::forget(bytes);
    result.words = [VALUE_DIAGNOSTIC, pointer, length, 0, 0, 0, 0, 0];
}
