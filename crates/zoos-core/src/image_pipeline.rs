use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType as ResizeFilter;
use image::{
    ColorType, DynamicImage, GenericImageView, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, RgbImage, RgbaImage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataPolicy {
    Preserve,
    Strip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputEncoding {
    Png,
    Jpeg,
    Webp,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageMetadata {
    pub icc: Option<Vec<u8>>,
    /// TIFF bytes without an `Exif\0\0` container prefix.
    pub exif: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePipelineLimits {
    pub max_input_bytes: u64,
    pub max_icc_bytes: usize,
    pub max_exif_bytes: usize,
    pub max_metadata_bytes: usize,
    pub max_pixels: u64,
}

impl Default for ImagePipelineLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 512 * 1024 * 1024,
            max_icc_bytes: 4 * 1024 * 1024,
            max_exif_bytes: 4 * 1024 * 1024,
            max_metadata_bytes: 8 * 1024 * 1024,
            max_pixels: 100_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    pub input_path: PathBuf,
    pub inference_png: PathBuf,
    pub alpha_png: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    pub had_alpha: bool,
    pub orientation: u16,
    pub metadata: ImageMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPipelineOutput {
    pub width: u32,
    pub height: u32,
    pub format: OutputEncoding,
    pub has_alpha: bool,
    pub icc_preserved: bool,
    pub exif_preserved: bool,
    pub sha256: String,
}

pub fn prepare_image_input(
    input: &Path,
    workspace: &Path,
    limits: ImagePipelineLimits,
) -> Result<PreparedImage, Goal1bImageError> {
    let file_meta = fs::symlink_metadata(input)?;
    if !input.is_absolute() || !file_meta.is_file() || file_meta.file_type().is_symlink() {
        return Err(Goal1bImageError::InvalidInput);
    }
    if file_meta.len() > limits.max_input_bytes {
        return Err(Goal1bImageError::InputTooLarge);
    }
    let bytes = fs::read(input)?;
    let format = image::guess_format(&bytes).map_err(|_| Goal1bImageError::UnsupportedFormat)?;
    let mut metadata = match format {
        ImageFormat::Png => metadata_from_png(&bytes, limits)?,
        ImageFormat::Jpeg => metadata_from_jpeg(&bytes, limits)?,
        _ => return Err(Goal1bImageError::UnsupportedFormat),
    };
    let orientation = metadata
        .exif
        .as_deref()
        .map(exif_orientation)
        .transpose()?
        .unwrap_or(1);
    if !(1..=8).contains(&orientation) {
        return Err(Goal1bImageError::InvalidExif);
    }

    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| Goal1bImageError::Decode)?;
    let decoder = reader
        .into_decoder()
        .map_err(|_| Goal1bImageError::Decode)?;
    let (raw_width, raw_height) = decoder.dimensions();
    if raw_width == 0
        || raw_height == 0
        || u64::from(raw_width) * u64::from(raw_height) > limits.max_pixels
    {
        return Err(Goal1bImageError::ImageTooLarge);
    }
    if !matches!(decoder.color_type(), ColorType::Rgb8 | ColorType::Rgba8) {
        return Err(Goal1bImageError::UnsupportedImageMode);
    }
    drop(decoder);
    let decoded = image::load_from_memory_with_format(&bytes, format)
        .map_err(|_| Goal1bImageError::Decode)?;
    let had_alpha = matches!(decoded, DynamicImage::ImageRgba8(_));
    let oriented = match decoded {
        DynamicImage::ImageRgb8(image) => DynamicImage::ImageRgb8(orient_rgb(image, orientation)),
        DynamicImage::ImageRgba8(image) => {
            DynamicImage::ImageRgba8(orient_rgba(image, orientation))
        }
        _ => return Err(Goal1bImageError::UnsupportedImageMode),
    };
    if let Some(exif) = metadata.exif.as_mut() {
        normalize_exif_orientation(exif)?;
    }
    fs::create_dir_all(workspace)?;
    let inference_png = workspace.join("inference-rgb.png");
    let alpha_png = had_alpha.then(|| workspace.join("alpha.png"));
    let rgb = oriented.to_rgb8();
    encode_png(
        &inference_png,
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        ColorType::Rgb8,
        &ImageMetadata::default(),
    )?;
    if let Some(path) = alpha_png.as_ref() {
        let rgba = oriented.to_rgba8();
        let alpha: Vec<u8> = rgba.pixels().map(|pixel| pixel[3]).collect();
        encode_png(
            path,
            &alpha,
            rgba.width(),
            rgba.height(),
            ColorType::L8,
            &ImageMetadata::default(),
        )?;
    }
    Ok(PreparedImage {
        input_path: input.to_owned(),
        inference_png,
        alpha_png,
        width: rgb.width(),
        height: rgb.height(),
        had_alpha,
        orientation,
        metadata,
    })
}

pub fn render_pipeline_output(
    prepared: &PreparedImage,
    runner_x4_png: &Path,
    partial: &Path,
    scale: u8,
    format: OutputEncoding,
    policy: MetadataPolicy,
    limits: ImagePipelineLimits,
) -> Result<VerifiedPipelineOutput, Goal1bImageError> {
    if !matches!(scale, 2 | 4) {
        return Err(Goal1bImageError::UnsupportedScale);
    }
    if prepared.had_alpha && format == OutputEncoding::Jpeg {
        return Err(Goal1bImageError::AlphaJpegUnsupported);
    }
    let x4 = (
        prepared
            .width
            .checked_mul(4)
            .ok_or(Goal1bImageError::ImageTooLarge)?,
        prepared
            .height
            .checked_mul(4)
            .ok_or(Goal1bImageError::ImageTooLarge)?,
    );
    if fs::metadata(runner_x4_png)?.len() > limits.max_input_bytes {
        return Err(Goal1bImageError::InvalidRunnerOutput);
    }
    let runner_bytes = fs::read(runner_x4_png)?;
    let runner_reader = ImageReader::new(Cursor::new(&runner_bytes))
        .with_guessed_format()
        .map_err(|_| Goal1bImageError::InvalidRunnerOutput)?;
    if runner_reader.format() != Some(ImageFormat::Png) {
        return Err(Goal1bImageError::InvalidRunnerOutput);
    }
    let runner_decoder = runner_reader
        .into_decoder()
        .map_err(|_| Goal1bImageError::InvalidRunnerOutput)?;
    if runner_decoder.color_type() != ColorType::Rgb8 || runner_decoder.dimensions() != x4 {
        return Err(Goal1bImageError::InvalidRunnerOutput);
    }
    drop(runner_decoder);
    let runner = image::load_from_memory_with_format(&runner_bytes, ImageFormat::Png)
        .map_err(|_| Goal1bImageError::InvalidRunnerOutput)?;
    let runner = match runner {
        DynamicImage::ImageRgb8(value) => value,
        _ => return Err(Goal1bImageError::InvalidRunnerOutput),
    };
    if runner.dimensions() != x4 {
        return Err(Goal1bImageError::InvalidRunnerOutput);
    }
    let target = (
        prepared
            .width
            .checked_mul(u32::from(scale))
            .ok_or(Goal1bImageError::ImageTooLarge)?,
        prepared
            .height
            .checked_mul(u32::from(scale))
            .ok_or(Goal1bImageError::ImageTooLarge)?,
    );
    if u64::from(target.0) * u64::from(target.1) > limits.max_pixels {
        return Err(Goal1bImageError::ImageTooLarge);
    }
    let rgb = if scale == 2 {
        image::imageops::resize(&runner, target.0, target.1, ResizeFilter::Lanczos3)
    } else {
        runner
    };
    let pixels = if let Some(alpha_path) = prepared.alpha_png.as_ref() {
        let alpha = image::open(alpha_path)
            .map_err(|_| Goal1bImageError::InvalidAlpha)?
            .into_luma8();
        if alpha.dimensions() != (prepared.width, prepared.height) {
            return Err(Goal1bImageError::InvalidAlpha);
        }
        let alpha = image::imageops::resize(&alpha, target.0, target.1, ResizeFilter::Lanczos3);
        let mut rgba = RgbaImage::new(target.0, target.1);
        for ((destination, source), opacity) in
            rgba.pixels_mut().zip(rgb.pixels()).zip(alpha.pixels())
        {
            *destination = image::Rgba([source[0], source[1], source[2], opacity[0]]);
        }
        DynamicImage::ImageRgba8(rgba)
    } else {
        DynamicImage::ImageRgb8(rgb)
    };
    let metadata = if policy == MetadataPolicy::Preserve {
        &prepared.metadata
    } else {
        &ImageMetadata::default()
    };
    encode_output(partial, &pixels, format, metadata)?;
    verify_pipeline_output(partial, prepared, scale, format, policy, limits)
}

pub fn verify_pipeline_output(
    partial: &Path,
    prepared: &PreparedImage,
    scale: u8,
    format: OutputEncoding,
    policy: MetadataPolicy,
    limits: ImagePipelineLimits,
) -> Result<VerifiedPipelineOutput, Goal1bImageError> {
    if !matches!(scale, 2 | 4) {
        return Err(Goal1bImageError::UnsupportedScale);
    }
    if fs::metadata(partial)?.len() > limits.max_input_bytes {
        return Err(Goal1bImageError::ImageTooLarge);
    }
    let bytes = fs::read(partial)?;
    let detected = image::guess_format(&bytes).map_err(|_| Goal1bImageError::InvalidOutput)?;
    let actual_format = match detected {
        ImageFormat::Png => OutputEncoding::Png,
        ImageFormat::Jpeg => OutputEncoding::Jpeg,
        ImageFormat::WebP => OutputEncoding::Webp,
        _ => return Err(Goal1bImageError::InvalidOutput),
    };
    if actual_format != format {
        return Err(Goal1bImageError::InvalidOutput);
    }
    let expected = (
        prepared
            .width
            .checked_mul(u32::from(scale))
            .ok_or(Goal1bImageError::ImageTooLarge)?,
        prepared
            .height
            .checked_mul(u32::from(scale))
            .ok_or(Goal1bImageError::ImageTooLarge)?,
    );
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| Goal1bImageError::InvalidOutput)?;
    let decoder = reader
        .into_decoder()
        .map_err(|_| Goal1bImageError::InvalidOutput)?;
    if decoder.dimensions() != expected
        || !matches!(decoder.color_type(), ColorType::Rgb8 | ColorType::Rgba8)
    {
        return Err(Goal1bImageError::InvalidOutput);
    }
    drop(decoder);
    let decoded = image::load_from_memory_with_format(&bytes, detected)
        .map_err(|_| Goal1bImageError::InvalidOutput)?;
    let has_alpha = matches!(decoded, DynamicImage::ImageRgba8(_));
    if has_alpha != prepared.had_alpha
        || (!has_alpha && !matches!(decoded, DynamicImage::ImageRgb8(_)))
    {
        return Err(Goal1bImageError::InvalidOutput);
    }
    let actual_metadata = match format {
        OutputEncoding::Png => metadata_from_png(&bytes, limits)?,
        OutputEncoding::Jpeg => metadata_from_jpeg(&bytes, limits)?,
        OutputEncoding::Webp => metadata_from_webp(&bytes, limits)?,
    };
    let expected_metadata = if policy == MetadataPolicy::Preserve {
        &prepared.metadata
    } else {
        &ImageMetadata::default()
    };
    if &actual_metadata != expected_metadata {
        return Err(Goal1bImageError::MetadataMismatch);
    }
    Ok(VerifiedPipelineOutput {
        width: expected.0,
        height: expected.1,
        format,
        has_alpha,
        icc_preserved: actual_metadata.icc.is_some(),
        exif_preserved: actual_metadata.exif.is_some(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    })
}

fn orient_rgb(image: RgbImage, orientation: u16) -> RgbImage {
    orient(image, orientation)
}
fn orient_rgba(image: RgbaImage, orientation: u16) -> RgbaImage {
    orient(image, orientation)
}
fn orient<P>(
    image: image::ImageBuffer<P, Vec<u8>>,
    orientation: u16,
) -> image::ImageBuffer<P, Vec<u8>>
where
    P: image::Pixel<Subpixel = u8> + 'static,
{
    match orientation {
        1 => image,
        2 => image::imageops::flip_horizontal(&image),
        3 => image::imageops::rotate180(&image),
        4 => image::imageops::flip_vertical(&image),
        5 => image::imageops::rotate270(&image::imageops::flip_horizontal(&image)),
        6 => image::imageops::rotate90(&image),
        7 => image::imageops::rotate90(&image::imageops::flip_horizontal(&image)),
        8 => image::imageops::rotate270(&image),
        _ => image,
    }
}

fn encode_output(
    path: &Path,
    image: &DynamicImage,
    format: OutputEncoding,
    metadata: &ImageMetadata,
) -> Result<(), Goal1bImageError> {
    let (width, height) = image.dimensions();
    let (bytes, color) = match image {
        DynamicImage::ImageRgb8(value) => (value.as_raw().as_slice(), ColorType::Rgb8),
        DynamicImage::ImageRgba8(value) => (value.as_raw().as_slice(), ColorType::Rgba8),
        _ => return Err(Goal1bImageError::InvalidOutput),
    };
    match format {
        OutputEncoding::Png => encode_png(path, bytes, width, height, color, metadata),
        OutputEncoding::Jpeg => {
            let mut encoded = Vec::new();
            JpegEncoder::new_with_quality(&mut encoded, 95)
                .write_image(bytes, width, height, color.into())
                .map_err(|_| Goal1bImageError::Encode)?;
            inject_jpeg_metadata(&mut encoded, metadata)?;
            fs::write(path, encoded)?;
            Ok(())
        }
        OutputEncoding::Webp => {
            let mut encoded = Vec::new();
            WebPEncoder::new_lossless(&mut encoded)
                .write_image(bytes, width, height, color.into())
                .map_err(|_| Goal1bImageError::Encode)?;
            fs::write(
                path,
                inject_webp_metadata(&encoded, width, height, color.has_alpha(), metadata)?,
            )?;
            Ok(())
        }
    }
}

fn encode_png(
    path: &Path,
    bytes: &[u8],
    width: u32,
    height: u32,
    color: ColorType,
    metadata: &ImageMetadata,
) -> Result<(), Goal1bImageError> {
    let mut encoded = Vec::new();
    PngEncoder::new_with_quality(&mut encoded, CompressionType::Default, FilterType::Adaptive)
        .write_image(bytes, width, height, color.into())
        .map_err(|_| Goal1bImageError::Encode)?;
    let mut chunks = Vec::new();
    if let Some(icc) = metadata.icc.as_ref() {
        let mut payload = b"zoos\0\0".to_vec();
        let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
        zlib.write_all(icc)?;
        payload.extend(zlib.finish()?);
        chunks.push((*b"iCCP", payload));
    }
    if let Some(exif) = metadata.exif.as_ref() {
        chunks.push((*b"eXIf", exif.clone()));
    }
    fs::write(path, inject_png_chunks(&encoded, &chunks)?)?;
    Ok(())
}

fn inject_png_chunks(
    png: &[u8],
    chunks: &[([u8; 4], Vec<u8>)],
) -> Result<Vec<u8>, Goal1bImageError> {
    if !png.starts_with(PNG_SIGNATURE) || png.len() < 33 {
        return Err(Goal1bImageError::Encode);
    }
    let ihdr_end = 8
        + 12
        + usize::try_from(u32::from_be_bytes(png[8..12].try_into().unwrap()))
            .map_err(|_| Goal1bImageError::Encode)?;
    let mut out = png[..ihdr_end].to_vec();
    for (kind, payload) in chunks {
        append_png_chunk(&mut out, kind, payload)?;
    }
    out.extend_from_slice(&png[ihdr_end..]);
    Ok(out)
}

fn append_png_chunk(
    out: &mut Vec<u8>,
    kind: &[u8; 4],
    payload: &[u8],
) -> Result<(), Goal1bImageError> {
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| Goal1bImageError::MetadataTooLarge)?
            .to_be_bytes(),
    );
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(payload);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
    Ok(())
}

fn metadata_from_png(
    bytes: &[u8],
    limits: ImagePipelineLimits,
) -> Result<ImageMetadata, Goal1bImageError> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(Goal1bImageError::Decode);
    }
    let mut at = 8;
    let mut metadata = ImageMetadata::default();
    while at + 12 <= bytes.len() {
        let len = usize::try_from(u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()))
            .map_err(|_| Goal1bImageError::MetadataTooLarge)?;
        let end = at
            .checked_add(12)
            .and_then(|v| v.checked_add(len))
            .ok_or(Goal1bImageError::MetadataTooLarge)?;
        if end > bytes.len() {
            return Err(Goal1bImageError::Decode);
        }
        let kind = &bytes[at + 4..at + 8];
        let payload = &bytes[at + 8..at + 8 + len];
        match kind {
            b"iCCP" => {
                if metadata.icc.is_some() {
                    return Err(Goal1bImageError::InvalidMetadata);
                }
                let nul = payload
                    .iter()
                    .position(|v| *v == 0)
                    .ok_or(Goal1bImageError::InvalidMetadata)?;
                if nul + 2 > payload.len() || payload[nul + 1] != 0 {
                    return Err(Goal1bImageError::InvalidMetadata);
                }
                metadata.icc = Some(read_zlib_limited(
                    &payload[nul + 2..],
                    limits.max_icc_bytes,
                )?);
            }
            b"eXIf" => {
                if payload.len() > limits.max_exif_bytes {
                    return Err(Goal1bImageError::MetadataTooLarge);
                }
                set_unique_metadata(&mut metadata.exif, payload.to_vec())?;
            }
            _ => {}
        }
        at = end;
        if kind == b"IEND" {
            break;
        }
    }
    validate_metadata(&metadata, limits)?;
    Ok(metadata)
}

