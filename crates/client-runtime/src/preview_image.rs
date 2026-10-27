use image::{
    DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, Limits,
    codecs::{
        jpeg::JpegDecoder,
        png::{PngDecoder, PngEncoder},
        webp::WebPDecoder,
    },
};
use std::io::{self, Cursor, Write};

const MAX_INPUT_BYTES: usize = 25 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 25 * 1024 * 1024;
const MAX_SIDE: u32 = 8192;
const MAX_PIXELS: u64 = 16_000_000;
const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_WEBP_EXIF_BYTES: usize = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct ValidatedPreviewImage {
    pub png_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for ValidatedPreviewImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatedPreviewImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("png_bytes_len", &self.png_bytes.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewImageError {
    TooLarge,
    Unsupported,
    Invalid,
    Dimensions,
    AnimationUnsupported,
    OutputTooLarge,
    DecodeLimit,
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    limits
}

fn decode_error(error: image::ImageError) -> PreviewImageError {
    match error {
        image::ImageError::Limits(_) => PreviewImageError::Dimensions,
        _ => PreviewImageError::Invalid,
    }
}

fn validate_webp_exif_size(bytes: &[u8]) -> Result<(), PreviewImageError> {
    let size = u32::from_le_bytes(
        bytes
            .get(4..8)
            .ok_or(PreviewImageError::Invalid)?
            .try_into()
            .map_err(|_| PreviewImageError::Invalid)?,
    ) as usize;
    let end = size
        .checked_add(8)
        .filter(|end| *end >= 12 && *end <= bytes.len())
        .ok_or(PreviewImageError::Invalid)?;
    let mut offset = 12usize;
    while offset < end {
        let header = bytes
            .get(offset..offset.checked_add(8).ok_or(PreviewImageError::Invalid)?)
            .ok_or(PreviewImageError::Invalid)?;
        let length = u32::from_le_bytes(
            header[4..8]
                .try_into()
                .map_err(|_| PreviewImageError::Invalid)?,
        ) as usize;
        let data_end = offset
            .checked_add(8)
            .and_then(|offset| offset.checked_add(length))
            .ok_or(PreviewImageError::Invalid)?;
        let padded_end = data_end
            .checked_add(length % 2)
            .filter(|offset| *offset <= end)
            .ok_or(PreviewImageError::Invalid)?;
        if &header[..4] == b"EXIF" && length > MAX_WEBP_EXIF_BYTES {
            return Err(PreviewImageError::DecodeLimit);
        }
        offset = padded_end;
    }
    Ok(())
}

struct LimitedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for LimitedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_OUTPUT_BYTES.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("preview output limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn decode_preview<D: ImageDecoder>(
    mut decoder: D,
) -> Result<ValidatedPreviewImage, PreviewImageError> {
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > MAX_SIDE
        || height > MAX_SIDE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(PreviewImageError::Dimensions);
    }
    let total = decoder.total_bytes();
    if total > MAX_DECODED_BYTES {
        return Err(PreviewImageError::DecodeLimit);
    }
    let mut remaining = limits();
    remaining
        .reserve(total)
        .map_err(|_| PreviewImageError::DecodeLimit)?;
    decoder.set_limits(remaining).map_err(decode_error)?;
    let orientation = decoder.orientation().map_err(decode_error)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    image.apply_orientation(orientation);
    let (width, height) = (image.width(), image.height());
    let mut output = LimitedOutput {
        bytes: Vec::new(),
        exceeded: false,
    };
    let result = PngEncoder::new(&mut output).write_image(
        image.as_bytes(),
        width,
        height,
        image.color().into(),
    );
    if output.exceeded {
        return Err(PreviewImageError::OutputTooLarge);
    }
    result.map_err(|_| PreviewImageError::Invalid)?;
    Ok(ValidatedPreviewImage {
        png_bytes: output.bytes,
        width,
        height,
    })
}

pub fn validate_preview_image(bytes: &[u8]) -> Result<ValidatedPreviewImage, PreviewImageError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(PreviewImageError::TooLarge);
    }
    let format = image::guess_format(bytes).map_err(|_| PreviewImageError::Unsupported)?;
    match format {
        ImageFormat::Png => {
            let decoder =
                PngDecoder::with_limits(Cursor::new(bytes), limits()).map_err(decode_error)?;
            if decoder.is_apng().map_err(decode_error)? {
                return Err(PreviewImageError::AnimationUnsupported);
            }
            decode_preview(decoder)
        }
        ImageFormat::Jpeg => {
            decode_preview(JpegDecoder::new(Cursor::new(bytes)).map_err(decode_error)?)
        }
        ImageFormat::WebP => {
            validate_webp_exif_size(bytes)?;
            let decoder = WebPDecoder::new(Cursor::new(bytes)).map_err(decode_error)?;
            if decoder.has_animation() {
                return Err(PreviewImageError::AnimationUnsupported);
            }
            decode_preview(decoder)
        }
        _ => Err(PreviewImageError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_output_rejects_a_write_beyond_its_bound() {
        let mut output = LimitedOutput {
            bytes: vec![0; MAX_OUTPUT_BYTES],
            exceeded: false,
        };
        assert!(output.write_all(&[1]).is_err());
        assert!(output.exceeded);
        assert_eq!(output.bytes.len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn debug_output_does_not_include_image_bytes() {
        let image = ValidatedPreviewImage {
            png_bytes: b"private".to_vec(),
            width: 2,
            height: 3,
        };
        assert_eq!(
            format!("{image:?}"),
            "ValidatedPreviewImage { width: 2, height: 3, png_bytes_len: 7 }"
        );
    }
}
