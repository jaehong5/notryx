use base64::{Engine, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use notryx_client_runtime::{PreviewImageError, validate_preview_image};
use std::io::Cursor;

fn fixture(format: ImageFormat) -> Vec<u8> {
    let pixels = RgbaImage::from_fn(2, 2, |x, y| Rgba([(x * 80) as u8, (y * 80) as u8, 64, 255]));
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(pixels)
        .write_to(&mut bytes, format)
        .unwrap();
    bytes.into_inner()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

#[test]
fn supported_formats_are_decoded_and_reencoded_as_passive_png_without_mutating_source() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
        let mut bytes = fixture(format);
        let original = bytes.clone();
        bytes.extend_from_slice(b"<svg onload=alert(1)>untrusted trailing content</svg>");
        let preview = validate_preview_image(&bytes).unwrap();
        assert_eq!((preview.width, preview.height), (2, 2));
        assert_eq!(
            image::guess_format(&preview.png_bytes).unwrap(),
            ImageFormat::Png
        );
        assert_eq!(
            image::load_from_memory(&preview.png_bytes).unwrap().width(),
            2
        );
        assert!(
            !preview
                .png_bytes
                .windows(b"<svg".len())
                .any(|part| part == b"<svg")
        );
        assert_eq!(&bytes[..original.len()], original);
    }
}

#[test]
fn rejects_untrusted_or_corrupt_formats_and_oversized_input() {
    for bytes in [
        &b"<svg onload='alert(1)'></svg>"[..],
        &b"%PDF-1.7"[..],
        &b"<html>"[..],
        &b""[..],
    ] {
        assert!(validate_preview_image(bytes).is_err());
    }
    let png = fixture(ImageFormat::Png);
    assert!(validate_preview_image(&png[..png.len() / 2]).is_err());
    assert_eq!(
        validate_preview_image(&vec![0; 25 * 1024 * 1024 + 1]),
        Err(PreviewImageError::TooLarge)
    );
}

#[test]
fn rejects_dimensions_and_pixel_count_before_decompression() {
    for (width, height) in [(8193u32, 1u32), (4001, 4000)] {
        let mut png = fixture(ImageFormat::Png);
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(
            validate_preview_image(&png),
            Err(PreviewImageError::Dimensions)
        );
    }
}

#[test]
fn animated_png_and_webp_are_not_silently_rendered_as_static_images() {
    let mut apng = fixture(ImageFormat::Png);
    let mut chunk = Vec::from(&b"acTL"[..]);
    chunk.extend_from_slice(&2u32.to_be_bytes());
    chunk.extend_from_slice(&0u32.to_be_bytes());
    let mut encoded_chunk = 8u32.to_be_bytes().to_vec();
    encoded_chunk.extend_from_slice(&chunk);
    encoded_chunk.extend_from_slice(&crc32(&chunk).to_be_bytes());
    apng.splice(33..33, encoded_chunk);
    assert_eq!(
        validate_preview_image(&apng),
        Err(PreviewImageError::AnimationUnsupported)
    );
    let animated_webp = STANDARD.decode("UklGRsAAAABXRUJQVlA4WAoAAAACAAAAAQAAAQAAQU5JTQYAAAD/////AABBTk1GSAAAAAAAAAAAAAEAAAEAAGQAAAJWUDggMAAAANABAJ0BKgIAAgACADQloAJ0ugH4AAOwAP7wxAv/ILlhdcjX/yA/5Af8gP/48gAAAEFOTUZEAAAAAAAAAAAAAQAAAQAAZAAAAFZQOCAsAAAAlAEAnQEqAgACAAAANCWgAnS6AAOYAP75k2//kB//kB//kB//ID/iF3sgMAA=").unwrap();
    assert_eq!(
        validate_preview_image(&animated_webp),
        Err(PreviewImageError::AnimationUnsupported)
    );
}

#[test]
fn jpeg_orientation_is_applied_before_metadata_is_removed() {
    let pixels = image::RgbImage::from_fn(2, 3, |x, y| {
        image::Rgb([(x * 80) as u8, (y * 80) as u8, 64])
    });
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(pixels)
        .write_to(&mut encoded, ImageFormat::Jpeg)
        .unwrap();
    let mut bytes = encoded.into_inner();
    let app1: [u8; 36] = [
        0xff, 0xe1, 0x00, 0x22, b'E', b'x', b'i', b'f', 0, 0, b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0,
        0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
    ];
    bytes.splice(2..2, app1);
    let preview = validate_preview_image(&bytes).unwrap();
    assert_eq!((preview.width, preview.height), (3, 2));
    assert!(
        !preview
            .png_bytes
            .windows(6)
            .any(|value| value == b"Exif\0\0")
    );
}

#[test]
fn webp_rejects_an_exif_chunk_over_the_metadata_limit() {
    let simple = fixture(ImageFormat::WebP);
    let mut webp = Vec::from(&b"RIFF\0\0\0\0WEBP"[..]);
    webp.extend_from_slice(b"VP8X");
    webp.extend_from_slice(&10u32.to_le_bytes());
    webp.extend_from_slice(&[0x08, 0, 0, 0, 1, 0, 0, 1, 0, 0]);
    webp.extend_from_slice(&simple[12..]);
    webp.extend_from_slice(b"EXIF");
    let exif_len = 1024 * 1024 + 1;
    webp.extend_from_slice(&(exif_len as u32).to_le_bytes());
    webp.extend(std::iter::repeat_n(0, exif_len + 1));
    let size = (webp.len() - 8) as u32;
    webp[4..8].copy_from_slice(&size.to_le_bytes());
    assert_eq!(
        validate_preview_image(&webp),
        Err(PreviewImageError::DecodeLimit)
    );
}