fn metadata_from_jpeg(
    bytes: &[u8],
    limits: ImagePipelineLimits,
) -> Result<ImageMetadata, Goal1bImageError> {
    if !bytes.starts_with(b"\xff\xd8") {
        return Err(Goal1bImageError::Decode);
    }
    let mut at = 2;
    let mut metadata = ImageMetadata::default();
    let mut icc_parts: Vec<(u8, u8, Vec<u8>)> = Vec::new();
    while at + 4 <= bytes.len() && bytes[at] == 0xff {
        while at < bytes.len() && bytes[at] == 0xff {
            at += 1;
        }
        let marker = *bytes.get(at).ok_or(Goal1bImageError::Decode)?;
        at += 1;
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let len = usize::from(u16::from_be_bytes(
            bytes
                .get(at..at + 2)
                .ok_or(Goal1bImageError::Decode)?
                .try_into()
                .unwrap(),
        ));
        if len < 2 || at + len > bytes.len() {
            return Err(Goal1bImageError::Decode);
        }
        let payload = &bytes[at + 2..at + len];
        if marker == 0xe1 && payload.starts_with(b"Exif\0\0") {
            if payload.len() - 6 > limits.max_exif_bytes {
                return Err(Goal1bImageError::MetadataTooLarge);
            }
            set_unique_metadata(&mut metadata.exif, payload[6..].to_vec())?;
        }
        if marker == 0xe2 && payload.starts_with(b"ICC_PROFILE\0") && payload.len() >= 14 {
            let accumulated = icc_parts
                .iter()
                .map(|(_, _, part)| part.len())
                .sum::<usize>()
                .checked_add(payload.len() - 14)
                .ok_or(Goal1bImageError::MetadataTooLarge)?;
            if accumulated > limits.max_icc_bytes {
                return Err(Goal1bImageError::MetadataTooLarge);
            }
            icc_parts.push((payload[12], payload[13], payload[14..].to_vec()));
        }
        at += len;
    }
    if !icc_parts.is_empty() {
        let count = icc_parts[0].1;
        if count == 0
            || icc_parts.len() != usize::from(count)
            || icc_parts.iter().any(|(_, c, _)| *c != count)
        {
            return Err(Goal1bImageError::InvalidMetadata);
        }
        icc_parts.sort_by_key(|(index, _, _)| *index);
        if icc_parts
            .iter()
            .enumerate()
            .any(|(i, (index, _, _))| usize::from(*index) != i + 1)
        {
            return Err(Goal1bImageError::InvalidMetadata);
        }
        metadata.icc = Some(
            icc_parts
                .into_iter()
                .flat_map(|(_, _, part)| part)
                .collect(),
        );
    }
    validate_metadata(&metadata, limits)?;
    Ok(metadata)
}

