use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use atomic_write_file::AtomicWriteFile;
use image::{ColorType, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zoos_runner_protocol::ImageInputFormat;

const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_OUTPUT_SIDE: u64 = 32_000;
const MAX_OUTPUT_PIXELS: u64 = 100_000_000;
const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedImageInput {
    pub path: PathBuf,
    pub sha256: String,
    pub format: ImageInputFormat,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageOutputPlan {
    pub job_id: String,
    pub input: ValidatedImageInput,
    pub scale: u8,
    pub output_width: u32,
    pub output_height: u32,
    pub final_path: PathBuf,
    pub partial_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageVerification {
    pub schema_version: u32,
    pub job_id: String,
    pub input_path: PathBuf,
    pub input_sha256_before: String,
    pub input_sha256_after: String,
    pub output_path: PathBuf,
    pub output_sha256: String,
    pub output_format: String,
    pub output_width: u32,
    pub output_height: u32,
}

pub fn plan_image_output(
    input_path: impl AsRef<Path>,
    scale: u8,
    job_id: impl Into<String>,
) -> Result<ImageOutputPlan, ImageSafetyError> {
    if !matches!(scale, 2 | 4) {
        return Err(ImageSafetyError::UnsupportedScale(scale));
    }
    let input = validate_image_input(input_path.as_ref())?;
    let (output_width, output_height) =
        checked_output_dimensions(input.width, input.height, scale)?;
    let pixels = u64::from(output_width) * u64::from(output_height);

    let parent = input
        .path
        .parent()
        .ok_or(ImageSafetyError::InvalidInputPath)?;
    let output_dir = parent.join("Upscaled");
    fs::create_dir_all(&output_dir)?;
    let required = MIN_FREE_BYTES.max(pixels.saturating_mul(3).saturating_mul(2));
    let available = fs4::available_space(&output_dir)?;
    if available < required {
        return Err(ImageSafetyError::InsufficientDisk {
            required,
            available,
        });
    }

    let stem = input
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .ok_or(ImageSafetyError::InvalidInputPath)?;
    let base = format!("{stem}_upscaled_{scale}x");
    let final_path = first_available_output(&output_dir, &base)?;
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ImageSafetyError::InvalidInputPath)?;
    let job_id = job_id.into();
    let partial_path = output_dir.join(format!(".{file_name}.zoos-{job_id}.partial.png"));
    if partial_path.exists() {
        return Err(ImageSafetyError::OutputExists(partial_path));
    }

    Ok(ImageOutputPlan {
        job_id,
        input,
        scale,
        output_width,
        output_height,
        final_path,
        partial_path,
    })
}

pub fn validate_image_input(path: &Path) -> Result<ValidatedImageInput, ImageSafetyError> {
    if !path.is_absolute() {
        return Err(ImageSafetyError::InvalidInputPath);
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ImageSafetyError::InvalidInputPath);
    }
    if metadata.len() > MAX_INPUT_BYTES {
        return Err(ImageSafetyError::InputTooLarge(metadata.len()));
    }

    let reader = ImageReader::open(path)?.with_guessed_format()?;
    let actual_format = reader.format().ok_or(ImageSafetyError::UnsupportedFormat)?;
    let format = match actual_format {
        ImageFormat::Png => ImageInputFormat::Png,
        ImageFormat::Jpeg => ImageInputFormat::Jpeg,
        _ => return Err(ImageSafetyError::UnsupportedFormat),
    };
    let decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || decoder.color_type() != ColorType::Rgb8 {
        return Err(ImageSafetyError::UnsupportedImageMode);
    }
    // Every accepted input must at least fit the x2 output policy. Full RGB decode is then
    // bounded to 75 MB, before the selected scale applies its tighter planning check.
    checked_output_dimensions(width, height, 2)?;
    drop(decoder);

    if format == ImageInputFormat::Jpeg {
        validate_jpeg_structure_and_orientation(path)?;
    }
    match image::open(path)? {
        DynamicImage::ImageRgb8(image) if image.width() == width && image.height() == height => {}
        _ => return Err(ImageSafetyError::UnsupportedImageMode),
    }

    Ok(ValidatedImageInput {
        path: path.to_owned(),
        sha256: sha256_file(path)?,
        format,
        width,
        height,
    })
}

pub fn recheck_input(input: &ValidatedImageInput) -> Result<String, ImageSafetyError> {
    let current = sha256_file(&input.path).map_err(|_| ImageSafetyError::InputChanged)?;
    if current != input.sha256 {
        return Err(ImageSafetyError::InputChanged);
    }
    Ok(current)
}

pub fn verify_partial_output(plan: &ImageOutputPlan) -> Result<String, ImageSafetyError> {
    let reader = ImageReader::open(&plan.partial_path)
        .map_err(|_| ImageSafetyError::InvalidOutput("output cannot be opened"))?
        .with_guessed_format()
        .map_err(|_| ImageSafetyError::InvalidOutput("output format cannot be detected"))?;
    if reader.format() != Some(ImageFormat::Png) {
        return Err(ImageSafetyError::InvalidOutput("output is not PNG"));
    }
    let decoder = reader
        .into_decoder()
        .map_err(|_| ImageSafetyError::InvalidOutput("output cannot be decoded"))?;
    if decoder.color_type() != ColorType::Rgb8
        || decoder.dimensions() != (plan.output_width, plan.output_height)
    {
        return Err(ImageSafetyError::InvalidOutput(
            "output is not RGB8 at the planned dimensions",
        ));
    }
    drop(decoder);
    let image = match image::open(&plan.partial_path)
        .map_err(|_| ImageSafetyError::InvalidOutput("output cannot be decoded"))?
    {
        DynamicImage::ImageRgb8(image) => image,
        _ => return Err(ImageSafetyError::InvalidOutput("output is not RGB8")),
    };
    if !image.as_raw().iter().any(|channel| *channel != 0) {
        return Err(ImageSafetyError::InvalidOutput(
            "output pixels are all zero",
        ));
    }
    sha256_file(&plan.partial_path)
}

pub fn publish_verified_output(
    plan: &ImageOutputPlan,
    verification_path: &Path,
) -> Result<ImageVerification, ImageSafetyError> {
    let output_hash = verify_partial_output(plan)?;
    // Keep this check immediately adjacent to publication; decoding a large output can take time.
    let input_after = recheck_input(&plan.input)?;
    let verification = ImageVerification {
        schema_version: 1,
        job_id: plan.job_id.clone(),
        input_path: plan.input.path.clone(),
        input_sha256_before: plan.input.sha256.clone(),
        input_sha256_after: input_after,
        output_path: plan.final_path.clone(),
        output_sha256: output_hash.clone(),
        output_format: "png".into(),
        output_width: plan.output_width,
        output_height: plan.output_height,
    };
    // Persist the exact hash before the atomic rename. On crash recovery, the presence of the
    // partial distinguishes "intent recorded" from "rename completed", allowing safe rollback.
    write_json_atomic(verification_path, &verification)?;
    if let Err(error) = no_replace_rename(&plan.partial_path, &plan.final_path) {
        let _ = remove_file_if_present(verification_path);
        return Err(error);
    }
    if let Err(error) = sync_parent_directory(&plan.final_path) {
        if sha256_file(&plan.final_path).ok().as_deref() == Some(output_hash.as_str()) {
            let _ = fs::remove_file(&plan.final_path);
        }
        let _ = remove_file_if_present(verification_path);
        return Err(error);
    }
    Ok(verification)
}

pub fn cleanup_owned_output(
    plan: &ImageOutputPlan,
    verification_path: &Path,
) -> Result<(), ImageSafetyError> {
    let partial_was_present = match fs::symlink_metadata(&plan.partial_path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    remove_file_if_present(&plan.partial_path)?;
    let verification = match File::open(verification_path) {
        Ok(file) => serde_json::from_reader::<_, ImageVerification>(BufReader::new(file)).ok(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if !partial_was_present
        && let Some(verification) = verification
        && verification.job_id == plan.job_id
        && verification.output_path == plan.final_path
        && sha256_file(&plan.final_path).ok().as_deref()
            == Some(verification.output_sha256.as_str())
    {
        remove_file_if_present(&plan.final_path)?;
    }
    remove_file_if_present(verification_path)?;
    Ok(())
}

fn first_available_output(directory: &Path, base: &str) -> Result<PathBuf, ImageSafetyError> {
    let initial = directory.join(format!("{base}.png"));
    if !initial.exists() {
        return Ok(initial);
    }
    for suffix in 2..=999 {
        let candidate = directory.join(format!("{base}_{suffix}.png"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(ImageSafetyError::NoOutputNameAvailable)
}

fn checked_output_dimension(value: u32, scale: u8) -> Result<u32, ImageSafetyError> {
    let output = u64::from(value) * u64::from(scale);
    if output > MAX_OUTPUT_SIDE {
        return Err(ImageSafetyError::OutputTooLarge {
            width: u32::try_from(output).unwrap_or(u32::MAX),
            height: 0,
        });
    }
    Ok(output as u32)
}

fn checked_output_dimensions(
    width: u32,
    height: u32,
    scale: u8,
) -> Result<(u32, u32), ImageSafetyError> {
    let output_width = checked_output_dimension(width, scale)?;
    let output_height = checked_output_dimension(height, scale)?;
    if u64::from(output_width) * u64::from(output_height) > MAX_OUTPUT_PIXELS {
        return Err(ImageSafetyError::OutputTooLarge {
            width: output_width,
            height: output_height,
        });
    }
    Ok((output_width, output_height))
}

pub(crate) fn sha256_file(path: &Path) -> Result<String, ImageSafetyError> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_jpeg_structure_and_orientation(path: &Path) -> Result<(), ImageSafetyError> {
    let bytes = fs::read(path)?;
    if bytes.len() < 4 || &bytes[..2] != b"\xff\xd8" {
        return Err(ImageSafetyError::UnsupportedFormat);
    }
    let mut offset = 2;
    let mut rgb_components = None;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xff {
            return Err(ImageSafetyError::CorruptImage);
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        if offset >= bytes.len() {
            break;
        }
        let marker = bytes[offset];
        offset += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            continue;
        }
        if offset + 2 > bytes.len() {
            return Err(ImageSafetyError::CorruptImage);
        }
        let length = usize::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]]));
        if length < 2 || offset + length > bytes.len() {
            return Err(ImageSafetyError::CorruptImage);
        }
        let segment = &bytes[offset + 2..offset + length];
        if marker == 0xe1
            && segment.starts_with(b"Exif\0\0")
            && let Some(orientation) = parse_exif_orientation(&segment[6..])?
            && orientation != 1
        {
            return Err(ImageSafetyError::UnsupportedOrientation(orientation));
        }
        if is_start_of_frame(marker) {
            if segment.len() < 6 || segment[0] != 8 {
                return Err(ImageSafetyError::UnsupportedImageMode);
            }
            rgb_components = Some(segment[5]);
        }
        offset += length;
    }
    if rgb_components != Some(3) {
        return Err(ImageSafetyError::UnsupportedImageMode);
    }
    Ok(())
}

fn is_start_of_frame(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn parse_exif_orientation(tiff: &[u8]) -> Result<Option<u16>, ImageSafetyError> {
    if tiff.len() < 8 {
        return Err(ImageSafetyError::CorruptImage);
    }
    let little = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(ImageSafetyError::CorruptImage),
    };
    let read_u16 = |at: usize| -> Option<u16> {
        let bytes: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let read_u32 = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    if read_u16(2) != Some(42) {
        return Err(ImageSafetyError::CorruptImage);
    }
    let ifd = usize::try_from(read_u32(4).ok_or(ImageSafetyError::CorruptImage)?)
        .map_err(|_| ImageSafetyError::CorruptImage)?;
    let count = usize::from(read_u16(ifd).ok_or(ImageSafetyError::CorruptImage)?);
    for index in 0..count {
        let entry = ifd + 2 + index * 12;
        if entry + 12 > tiff.len() {
            return Err(ImageSafetyError::CorruptImage);
        }
        if read_u16(entry) == Some(0x0112) {
            if read_u16(entry + 2) != Some(3) || read_u32(entry + 4) != Some(1) {
                return Err(ImageSafetyError::CorruptImage);
            }
            return read_u16(entry + 8)
                .map(Some)
                .ok_or(ImageSafetyError::CorruptImage);
        }
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
pub(crate) fn no_replace_rename(source: &Path, destination: &Path) -> Result<(), ImageSafetyError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const RENAME_EXCL: libc::c_uint = 0x00000004;
    unsafe extern "C" {
        fn renamex_np(
            from: *const libc::c_char,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    let destination_path = destination.to_owned();
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| ImageSafetyError::InvalidInputPath)?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| ImageSafetyError::InvalidInputPath)?;
    // SAFETY: both C strings are live and NUL-terminated for the duration of the call.
    if unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::AlreadyExists {
        Err(ImageSafetyError::OutputExists(destination_path))
    } else {
        Err(error.into())
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn no_replace_rename(source: &Path, destination: &Path) -> Result<(), ImageSafetyError> {
    match fs::hard_link(source, destination) {
        Ok(()) => {
            if let Err(error) = fs::remove_file(source) {
                let _ = fs::remove_file(destination);
                return Err(error.into());
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(ImageSafetyError::OutputExists(destination.to_owned()))
        }
        Err(error) => Err(error.into()),
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), ImageSafetyError> {
    let mut file = AtomicWriteFile::open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.commit()?;
    Ok(())
}

fn remove_file_if_present(path: &Path) -> Result<(), ImageSafetyError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn sync_parent_directory(path: &Path) -> Result<(), ImageSafetyError> {
    File::open(path.parent().ok_or(ImageSafetyError::InvalidInputPath)?)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ImageSafetyError {
    #[error("input path must be an absolute regular file")]
    InvalidInputPath,
    #[error("input is larger than 512 MiB ({0} bytes)")]
    InputTooLarge(u64),
    #[error("only PNG and JPEG inputs are supported")]
    UnsupportedFormat,
    #[error("image must be a non-empty RGB8 image")]
    UnsupportedImageMode,
    #[error("JPEG EXIF orientation {0} is not supported")]
    UnsupportedOrientation(u16),
    #[error("image is corrupt")]
    CorruptImage,
    #[error("scale must be 2 or 4, got {0}")]
    UnsupportedScale(u8),
    #[error("planned output is too large ({width}x{height})")]
    OutputTooLarge { width: u32, height: u32 },
    #[error("insufficient disk space: need {required} bytes, have {available} bytes")]
    InsufficientDisk { required: u64, available: u64 },
    #[error("all output names through suffix _999 already exist")]
    NoOutputNameAvailable,
    #[error("output already exists: {0}")]
    OutputExists(PathBuf),
    #[error("input changed after planning")]
    InputChanged,
    #[error("invalid generated output: {0}")]
    InvalidOutput(&'static str),
    #[error("image decoding failed: {0}")]
    Image(#[from] image::ImageError),
    #[error("image I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("verification JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl ImageSafetyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutputTooLarge { .. } => "OUTPUT_TOO_LARGE",
            Self::InsufficientDisk { .. } => "INSUFFICIENT_DISK",
            Self::OutputExists(_) | Self::NoOutputNameAvailable => "OUTPUT_EXISTS",
            Self::InputChanged => "INPUT_CHANGED",
            Self::InvalidOutput(_) | Self::Io(_) | Self::Json(_) => "UPSTREAM_FAILED",
            Self::InvalidInputPath
            | Self::InputTooLarge(_)
            | Self::UnsupportedFormat
            | Self::UnsupportedImageMode
            | Self::UnsupportedOrientation(_)
            | Self::CorruptImage
            | Self::UnsupportedScale(_)
            | Self::Image(_) => "UNSUPPORTED_IMAGE_MODE",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, ImageBuffer, Luma, Rgb, RgbImage, Rgba, RgbaImage};

    fn rgb_png(path: &Path, width: u32, height: u32, pixel: [u8; 3]) {
        RgbImage::from_pixel(width, height, Rgb(pixel))
            .save_with_format(path, ImageFormat::Png)
            .expect("RGB PNG fixture must save");
    }

    fn rgb_jpeg(path: &Path) {
        RgbImage::from_pixel(3, 2, Rgb([10, 20, 30]))
            .save_with_format(path, ImageFormat::Jpeg)
            .expect("RGB JPEG fixture must save");
    }

    fn insert_exif_orientation(path: &Path, orientation: u16) {
        let jpeg = fs::read(path).expect("JPEG fixture must read");
        let mut payload = b"Exif\0\0MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
        payload.extend_from_slice(&orientation.to_be_bytes());
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let length = u16::try_from(payload.len() + 2).expect("EXIF fixture must fit");
        let mut with_exif = jpeg[..2].to_vec();
        with_exif.extend_from_slice(&[0xff, 0xe1]);
        with_exif.extend_from_slice(&length.to_be_bytes());
        with_exif.extend_from_slice(&payload);
        with_exif.extend_from_slice(&jpeg[2..]);
        fs::write(path, with_exif).expect("EXIF JPEG fixture must save");
    }

    #[test]
    fn accepts_rgb8_png_and_jpeg_with_missing_or_normal_orientation() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let png = directory.path().join("photo.png");
        let jpeg = directory.path().join("photo.jpg");
        let normal_jpeg = directory.path().join("normal.jpg");
        rgb_png(&png, 2, 3, [1, 2, 3]);
        rgb_jpeg(&jpeg);
        fs::copy(&jpeg, &normal_jpeg).expect("JPEG fixture must copy");
        insert_exif_orientation(&normal_jpeg, 1);

        assert_eq!(
            validate_image_input(&png)
                .expect("PNG must validate")
                .format,
            ImageInputFormat::Png
        );
        assert_eq!(
            validate_image_input(&jpeg)
                .expect("JPEG without EXIF must validate")
                .format,
            ImageInputFormat::Jpeg
        );
        validate_image_input(&normal_jpeg).expect("orientation 1 must validate");
    }

    #[test]
    fn rejects_relative_alpha_grayscale_sixteen_bit_corrupt_and_rotated_inputs() {
        assert!(matches!(
            validate_image_input(Path::new("relative.png")),
            Err(ImageSafetyError::InvalidInputPath)
        ));
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let alpha = directory.path().join("alpha.png");
        let gray = directory.path().join("gray.png");
        let sixteen = directory.path().join("sixteen.png");
        let corrupt = directory.path().join("corrupt.png");
        let rotated = directory.path().join("rotated.jpg");
        RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 4]))
            .save(&alpha)
            .expect("alpha fixture must save");
        GrayImage::from_pixel(2, 2, Luma([1]))
            .save(&gray)
            .expect("gray fixture must save");
        let rgb16: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::from_pixel(2, 2, Rgb([1, 2, 3]));
        DynamicImage::ImageRgb16(rgb16)
            .save(&sixteen)
            .expect("16-bit fixture must save");
        fs::write(&corrupt, b"\x89PNG\r\n\x1a\ntruncated").expect("corrupt fixture must save");
        rgb_jpeg(&rotated);
        insert_exif_orientation(&rotated, 6);

        for path in [&alpha, &gray, &sixteen] {
            assert!(matches!(
                validate_image_input(path),
                Err(ImageSafetyError::UnsupportedImageMode)
            ));
        }
        assert!(validate_image_input(&corrupt).is_err());
        assert!(matches!(
            validate_image_input(&rotated),
            Err(ImageSafetyError::UnsupportedOrientation(6))
        ));
    }

    #[test]
    fn rejects_input_larger_than_512_mib_before_decoding() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let oversized = directory.path().join("oversized.png");
        let file = File::create(&oversized).expect("sparse fixture must create");
        file.set_len(MAX_INPUT_BYTES + 1)
            .expect("sparse fixture must resize");
        assert!(matches!(
            validate_image_input(&oversized),
            Err(ImageSafetyError::InputTooLarge(size)) if size == MAX_INPUT_BYTES + 1
        ));
    }

    #[test]
    fn output_dimension_limits_cover_side_and_total_pixels() {
        assert!(matches!(
            checked_output_dimensions(8_001, 1, 4),
            Err(ImageSafetyError::OutputTooLarge { .. })
        ));
        assert!(matches!(
            checked_output_dimensions(10_000, 2_501, 2),
            Err(ImageSafetyError::OutputTooLarge { .. })
        ));
        assert_eq!(
            checked_output_dimensions(5_000, 5_000, 2).expect("100M pixels is allowed"),
            (10_000, 10_000)
        );
        assert_eq!(MIN_FREE_BYTES, 1_073_741_824);
    }

    #[test]
    fn public_error_codes_are_stable() {
        assert_eq!(ImageSafetyError::InputChanged.code(), "INPUT_CHANGED");
        assert_eq!(
            ImageSafetyError::OutputExists(PathBuf::from("output.png")).code(),
            "OUTPUT_EXISTS"
        );
        assert_eq!(
            ImageSafetyError::InvalidOutput("bad output").code(),
            "UPSTREAM_FAILED"
        );
        assert_eq!(
            ImageSafetyError::OutputTooLarge {
                width: 32_001,
                height: 1
            }
            .code(),
            "OUTPUT_TOO_LARGE"
        );
        assert_eq!(
            ImageSafetyError::UnsupportedImageMode.code(),
            "UNSUPPORTED_IMAGE_MODE"
        );
    }

    #[test]
    fn plan_uses_unicode_names_and_first_free_suffix() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let input = directory.path().join("고양이 사진.png");
        rgb_png(&input, 2, 3, [1, 2, 3]);
        let output_dir = directory.path().join("Upscaled");
        fs::create_dir(&output_dir).expect("output directory must exist");
        let existing = output_dir.join("고양이 사진_upscaled_2x.png");
        fs::write(&existing, b"existing").expect("existing output must save");

        let plan = plan_image_output(&input, 2, "job id").expect("plan must succeed");
        assert_eq!(
            plan.final_path.file_name().and_then(|name| name.to_str()),
            Some("고양이 사진_upscaled_2x_2.png")
        );
        assert_eq!(
            plan.partial_path.file_name().and_then(|name| name.to_str()),
            Some(".고양이 사진_upscaled_2x_2.png.zoos-job id.partial.png")
        );
        assert_eq!(
            fs::read(existing).expect("existing output must remain"),
            b"existing"
        );
    }

    #[test]
    fn input_hash_recheck_detects_changes() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let input = directory.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let validated = validate_image_input(&input).expect("input must validate");
        rgb_png(&input, 2, 2, [4, 5, 6]);
        assert!(matches!(
            recheck_input(&validated),
            Err(ImageSafetyError::InputChanged)
        ));
    }

    #[test]
    fn verifies_and_publishes_rgb8_png_with_verification_record() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let input = directory.path().join("input.png");
        rgb_png(&input, 2, 3, [1, 2, 3]);
        let plan = plan_image_output(&input, 2, "owned-job").expect("plan must succeed");
        rgb_png(&plan.partial_path, 4, 6, [9, 8, 7]);
        let verification_path = directory.path().join("verification.json");

        let verification = publish_verified_output(&plan, &verification_path)
            .expect("verified output must publish");
        assert!(plan.final_path.is_file());
        assert!(!plan.partial_path.exists());
        assert_eq!(verification.output_width, 4);
        assert_eq!(verification.output_height, 6);
        assert_eq!(
            verification.input_sha256_before,
            verification.input_sha256_after
        );
        let stored: ImageVerification =
            serde_json::from_slice(&fs::read(verification_path).expect("verification must read"))
                .expect("verification must deserialize");
        assert_eq!(stored, verification);
    }

    #[test]
    fn rejects_wrong_dimensions_mode_and_black_output() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let input = directory.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let plan = plan_image_output(&input, 2, "job").expect("plan must succeed");
        rgb_png(&plan.partial_path, 3, 4, [1, 1, 1]);
        assert!(verify_partial_output(&plan).is_err());
        GrayImage::from_pixel(4, 4, Luma([1]))
            .save(&plan.partial_path)
            .expect("gray output must save");
        assert!(verify_partial_output(&plan).is_err());
        rgb_png(&plan.partial_path, 4, 4, [0, 0, 0]);
        assert!(matches!(
            verify_partial_output(&plan),
            Err(ImageSafetyError::InvalidOutput(
                "output pixels are all zero"
            ))
        ));
    }

    #[test]
    fn publish_race_never_overwrites_existing_output_or_input() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let input = directory.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let input_before = fs::read(&input).expect("input must read");
        let plan = plan_image_output(&input, 2, "job").expect("plan must succeed");
        rgb_png(&plan.partial_path, 4, 4, [4, 5, 6]);
        fs::write(&plan.final_path, b"raced output").expect("race output must save");

        assert!(matches!(
            publish_verified_output(&plan, &directory.path().join("verification.json")),
            Err(ImageSafetyError::OutputExists(_))
        ));
        assert_eq!(
            fs::read(&plan.final_path).expect("race output must read"),
            b"raced output"
        );
        assert_eq!(fs::read(&input).expect("input must read"), input_before);

        let mut malicious = plan.clone();
        malicious.final_path.clone_from(&input);
        assert!(matches!(
            no_replace_rename(&malicious.partial_path, &malicious.final_path),
            Err(ImageSafetyError::OutputExists(_))
        ));
        assert_eq!(fs::read(&input).expect("input must read"), input_before);
    }

    #[test]
    fn cleanup_removes_only_owned_partial_or_hash_proven_final() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let input = directory.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let plan = plan_image_output(&input, 2, "job").expect("plan must succeed");
        rgb_png(&plan.partial_path, 4, 4, [4, 5, 6]);
        fs::write(&plan.final_path, b"pre-existing").expect("existing output must save");
        let marker = directory.path().join("verification.json");
        cleanup_owned_output(&plan, &marker).expect("cleanup must succeed");
        assert!(!plan.partial_path.exists());
        assert_eq!(
            fs::read(&plan.final_path).expect("existing must read"),
            b"pre-existing"
        );

        // A crash after recording publish intent but before rename leaves the partial in place.
        // Even an independently-created final with identical bytes must not be deleted.
        fs::remove_file(&plan.final_path).expect("existing output must remove for fixture");
        rgb_png(&plan.partial_path, 4, 4, [4, 5, 6]);
        fs::copy(&plan.partial_path, &plan.final_path).expect("identical raced final must copy");
        let input_hash = sha256_file(&input).unwrap();
        write_json_atomic(
            &marker,
            &ImageVerification {
                schema_version: 1,
                job_id: plan.job_id.clone(),
                input_path: input.clone(),
                input_sha256_before: input_hash.clone(),
                input_sha256_after: input_hash,
                output_path: plan.final_path.clone(),
                output_sha256: sha256_file(&plan.final_path).unwrap(),
                output_format: "png".into(),
                output_width: 4,
                output_height: 4,
            },
        )
        .unwrap();
        cleanup_owned_output(&plan, &marker).expect("pre-rename cleanup must succeed");
        assert!(plan.final_path.exists());
        assert!(!plan.partial_path.exists());
        assert!(!marker.exists());

        fs::remove_file(&plan.final_path).expect("raced final must remove for owned fixture");
        rgb_png(&plan.partial_path, 4, 4, [4, 5, 6]);
        publish_verified_output(&plan, &marker).expect("owned output must publish");
        cleanup_owned_output(&plan, &marker).expect("owned cleanup must succeed");
        assert!(!plan.final_path.exists());
        assert!(!marker.exists());
    }
}