fn inject_jpeg_metadata(
    jpeg: &mut Vec<u8>,
    metadata: &ImageMetadata,
) -> Result<(), Goal1bImageError> {
    if !jpeg.starts_with(b"\xff\xd8") {
        return Err(Goal1bImageError::Encode);
    }
    let mut segments = Vec::new();
    if let Some(exif) = metadata.exif.as_ref() {
        let mut p = b"Exif\0\0".to_vec();
        p.extend(exif);
        append_jpeg_segment(&mut segments, 0xe1, &p)?;
    }
    if let Some(icc) = metadata.icc.as_ref() {
        const CHUNK: usize = 65_519;
        let count = icc.len().div_ceil(CHUNK);
        if count > 255 {
            return Err(Goal1bImageError::MetadataTooLarge);
        }
        for (index, part) in icc.chunks(CHUNK).enumerate() {
            let mut p = b"ICC_PROFILE\0".to_vec();
            p.push((index + 1) as u8);
            p.push(count as u8);
            p.extend(part);
            append_jpeg_segment(&mut segments, 0xe2, &p)?;
        }
    }
    jpeg.splice(2..2, segments);
    Ok(())
}

fn append_jpeg_segment(
    out: &mut Vec<u8>,
    marker: u8,
    payload: &[u8],
) -> Result<(), Goal1bImageError> {
    let len = u16::try_from(payload.len() + 2).map_err(|_| Goal1bImageError::MetadataTooLarge)?;
    out.extend_from_slice(&[0xff, marker]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

fn inject_webp_metadata(
    webp: &[u8],
    width: u32,
    height: u32,
    alpha: bool,
    metadata: &ImageMetadata,
) -> Result<Vec<u8>, Goal1bImageError> {
    if webp.len() < 12 || &webp[..4] != b"RIFF" || &webp[8..12] != b"WEBP" {
        return Err(Goal1bImageError::Encode);
    }
    let mut body = Vec::new();
    let flags = (if metadata.icc.is_some() { 0x20 } else { 0 })
        | (if alpha { 0x10 } else { 0 })
        | (if metadata.exif.is_some() { 0x08 } else { 0 });
    let mut vp8x = vec![flags, 0, 0, 0];
    vp8x.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    vp8x.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    append_riff_chunk(&mut body, b"VP8X", &vp8x)?;
    if let Some(icc) = metadata.icc.as_ref() {
        append_riff_chunk(&mut body, b"ICCP", icc)?;
    }
    body.extend_from_slice(&webp[12..]);
    if let Some(exif) = metadata.exif.as_ref() {
        append_riff_chunk(&mut body, b"EXIF", exif)?;
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(
        &u32::try_from(body.len() + 4)
            .map_err(|_| Goal1bImageError::Encode)?
            .to_le_bytes(),
    );
    out.extend_from_slice(b"WEBP");
    out.extend(body);
    Ok(out)
}

fn append_riff_chunk(
    out: &mut Vec<u8>,
    kind: &[u8; 4],
    payload: &[u8],
) -> Result<(), Goal1bImageError> {
    out.extend_from_slice(kind);
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| Goal1bImageError::MetadataTooLarge)?
            .to_le_bytes(),
    );
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0)
    }
    Ok(())
}

fn metadata_from_webp(
    bytes: &[u8],
    limits: ImagePipelineLimits,
) -> Result<ImageMetadata, Goal1bImageError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(Goal1bImageError::Decode);
    }
    let mut at = 12;
    let mut metadata = ImageMetadata::default();
    while at + 8 <= bytes.len() {
        let kind = &bytes[at..at + 4];
        let len = usize::try_from(u32::from_le_bytes(
            bytes[at + 4..at + 8].try_into().unwrap(),
        ))
        .map_err(|_| Goal1bImageError::MetadataTooLarge)?;
        let end = at
            .checked_add(8)
            .and_then(|v| v.checked_add(len))
            .ok_or(Goal1bImageError::MetadataTooLarge)?;
        if end > bytes.len() {
            return Err(Goal1bImageError::Decode);
        };
        let p = &bytes[at + 8..end];
        match kind {
            b"ICCP" => {
                if p.len() > limits.max_icc_bytes {
                    return Err(Goal1bImageError::MetadataTooLarge);
                }
                if metadata.icc.replace(p.to_vec()).is_some() {
                    return Err(Goal1bImageError::InvalidMetadata);
                }
            }
            b"EXIF" => {
                if p.len() > limits.max_exif_bytes.saturating_add(6) {
                    return Err(Goal1bImageError::MetadataTooLarge);
                }
                set_unique_metadata(
                    &mut metadata.exif,
                    p.strip_prefix(b"Exif\0\0").unwrap_or(p).to_vec(),
                )?;
            }
            _ => {}
        }
        at = end + (len % 2)
    }
    validate_metadata(&metadata, limits)?;
    Ok(metadata)
}

fn validate_metadata(
    metadata: &ImageMetadata,
    limits: ImagePipelineLimits,
) -> Result<(), Goal1bImageError> {
    let icc = metadata.icc.as_ref().map_or(0, Vec::len);
    let exif = metadata.exif.as_ref().map_or(0, Vec::len);
    if icc > limits.max_icc_bytes
        || exif > limits.max_exif_bytes
        || icc
            .checked_add(exif)
            .ok_or(Goal1bImageError::MetadataTooLarge)?
            > limits.max_metadata_bytes
    {
        return Err(Goal1bImageError::MetadataTooLarge);
    }
    Ok(())
}

fn set_unique_metadata(
    destination: &mut Option<Vec<u8>>,
    value: Vec<u8>,
) -> Result<(), Goal1bImageError> {
    if destination.replace(value).is_some() {
        return Err(Goal1bImageError::InvalidMetadata);
    }
    Ok(())
}
fn read_zlib_limited(bytes: &[u8], max: usize) -> Result<Vec<u8>, Goal1bImageError> {
    let mut decoder = ZlibDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .by_ref()
        .take(u64::try_from(max).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut out)
        .map_err(|_| Goal1bImageError::InvalidMetadata)?;
    if out.len() > max {
        return Err(Goal1bImageError::MetadataTooLarge);
    }
    Ok(out)
}

fn exif_orientation(tiff: &[u8]) -> Result<u16, Goal1bImageError> {
    let Some((little, ifd)) = tiff_header(tiff)? else {
        return Ok(1);
    };
    let count = read_u16(tiff, ifd, little).ok_or(Goal1bImageError::InvalidExif)? as usize;
    for i in 0..count {
        let at = exif_entry_offset(ifd, i)?;
        if read_u16(tiff, at, little) == Some(0x0112) {
            if read_u16(tiff, at + 2, little) != Some(3)
                || read_u32(tiff, at + 4, little) != Some(1)
            {
                return Err(Goal1bImageError::InvalidExif);
            }
            return read_u16(tiff, at + 8, little).ok_or(Goal1bImageError::InvalidExif);
        }
    }
    Ok(1)
}
fn normalize_exif_orientation(tiff: &mut [u8]) -> Result<(), Goal1bImageError> {
    let Some((little, ifd)) = tiff_header(tiff)? else {
        return Ok(());
    };
    let count = read_u16(tiff, ifd, little).ok_or(Goal1bImageError::InvalidExif)? as usize;
    for i in 0..count {
        let at = exif_entry_offset(ifd, i)?;
        if read_u16(tiff, at, little) == Some(0x0112) {
            let value = if little {
                1u16.to_le_bytes()
            } else {
                1u16.to_be_bytes()
            };
            tiff.get_mut(at + 8..at + 10)
                .ok_or(Goal1bImageError::InvalidExif)?
                .copy_from_slice(&value);
            return Ok(());
        }
    }
    Ok(())
}
fn tiff_header(tiff: &[u8]) -> Result<Option<(bool, usize)>, Goal1bImageError> {
    if tiff.is_empty() {
        return Ok(None);
    }
    if tiff.len() < 8 {
        return Err(Goal1bImageError::InvalidExif);
    }
    let little = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(Goal1bImageError::InvalidExif),
    };
    if read_u16(tiff, 2, little) != Some(42) {
        return Err(Goal1bImageError::InvalidExif);
    }
    let ifd = usize::try_from(read_u32(tiff, 4, little).ok_or(Goal1bImageError::InvalidExif)?)
        .map_err(|_| Goal1bImageError::InvalidExif)?;
    Ok(Some((little, ifd)))
}
fn read_u16(bytes: &[u8], at: usize, little: bool) -> Option<u16> {
    let b = bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(if little {
        u16::from_le_bytes(b)
    } else {
        u16::from_be_bytes(b)
    })
}
fn read_u32(bytes: &[u8], at: usize, little: bool) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    })
}

fn exif_entry_offset(ifd: usize, index: usize) -> Result<usize, Goal1bImageError> {
    ifd.checked_add(2)
        .and_then(|value| {
            index
                .checked_mul(12)
                .and_then(|entry| value.checked_add(entry))
        })
        .ok_or(Goal1bImageError::InvalidExif)
}

#[derive(Debug, Error)]
pub enum Goal1bImageError {
    #[error("input must be an absolute regular file")]
    InvalidInput,
    #[error("input exceeds the configured byte limit")]
    InputTooLarge,
    #[error("only PNG and JPEG inputs are supported")]
    UnsupportedFormat,
    #[error("only RGB8 and RGBA8 inputs are supported")]
    UnsupportedImageMode,
    #[error("image exceeds the configured pixel limit")]
    ImageTooLarge,
    #[error("image could not be decoded")]
    Decode,
    #[error("EXIF data is invalid")]
    InvalidExif,
    #[error("metadata is invalid")]
    InvalidMetadata,
    #[error("metadata exceeds the configured limits")]
    MetadataTooLarge,
    #[error("scale must be 2 or 4")]
    UnsupportedScale,
    #[error("alpha input cannot be encoded as JPEG")]
    AlphaJpegUnsupported,
    #[error("runner output must be an exact RGB8 x4 PNG")]
    InvalidRunnerOutput,
    #[error("alpha plane is invalid")]
    InvalidAlpha,
    #[error("output encoding failed")]
    Encode,
    #[error("partial output is invalid")]
    InvalidOutput,
    #[error("partial output metadata does not match policy")]
    MetadataMismatch,
    #[error("image pipeline I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, ImageBuffer, Luma, Rgb, Rgba};

    fn exif(orientation: u16, padding: usize) -> Vec<u8> {
        let mut value = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
        value.extend_from_slice(&orientation.to_be_bytes());
        value.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        value.resize(value.len() + padding, 0x42);
        value
    }

    fn base_rgb() -> RgbImage {
        ImageBuffer::from_fn(2, 3, |x, y| Rgb([(y * 2 + x + 1) as u8, 0, 0]))
    }

    fn write_png(path: &Path, image: &DynamicImage, metadata: &ImageMetadata) {
        encode_output(path, image, OutputEncoding::Png, metadata).expect("fixture must encode");
    }

    fn prepare_fixture(
        directory: &tempfile::TempDir,
        image: DynamicImage,
        metadata: ImageMetadata,
    ) -> PreparedImage {
        let input = directory.path().join("input.png");
        write_png(&input, &image, &metadata);
        prepare_image_input(
            &input,
            &directory.path().join("workspace"),
            ImagePipelineLimits::default(),
        )
        .expect("fixture must prepare")
    }

    fn runner_x4(prepared: &PreparedImage, path: &Path) {
        let rgb = image::open(&prepared.inference_png)
            .expect("inference must decode")
            .into_rgb8();
        let output = image::imageops::resize(
            &rgb,
            prepared.width * 4,
            prepared.height * 4,
            ResizeFilter::Nearest,
        );
        output
            .save_with_format(path, ImageFormat::Png)
            .expect("runner fixture must save");
    }

    #[test]
    fn applies_all_eight_exif_orientations_and_normalizes_tag() {
        let expected = [
            (2, 3, vec![1, 2, 3, 4, 5, 6]),
            (2, 3, vec![2, 1, 4, 3, 6, 5]),
            (2, 3, vec![6, 5, 4, 3, 2, 1]),
            (2, 3, vec![5, 6, 3, 4, 1, 2]),
            (3, 2, vec![1, 3, 5, 2, 4, 6]),
            (3, 2, vec![5, 3, 1, 6, 4, 2]),
            (3, 2, vec![6, 4, 2, 5, 3, 1]),
            (3, 2, vec![2, 4, 6, 1, 3, 5]),
        ];
        for (index, (width, height, red)) in expected.into_iter().enumerate() {
            let directory = tempfile::tempdir().expect("tempdir");
            let prepared = prepare_fixture(
                &directory,
                DynamicImage::ImageRgb8(base_rgb()),
                ImageMetadata {
                    icc: None,
                    exif: Some(exif((index + 1) as u16, 0)),
                },
            );
            assert_eq!((prepared.width, prepared.height), (width, height));
            let actual = image::open(&prepared.inference_png)
                .expect("inference must decode")
                .into_rgb8()
                .pixels()
                .map(|pixel| pixel[0])
                .collect::<Vec<_>>();
            assert_eq!(actual, red, "orientation {}", index + 1);
            assert_eq!(
                exif_orientation(prepared.metadata.exif.as_deref().expect("EXIF"))
                    .expect("valid EXIF"),
                1
            );
        }
    }

    #[test]
    fn prepares_rgb_inference_and_alpha_plane_without_rejecting_transparency() {
        let directory = tempfile::tempdir().expect("tempdir");
        let rgba = ImageBuffer::from_fn(3, 2, |x, y| {
            Rgba([10 + x as u8, 20, 30, (x * 80 + y * 15) as u8])
        });
        let prepared = prepare_fixture(
            &directory,
            DynamicImage::ImageRgba8(rgba),
            ImageMetadata::default(),
        );
        assert!(prepared.had_alpha);
        let alpha = image::open(prepared.alpha_png.expect("alpha path"))
            .expect("alpha must decode")
            .into_luma8();
        assert_eq!(
            alpha.pixels().map(|p| p[0]).collect::<Vec<_>>(),
            vec![0, 80, 160, 15, 95, 175]
        );

        let transparent = ImageBuffer::from_pixel(2, 2, Rgba([0, 0, 0, 0]));
        let second = tempfile::tempdir().expect("tempdir");
        prepare_fixture(
            &second,
            DynamicImage::ImageRgba8(transparent),
            ImageMetadata::default(),
        );
    }

    #[test]
    fn prepares_rgb8_jpeg_with_icc_and_exif_orientation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let input = directory.path().join("input.jpg");
        let rgb = base_rgb();
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                ColorType::Rgb8.into(),
            )
            .expect("JPEG fixture must encode");
        inject_jpeg_metadata(
            &mut jpeg,
            &ImageMetadata {
                icc: Some(vec![3; 80_000]),
                exif: Some(exif(6, 0)),
            },
        )
        .expect("metadata must inject");
        fs::write(&input, jpeg).expect("fixture must write");
        let prepared = prepare_image_input(
            &input,
            &directory.path().join("workspace"),
            ImagePipelineLimits::default(),
        )
        .expect("JPEG must prepare");
        assert_eq!((prepared.width, prepared.height), (3, 2));
        assert_eq!(prepared.metadata.icc.as_ref().map(Vec::len), Some(80_000));
        assert_eq!(
            exif_orientation(prepared.metadata.exif.as_deref().expect("EXIF")).expect("valid EXIF"),
            1
        );
        assert!(matches!(
            image::open(prepared.inference_png).expect("inference must decode"),
            DynamicImage::ImageRgb8(_)
        ));
    }

    #[test]
    fn rejects_exif_orientation_outside_one_through_eight() {
        let directory = tempfile::tempdir().expect("tempdir");
        let input = directory.path().join("input.png");
        write_png(
            &input,
            &DynamicImage::ImageRgb8(base_rgb()),
            &ImageMetadata {
                icc: None,
                exif: Some(exif(9, 0)),
            },
        );
        assert!(matches!(
            prepare_image_input(
                &input,
                &directory.path().join("workspace"),
                ImagePipelineLimits::default()
            ),
            Err(Goal1bImageError::InvalidExif)
        ));
    }

    #[test]
    fn renders_png_jpeg_and_lossless_webp_with_metadata_policy() {
        let metadata = ImageMetadata {
            icc: Some(b"test-icc-profile".to_vec()),
            exif: Some(exif(6, 4)),
        };

        for (format, alpha) in [
            (OutputEncoding::Png, false),
            (OutputEncoding::Jpeg, false),
            (OutputEncoding::Webp, true),
        ] {
            for policy in [MetadataPolicy::Preserve, MetadataPolicy::Strip] {
                let directory = tempfile::tempdir().expect("tempdir");
                let image = if alpha {
                    DynamicImage::ImageRgba8(ImageBuffer::from_fn(2, 3, |x, y| {
                        Rgba([20, 30, 40, (x * 100 + y * 20) as u8])
                    }))
                } else {
                    DynamicImage::ImageRgb8(base_rgb())
                };
                let prepared = prepare_fixture(&directory, image, metadata.clone());
                let runner = directory.path().join("runner.png");
                runner_x4(&prepared, &runner);
                let extension = match format {
                    OutputEncoding::Png => "png",
                    OutputEncoding::Jpeg => "jpg",
                    OutputEncoding::Webp => "webp",
                };
                let output = directory.path().join(format!("partial.{extension}"));
                let verified = render_pipeline_output(
                    &prepared,
                    &runner,
                    &output,
                    2,
                    format,
                    policy,
                    ImagePipelineLimits::default(),
                )
                .expect("output must render and verify");
                assert_eq!(
                    (verified.width, verified.height),
                    (prepared.width * 2, prepared.height * 2)
                );
                assert_eq!(verified.has_alpha, alpha);
                assert_eq!(verified.icc_preserved, policy == MetadataPolicy::Preserve);
                assert_eq!(verified.exif_preserved, policy == MetadataPolicy::Preserve);
                if format == OutputEncoding::Webp {
                    let first = image::open(&output).expect("WebP must decode").to_rgba8();
                    let second_path = directory.path().join("second.webp");
                    encode_output(
                        &second_path,
                        &DynamicImage::ImageRgba8(first.clone()),
                        OutputEncoding::Webp,
                        &ImageMetadata::default(),
                    )
                    .expect("second WebP must encode");
                    assert_eq!(image::open(second_path).expect("decode").to_rgba8(), first);
                }
            }
        }
    }

    #[test]
    fn x2_uses_x4_rgb_and_resizes_alpha_with_lanczos3() {
        let directory = tempfile::tempdir().expect("tempdir");
        let prepared = prepare_fixture(
            &directory,
            DynamicImage::ImageRgba8(ImageBuffer::from_fn(3, 1, |x, _| {
                Rgba([4, 5, 6, (x * 127) as u8])
            })),
            ImageMetadata::default(),
        );
        let runner = directory.path().join("runner.png");
        RgbImage::from_pixel(12, 4, Rgb([9, 8, 7]))
            .save_with_format(&runner, ImageFormat::Png)
            .expect("runner must save");
        let output = directory.path().join("partial.png");
        render_pipeline_output(
            &prepared,
            &runner,
            &output,
            2,
            OutputEncoding::Png,
            MetadataPolicy::Strip,
            ImagePipelineLimits::default(),
        )
        .expect("output must render");
        let rgba = image::open(output)
            .expect("output must decode")
            .into_rgba8();
        assert_eq!(rgba.dimensions(), (6, 2));
        assert!(rgba.pixels().all(|p| p.0[..3] == [9, 8, 7]));
        assert!(rgba.pixels().any(|p| p[3] > 0 && p[3] < 254));
    }

    #[test]
    fn rejects_alpha_jpeg_before_writing_and_invalid_runner_output() {
        let directory = tempfile::tempdir().expect("tempdir");
        let prepared = prepare_fixture(
            &directory,
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 2, Rgba([1, 2, 3, 4]))),
            ImageMetadata::default(),
        );
        let runner = directory.path().join("runner.png");
        runner_x4(&prepared, &runner);
        let output = directory.path().join("partial.jpg");
        assert!(matches!(
            render_pipeline_output(
                &prepared,
                &runner,
                &output,
                4,
                OutputEncoding::Jpeg,
                MetadataPolicy::Strip,
                ImagePipelineLimits::default()
            ),
            Err(Goal1bImageError::AlphaJpegUnsupported)
        ));
        assert!(!output.exists());
        RgbaImage::new(8, 8)
            .save_with_format(&runner, ImageFormat::Png)
            .expect("bad runner must save");
        assert!(matches!(
            render_pipeline_output(
                &prepared,
                &runner,
                &directory.path().join("partial.png"),
                4,
                OutputEncoding::Png,
                MetadataPolicy::Strip,
                ImagePipelineLimits::default()
            ),
            Err(Goal1bImageError::InvalidRunnerOutput)
        ));
    }

    #[test]
    fn rejects_grayscale_16bit_corrupt_and_size_attacks() {
        let directory = tempfile::tempdir().expect("tempdir");
        let gray = directory.path().join("gray.png");
        GrayImage::from_pixel(2, 2, Luma([2]))
            .save_with_format(&gray, ImageFormat::Png)
            .expect("gray fixture");
        let sixteen = directory.path().join("sixteen.png");
        let pixels = [1u16, 2, 3, 4]
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(&pixels, 2, 2, ColorType::L16.into())
            .expect("16-bit fixture");
        fs::write(&sixteen, encoded).expect("write fixture");
        let corrupt = directory.path().join("corrupt.png");
        fs::write(&corrupt, b"not an image").expect("write fixture");
        for path in [&gray, &sixteen] {
            assert!(matches!(
                prepare_image_input(
                    path,
                    &directory.path().join("w"),
                    ImagePipelineLimits::default()
                ),
                Err(Goal1bImageError::UnsupportedImageMode)
            ));
        }
        assert!(matches!(
            prepare_image_input(
                &corrupt,
                &directory.path().join("w"),
                ImagePipelineLimits::default()
            ),
            Err(Goal1bImageError::UnsupportedFormat | Goal1bImageError::Decode)
        ));

        let rgb = directory.path().join("rgb.png");
        write_png(
            &rgb,
            &DynamicImage::ImageRgb8(base_rgb()),
            &ImageMetadata::default(),
        );
        let tiny_bytes = ImagePipelineLimits {
            max_input_bytes: 1,
            ..ImagePipelineLimits::default()
        };
        assert!(matches!(
            prepare_image_input(&rgb, &directory.path().join("w"), tiny_bytes),
            Err(Goal1bImageError::InputTooLarge)
        ));
        let tiny_pixels = ImagePipelineLimits {
            max_pixels: 1,
            ..ImagePipelineLimits::default()
        };
        assert!(matches!(
            prepare_image_input(&rgb, &directory.path().join("w"), tiny_pixels),
            Err(Goal1bImageError::ImageTooLarge)
        ));
    }

    #[test]
    fn rejects_individual_total_and_compressed_metadata_attacks() {
        let directory = tempfile::tempdir().expect("tempdir");
        let input = directory.path().join("metadata.png");
        let metadata = ImageMetadata {
            icc: Some(vec![7; 40]),
            exif: Some(exif(1, 40)),
        };
        write_png(&input, &DynamicImage::ImageRgb8(base_rgb()), &metadata);
        let icc_limit = ImagePipelineLimits {
            max_icc_bytes: 8,
            ..ImagePipelineLimits::default()
        };
        assert!(matches!(
            prepare_image_input(&input, &directory.path().join("w1"), icc_limit),
            Err(Goal1bImageError::MetadataTooLarge)
        ));
        let exif_limit = ImagePipelineLimits {
            max_exif_bytes: 16,
            ..ImagePipelineLimits::default()
        };
        assert!(matches!(
            prepare_image_input(&input, &directory.path().join("w2"), exif_limit),
            Err(Goal1bImageError::MetadataTooLarge)
        ));
        let total_limit = ImagePipelineLimits {
            max_metadata_bytes: 50,
            ..ImagePipelineLimits::default()
        };
        assert!(matches!(
            prepare_image_input(&input, &directory.path().join("w3"), total_limit),
            Err(Goal1bImageError::MetadataTooLarge)
        ));

        let bomb = ImageMetadata {
            icc: Some(vec![0; 64 * 1024]),
            exif: None,
        };
        write_png(&input, &DynamicImage::ImageRgb8(base_rgb()), &bomb);
        let bounded = ImagePipelineLimits {
            max_icc_bytes: 1024,
            ..ImagePipelineLimits::default()
        };
        assert!(matches!(
            prepare_image_input(&input, &directory.path().join("w4"), bounded),
            Err(Goal1bImageError::MetadataTooLarge)
        ));
    }

    #[test]
    fn verification_rejects_wrong_format_dimensions_alpha_and_metadata() {
        let directory = tempfile::tempdir().expect("tempdir");
        let prepared = prepare_fixture(
            &directory,
            DynamicImage::ImageRgb8(base_rgb()),
            ImageMetadata {
                icc: Some(vec![1, 2, 3]),
                exif: Some(exif(1, 0)),
            },
        );
        let output = directory.path().join("partial.png");
        RgbImage::new(4, 6)
            .save_with_format(&output, ImageFormat::Png)
            .expect("fixture");
        assert!(matches!(
            verify_pipeline_output(
                &output,
                &prepared,
                2,
                OutputEncoding::Jpeg,
                MetadataPolicy::Strip,
                ImagePipelineLimits::default()
            ),
            Err(Goal1bImageError::InvalidOutput)
        ));
        assert!(matches!(
            verify_pipeline_output(
                &output,
                &prepared,
                4,
                OutputEncoding::Png,
                MetadataPolicy::Strip,
                ImagePipelineLimits::default()
            ),
            Err(Goal1bImageError::InvalidOutput)
        ));
        assert!(matches!(
            verify_pipeline_output(
                &output,
                &prepared,
                2,
                OutputEncoding::Png,
                MetadataPolicy::Preserve,
                ImagePipelineLimits::default()
            ),
            Err(Goal1bImageError::MetadataMismatch)
        ));
        write_png(
            &output,
            &DynamicImage::ImageRgba8(RgbaImage::new(4, 6)),
            &ImageMetadata::default(),
        );
        assert!(matches!(
            verify_pipeline_output(
                &output,
                &prepared,
                2,
                OutputEncoding::Png,
                MetadataPolicy::Strip,
                ImagePipelineLimits::default()
            ),
            Err(Goal1bImageError::InvalidOutput)
        ));
    }
}
