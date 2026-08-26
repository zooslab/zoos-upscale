use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use image::codecs::png::PngEncoder;
use image::{ColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, RgbImage};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Tensor;
use sha2::{Digest, Sha256};
use zoos_runner_protocol::{
    DeviceCapability, ImageBackendSettingsV2, ImageDeviceV2, ImageInferenceFormatV2,
    ImagePixelFormatV2, ImageSemanticModelV2, ImageUpscaleJobRequestV2, ModelCapability,
    PROTOCOL_VERSION, RunnerCapabilities, RunnerEvent, RunnerEventPayload, RunnerOutput,
    RunnerTask, UpstreamInfo,
};

const EXIT_SUCCESS: i32 = 0;
const EXIT_INVALID_INPUT: i32 = 10;
const EXIT_ASSET: i32 = 20;
const EXIT_UPSTREAM: i32 = 30;
const EXIT_CANCELLED: i32 = 50;
const CORE_TILE: u32 = 128;
const TILE_CONTEXT: u32 = 16;
const NATIVE_SCALE: u32 = 4;
const HEARTBEAT: Duration = Duration::from_secs(2);

const RUNTIME_HASH: &str = "68f6e54e695583adc371aef610ec4abb1ffaa3df656582922de7690f7e2000eb";
const PHOTO_MODEL: (&str, &str) = (
    "realesrgan-x4plus-fp32-opset17.onnx",
    "95c08dbcaa58b4fabae771e74ae458d93df59b86cdcb885b85ade5be4e7f826b",
);
const ANIME_MODEL: (&str, &str) = (
    "realesrgan-x4plus-anime-6b-fp32-opset17.onnx",
    "8244ce14b66d7f285f5ed4980ce53d098c9aa7c5533d8782a5deeb7217035eb1",
);

#[derive(Clone, Debug)]
struct Assets {
    runtime: PathBuf,
    models: PathBuf,
}

enum Action {
    Capabilities,
    Run(PathBuf),
}

pub fn run_cli(arguments: impl IntoIterator<Item = String>) -> i32 {
    ensure_signal_handlers();
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let (assets, action) = match parse_cli(&arguments) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_INVALID_INPUT;
        }
    };
    match action {
        Action::Capabilities => match verify_assets(&assets) {
            Ok(()) => {
                println!("{}", serde_json::json!(capabilities()));
                EXIT_SUCCESS
            }
            Err(error) => {
                eprintln!("{}: {}", asset_error_code(&assets), error);
                EXIT_ASSET
            }
        },
        Action::Run(job) => run_job_file(&job, &assets, &mut io::stdout().lock()),
    }
}

fn parse_cli(arguments: &[String]) -> Result<(Assets, Action), String> {
    let mut runtime = None;
    let mut models = None;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--runtime" | "--models" => {
                let flag = arguments[index].as_str();
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| format!("missing value for {flag}"))?;
                if flag == "--runtime" {
                    runtime = Some(PathBuf::from(value));
                } else {
                    models = Some(PathBuf::from(value));
                }
                index += 2;
            }
            value => {
                rest.push(value.to_owned());
                index += 1;
            }
        }
    }
    let assets = Assets {
        runtime: runtime.ok_or("--runtime is required")?,
        models: models.ok_or("--models is required")?,
    };
    if !assets.runtime.is_absolute() || !assets.models.is_absolute() {
        return Err("runtime and model paths must be absolute".into());
    }
    if assets.runtime.file_name().and_then(|value| value.to_str())
        != Some("libonnxruntime.1.29.0.dylib")
    {
        return Err("--runtime must name libonnxruntime.1.29.0.dylib".into());
    }
    let action = match rest.as_slice() {
        [flag, format] if flag == "--capabilities" && format == "--json" => {
            Action::Capabilities
        }
        [command, flag, path] if command == "run" && flag == "--job" => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("job path must be absolute".into());
            }
            Action::Run(path)
        }
        _ => return Err("usage: zoos-runner-ort --runtime <absolute libonnxruntime.1.29.0.dylib> --models <absolute> [--capabilities --json | run --job <absolute>]".into()),
    };
    Ok((assets, action))
}

fn capabilities() -> RunnerCapabilities {
    RunnerCapabilities {
        protocol_version: PROTOCOL_VERSION,
        runner_id: "zoos-runner-ort".into(),
        runner_version: env!("CARGO_PKG_VERSION").into(),
        tasks: vec![RunnerTask::ImageUpscale],
        upstream: Some(UpstreamInfo {
            name: "ONNX Runtime".into(),
            version: "1.29.0".into(),
            source_commit: None,
        }),
        models: vec![
            ModelCapability {
                id: "photo".into(),
                scales: vec![2, 4],
            },
            ModelCapability {
                id: "anime".into(),
                scales: vec![2, 4],
            },
        ],
        scales: vec![2, 4],
        devices: vec![DeviceCapability {
            index: 0,
            name: "CPU".into(),
            backend: "ort_cpu".into(),
        }],
        test_behaviors: Vec::new(),
    }
}

fn run_job_file(job_path: &Path, assets: &Assets, output: &mut impl Write) -> i32 {
    let request: ImageUpscaleJobRequestV2 = match File::open(job_path)
        .map_err(|error| error.to_string())
        .and_then(|file| serde_json::from_reader(file).map_err(|error| error.to_string()))
        .and_then(|request: ImageUpscaleJobRequestV2| {
            request
                .validate()
                .map(|()| request)
                .map_err(|error| error.to_string())
        }) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("invalid image job: {error}");
            return EXIT_INVALID_INPUT;
        }
    };
    run_job(&request, assets, output)
}

fn run_job(request: &ImageUpscaleJobRequestV2, assets: &Assets, output: &mut impl Write) -> i32 {
    CANCELLED.store(false, Ordering::SeqCst);
    let mut events = EventWriter::new(output, &request.job_id);
    if events.started("validating_assets").is_err() {
        return EXIT_UPSTREAM;
    }
    if let Err(error) = verify_assets(assets) {
        let _ = events.failed(asset_error_code(assets), &error);
        return EXIT_ASSET;
    }
    if let Err(error) = validate_cpu_request(request) {
        let _ = events.failed("UNSUPPORTED_IMAGE_MODE", &error);
        return EXIT_INVALID_INPUT;
    }
    match fs::symlink_metadata(&request.output.path) {
        Ok(_) => {
            let _ = events.failed("OUTPUT_EXISTS", "intermediate output already exists");
            return EXIT_UPSTREAM;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = events.failed(
                "UPSTREAM_FAILED",
                &format!("could not inspect output: {error}"),
            );
            return EXIT_UPSTREAM;
        }
    }

    let result = run_inference(request, assets, &mut events);
    match result {
        Ok(()) => {
            if events
                .emit(RunnerEventPayload::Completed {
                    output: RunnerOutput {
                        path: request.output.path.clone(),
                    },
                })
                .is_err()
            {
                let _ = fs::remove_file(&request.output.path);
                EXIT_UPSTREAM
            } else {
                EXIT_SUCCESS
            }
        }
        Err(error) => {
            let _ = events.failed(error.code, &error.message);
            error.exit_code
        }
    }
}

fn run_inference<W: Write>(
    request: &ImageUpscaleJobRequestV2,
    assets: &Assets,
    events: &mut EventWriter<'_, W>,
) -> Result<(), RunnerFailure> {
    let input = load_verified_input(request)?;
    let model = model_for(request.parameters.semantic_model);
    let model_path = assets.models.join(model.0);
    let (intra_threads, inter_threads) = match request.parameters.backend_settings {
        ImageBackendSettingsV2::OrtCpu {
            intra_threads,
            inter_threads,
            ..
        } => (intra_threads.min(8) as usize, inter_threads as usize),
        _ => return Err(RunnerFailure::input("ORT runner requires ort_cpu settings")),
    };
    if inter_threads != 1 {
        return Err(RunnerFailure::input("ORT inter_threads must equal 1"));
    }

    ort::init_from(&assets.runtime)
        .map_err(|error| RunnerFailure::asset(format!("could not load ONNX Runtime: {error}")))?
        .commit();
    let started = Instant::now();
    let mut session = create_session_with_heartbeat(&model_path, intra_threads, started, events)?;

    events
        .emit(RunnerEventPayload::Warning {
            code: "CPU_DEVICE".into(),
            message: format!("cpu ONNX Runtime 1.29.0, {intra_threads} intra-op threads"),
        })
        .map_err(RunnerFailure::io)?;

    let tiles = tile_plan(input.width(), input.height());
    let total = tiles.len() as u64;
    let mut destination =
        RgbImage::new(input.width() * NATIVE_SCALE, input.height() * NATIVE_SCALE);
    for (index, tile) in tiles.iter().enumerate() {
        if CANCELLED.load(Ordering::SeqCst) {
            return Err(RunnerFailure::cancelled());
        }
        let tensor = tile_tensor(&input, *tile);
        let result = infer_tile_with_heartbeat(
            &mut session,
            TileInference {
                input: tensor,
                width: tile.expanded_width(),
                height: tile.expanded_height(),
            },
            index as u64,
            total,
            started,
            events,
        )?;
        stitch_tile(&mut destination, *tile, &result)?;
        let completed = index as u64 + 1;
        events
            .progress(
                "upscaling",
                completed,
                total,
                started,
                Some(format!("tile-{completed}")),
            )
            .map_err(RunnerFailure::io)?;
    }
    if CANCELLED.load(Ordering::SeqCst) {
        return Err(RunnerFailure::cancelled());
    }
    encode_rgb_png_create_new(&request.output.path, &destination)?;
    if CANCELLED.load(Ordering::SeqCst) {
        let _ = fs::remove_file(&request.output.path);
        return Err(RunnerFailure::cancelled());
    }
    Ok(())
}

fn encode_rgb_png_create_new(path: &Path, image: &RgbImage) -> Result<(), RunnerFailure> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            RunnerFailure::upstream(format!("could not claim output path: {error}"))
        })?;
    let result = PngEncoder::new(&mut file)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ColorType::Rgb8.into(),
        )
        .map_err(|error| RunnerFailure::upstream(format!("could not encode RGB8 PNG: {error}")))
        .and_then(|()| {
            file.sync_all().map_err(|error| {
                RunnerFailure::upstream(format!("could not sync RGB8 PNG: {error}"))
            })
        });
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}

fn create_session_with_heartbeat<W: Write>(
    model_path: &Path,
    intra_threads: usize,
    started: Instant,
    events: &mut EventWriter<'_, W>,
) -> Result<Session, RunnerFailure> {
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(1);
        scope.spawn(move || {
            let result = (|| {
                let mut builder = Session::builder().map_err(|error| error.to_string())?;
                builder = builder
                    .with_optimization_level(GraphOptimizationLevel::All)
                    .map_err(|error| error.to_string())?;
                builder = builder
                    .with_intra_threads(intra_threads)
                    .map_err(|error| error.to_string())?;
                builder = builder
                    .with_inter_threads(1)
                    .map_err(|error| error.to_string())?;
                builder
                    .commit_from_file(model_path)
                    .map_err(|error| error.to_string())
            })();
            let _ = sender.send(result);
        });
        loop {
            match receiver.recv_timeout(HEARTBEAT) {
                Ok(Ok(session)) => return Ok(session),
                Ok(Err(error)) => {
                    return Err(RunnerFailure::upstream(format!(
                        "could not initialize model: {error}"
                    )));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    events
                        .progress("loading_model", 0, 1, started, Some("heartbeat".into()))
                        .map_err(RunnerFailure::io)?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RunnerFailure::upstream(
                        "ORT session initialization worker stopped",
                    ));
                }
            }
        }
    })
}

fn validate_cpu_request(request: &ImageUpscaleJobRequestV2) -> Result<(), String> {
    if request.parameters.device != ImageDeviceV2::Cpu {
        return Err("ORT runner accepts CPU jobs only".into());
    }
    match request.parameters.backend_settings {
        ImageBackendSettingsV2::OrtCpu {
            tile_size: CORE_TILE,
            intra_threads: 1..=8,
            inter_threads: 1,
        } => {}
        _ => {
            return Err(
                "ORT settings must use tile_size=128, intra_threads=1..8, inter_threads=1".into(),
            );
        }
    }
    if request.input.format != ImageInferenceFormatV2::Png
        || request.input.pixel_format != ImagePixelFormatV2::Rgb8
        || request.output.format != ImageInferenceFormatV2::Png
        || request.output.pixel_format != ImagePixelFormatV2::Rgb8
    {
        return Err("ORT runner accepts RGB8 PNG intermediate images only".into());
    }
    Ok(())
}

fn load_verified_input(request: &ImageUpscaleJobRequestV2) -> Result<RgbImage, RunnerFailure> {
    verify_hash(&request.input.path, &request.input.sha256)
        .map_err(RunnerFailure::input_changed)?;
    let reader = ImageReader::open(&request.input.path)
        .map_err(|error| RunnerFailure::input(format!("could not open input: {error}")))?
        .with_guessed_format()
        .map_err(|error| RunnerFailure::input(format!("could not detect input: {error}")))?;
    if reader.format() != Some(ImageFormat::Png) {
        return Err(RunnerFailure::input("inference input is not PNG"));
    }
    let decoder = reader
        .into_decoder()
        .map_err(|error| RunnerFailure::input(format!("could not decode input: {error}")))?;
    if decoder.color_type() != ColorType::Rgb8
        || decoder.dimensions() != (request.input.width, request.input.height)
    {
        return Err(RunnerFailure::input(
            "input mode or dimensions do not match the job",
        ));
    }
    drop(decoder);
    let image = image::open(&request.input.path)
        .map_err(|error| RunnerFailure::input(format!("could not decode RGB8 PNG: {error}")))?;
    match image {
        image::DynamicImage::ImageRgb8(rgb) => Ok(rgb),
        _ => Err(RunnerFailure::input("input is not RGB8")),
    }
}

fn model_for(model: ImageSemanticModelV2) -> (&'static str, &'static str) {
    match model {
        ImageSemanticModelV2::Photo => PHOTO_MODEL,
        ImageSemanticModelV2::Anime => ANIME_MODEL,
    }
}

fn verify_assets(assets: &Assets) -> Result<(), String> {
    if !assets.runtime.is_file() || !assets.models.is_dir() {
        return Err("verified ONNX Runtime assets are not installed".into());
    }
    verify_hash(&assets.runtime, RUNTIME_HASH)?;
    verify_hash(&assets.models.join(PHOTO_MODEL.0), PHOTO_MODEL.1)?;
    verify_hash(&assets.models.join(ANIME_MODEL.0), ANIME_MODEL.1)?;
    Ok(())
}

fn asset_error_code(assets: &Assets) -> &'static str {
    if assets.runtime.is_file() && assets.models.is_dir() {
        "ASSET_HASH_MISMATCH"
    } else {
        "ENGINE_NOT_INSTALLED"
    }
}

fn verify_hash(path: &Path, expected: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if format!("{:x}", hasher.finalize()) != expected {
        return Err(format!("asset hash mismatch for {}", path.display()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tile {
    core_x: u32,
    core_y: u32,
    core_width: u32,
    core_height: u32,
    expanded_x: u32,
    expanded_y: u32,
    expanded_right: u32,
    expanded_bottom: u32,
}

impl Tile {
    fn expanded_width(self) -> u32 {
        self.expanded_right - self.expanded_x
    }
    fn expanded_height(self) -> u32 {
        self.expanded_bottom - self.expanded_y
    }
}

fn tile_plan(width: u32, height: u32) -> Vec<Tile> {
    let mut tiles = Vec::new();
    for core_y in (0..height).step_by(CORE_TILE as usize) {
        for core_x in (0..width).step_by(CORE_TILE as usize) {
            let core_width = CORE_TILE.min(width - core_x);
            let core_height = CORE_TILE.min(height - core_y);
            tiles.push(Tile {
                core_x,
                core_y,
                core_width,
                core_height,
                expanded_x: core_x.saturating_sub(TILE_CONTEXT),
                expanded_y: core_y.saturating_sub(TILE_CONTEXT),
                expanded_right: (core_x + core_width + TILE_CONTEXT).min(width),
                expanded_bottom: (core_y + core_height + TILE_CONTEXT).min(height),
            });
        }
    }
    tiles
}

fn tile_tensor(image: &RgbImage, tile: Tile) -> Vec<f32> {
    let plane = (tile.expanded_width() * tile.expanded_height()) as usize;
    let mut tensor = vec![0.0; plane * 3];
    for y in tile.expanded_y..tile.expanded_bottom {
        for x in tile.expanded_x..tile.expanded_right {
            let pixel = image.get_pixel(x, y);
            let offset =
                ((y - tile.expanded_y) * tile.expanded_width() + (x - tile.expanded_x)) as usize;
            for channel in 0..3 {
                tensor[channel * plane + offset] = f32::from(pixel[channel]) / 255.0;
            }
        }
    }
    tensor
}

struct TileInference {
    input: Vec<f32>,
    width: u32,
    height: u32,
}

fn infer_tile_with_heartbeat<W: Write>(
    session: &mut Session,
    tile: TileInference,
    completed: u64,
    total: u64,
    started: Instant,
    events: &mut EventWriter<'_, W>,
) -> Result<Vec<f32>, RunnerFailure> {
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(1);
        scope.spawn(move || {
            let result = (|| {
                let tensor = Tensor::<f32>::from_array((
                    [1usize, 3, tile.height as usize, tile.width as usize],
                    tile.input.into_boxed_slice(),
                ))
                .map_err(|error| error.to_string())?;
                let outputs = session
                    .run(ort::inputs!["input" => tensor])
                    .map_err(|error| error.to_string())?;
                let value = outputs
                    .get("output")
                    .ok_or_else(|| "model did not return output".to_string())?;
                let (shape, data) = value
                    .try_extract_tensor::<f32>()
                    .map_err(|error| error.to_string())?;
                let expected = [
                    1_i64,
                    3,
                    i64::from(tile.height * NATIVE_SCALE),
                    i64::from(tile.width * NATIVE_SCALE),
                ];
                if **shape != expected {
                    return Err(format!("unexpected output shape: {shape:?}"));
                }
                Ok(data.to_vec())
            })();
            let _ = sender.send(result);
        });
        loop {
            match receiver.recv_timeout(HEARTBEAT) {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error)) => {
                    return Err(RunnerFailure::upstream(format!(
                        "ORT inference failed: {error}"
                    )));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    events
                        .progress(
                            "upscaling",
                            completed,
                            total,
                            started,
                            Some("heartbeat".into()),
                        )
                        .map_err(RunnerFailure::io)?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RunnerFailure::upstream("ORT inference worker stopped"));
                }
            }
        }
    })
}

fn stitch_tile(
    destination: &mut RgbImage,
    tile: Tile,
    output: &[f32],
) -> Result<(), RunnerFailure> {
    let output_width = tile.expanded_width() * NATIVE_SCALE;
    let output_height = tile.expanded_height() * NATIVE_SCALE;
    let plane = (output_width * output_height) as usize;
    if output.len() != plane * 3 {
        return Err(RunnerFailure::upstream(
            "ORT output tensor has an invalid length",
        ));
    }
    let crop_x = (tile.core_x - tile.expanded_x) * NATIVE_SCALE;
    let crop_y = (tile.core_y - tile.expanded_y) * NATIVE_SCALE;
    for y in 0..tile.core_height * NATIVE_SCALE {
        for x in 0..tile.core_width * NATIVE_SCALE {
            let source = ((crop_y + y) * output_width + crop_x + x) as usize;
            let mut pixel = [0u8; 3];
            for channel in 0..3 {
                let value = output[channel * plane + source];
                if !value.is_finite() {
                    return Err(RunnerFailure::upstream(
                        "ORT output contains a non-finite value",
                    ));
                }
                pixel[channel] = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            destination.put_pixel(
                tile.core_x * NATIVE_SCALE + x,
                tile.core_y * NATIVE_SCALE + y,
                image::Rgb(pixel),
            );
        }
    }
    Ok(())
}

struct EventWriter<'a, W> {
    output: &'a mut W,
    job_id: &'a str,
    sequence: u64,
}

impl<'a, W: Write> EventWriter<'a, W> {
    fn new(output: &'a mut W, job_id: &'a str) -> Self {
        Self {
            output,
            job_id,
            sequence: 1,
        }
    }
    fn emit(&mut self, payload: RunnerEventPayload) -> io::Result<()> {
        serde_json::to_writer(
            &mut *self.output,
            &RunnerEvent::new(self.sequence, self.job_id, payload),
        )?;
        self.output.write_all(b"\n")?;
        self.output.flush()?;
        self.sequence += 1;
        Ok(())
    }
    fn started(&mut self, stage: &str) -> io::Result<()> {
        self.emit(RunnerEventPayload::Started {
            stage: stage.into(),
        })
    }
    fn failed(&mut self, code: &str, message: &str) -> io::Result<()> {
        self.emit(RunnerEventPayload::Failed {
            error_code: code.into(),
            message: message.into(),
        })
    }
    fn progress(
        &mut self,
        stage: &str,
        completed: u64,
        total: u64,
        started: Instant,
        chunk_id: Option<String>,
    ) -> io::Result<()> {
        let elapsed = started.elapsed();
        let rate = if elapsed.is_zero() {
            0.0
        } else {
            completed as f64 / elapsed.as_secs_f64()
        };
        let remaining = if rate > 0.0 {
            Some((((total - completed) as f64 / rate) * 1000.0) as u64)
        } else {
            None
        };
        self.emit(RunnerEventPayload::Progress {
            stage: stage.into(),
            completed_units: completed,
            total_units: total,
            unit: "tile".into(),
            elapsed_ms: elapsed.as_millis() as u64,
            chunk_id,
            rate: Some(rate),
            rate_unit: Some("tiles_per_second".into()),
            estimated_remaining_ms: remaining,
        })
    }
}

#[derive(Debug)]
struct RunnerFailure {
    code: &'static str,
    message: String,
    exit_code: i32,
}

impl RunnerFailure {
    fn input(message: impl Into<String>) -> Self {
        Self {
            code: "UNSUPPORTED_IMAGE_MODE",
            message: message.into(),
            exit_code: EXIT_INVALID_INPUT,
        }
    }
    fn input_changed(message: impl Into<String>) -> Self {
        Self {
            code: "INPUT_CHANGED",
            message: message.into(),
            exit_code: EXIT_INVALID_INPUT,
        }
    }
    fn asset(message: impl Into<String>) -> Self {
        Self {
            code: "ASSET_HASH_MISMATCH",
            message: message.into(),
            exit_code: EXIT_ASSET,
        }
    }
    fn upstream(message: impl Into<String>) -> Self {
        Self {
            code: "UPSTREAM_FAILED",
            message: message.into(),
            exit_code: EXIT_UPSTREAM,
        }
    }
    fn cancelled() -> Self {
        Self {
            code: "CANCELLED",
            message: "CPU upscale was cancelled".into(),
            exit_code: EXIT_CANCELLED,
        }
    }
    fn io(error: io::Error) -> Self {
        Self::upstream(format!("could not write runner event: {error}"))
    }
}

static CANCELLED: AtomicBool = AtomicBool::new(false);
extern "C" fn cancellation_handler(_: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn ensure_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            cancellation_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            cancellation_handler as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(not(unix))]
fn ensure_signal_handlers() {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn cli_requires_absolute_pinned_runtime_and_models() {
        assert!(
            parse_cli(&[
                "--runtime".into(),
                "relative.dylib".into(),
                "--models".into(),
                "/tmp/models".into(),
                "--capabilities".into(),
                "--json".into()
            ])
            .is_err()
        );
        assert!(
            parse_cli(&[
                "--runtime".into(),
                "/tmp/libonnxruntime.1.29.0.dylib".into(),
                "--models".into(),
                "/tmp/models".into(),
                "--capabilities".into(),
                "--json".into()
            ])
            .is_ok()
        );
    }

    #[test]
    fn asset_hashes_are_pinned_to_catalog_values() {
        assert_eq!(
            RUNTIME_HASH,
            "68f6e54e695583adc371aef610ec4abb1ffaa3df656582922de7690f7e2000eb"
        );
        assert_eq!(
            PHOTO_MODEL.1,
            "95c08dbcaa58b4fabae771e74ae458d93df59b86cdcb885b85ade5be4e7f826b"
        );
        assert_eq!(
            ANIME_MODEL.1,
            "8244ce14b66d7f285f5ed4980ce53d098c9aa7c5533d8782a5deeb7217035eb1"
        );
    }

    #[test]
    fn tile_plan_has_context_and_covers_small_or_irregular_images() {
        assert_eq!(
            tile_plan(64, 48),
            vec![Tile {
                core_x: 0,
                core_y: 0,
                core_width: 64,
                core_height: 48,
                expanded_x: 0,
                expanded_y: 0,
                expanded_right: 64,
                expanded_bottom: 48
            }]
        );
        let tiles = tile_plan(257, 129);
        assert_eq!(tiles.len(), 6);
        assert_eq!(tiles[1].expanded_x, 112);
        assert_eq!(tiles[1].expanded_right, 257);
        assert_eq!(tiles[5].core_width, 1);
        assert_eq!(tiles[5].core_height, 1);
    }

    #[test]
    fn context_crop_stitches_only_the_core_and_rounds_rgb() {
        let tile = Tile {
            core_x: 1,
            core_y: 1,
            core_width: 1,
            core_height: 1,
            expanded_x: 0,
            expanded_y: 0,
            expanded_right: 2,
            expanded_bottom: 2,
        };
        let mut destination = RgbImage::new(8, 8);
        let plane = 8 * 8;
        let mut output = vec![0.0; plane * 3];
        for y in 4..8 {
            for x in 4..8 {
                let offset = y * 8 + x;
                output[offset] = 0.5;
                output[plane + offset] = 1.2;
                output[plane * 2 + offset] = -0.2;
            }
        }
        stitch_tile(&mut destination, tile, &output).unwrap();
        assert_eq!(destination.get_pixel(4, 4).0, [128, 255, 0]);
        assert_eq!(destination.get_pixel(7, 7).0, [128, 255, 0]);
        assert_eq!(destination.get_pixel(0, 0).0, [0, 0, 0]);
    }

    #[test]
    fn wrong_hash_is_detected_without_loading_ort() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input.png");
        fs::write(&input, b"changed").unwrap();
        assert!(
            verify_hash(&input, &"0".repeat(64))
                .unwrap_err()
                .contains("hash mismatch")
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_encoder_does_not_follow_or_remove_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let output = temp.path().join("output.png");
        let external = temp.path().join("outside.png");
        symlink(&external, &output).unwrap();

        let error = encode_rgb_png_create_new(&output, &RgbImage::new(2, 2)).unwrap_err();
        assert!(error.message.contains("claim output path"));
        assert!(!external.exists());
        assert!(
            fs::symlink_metadata(output)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn accepts_only_the_v2_cpu_ort_contract() {
        let request = ImageUpscaleJobRequestV2 {
            protocol_version: zoos_runner_protocol::IMAGE_PROTOCOL_VERSION_V2,
            job_id: "cpu-job".into(),
            task: zoos_runner_protocol::ImageTask::ImageUpscale,
            input: zoos_runner_protocol::ImageInferenceInputV2 {
                path: PathBuf::from("/tmp/input.png"),
                sha256: "0".repeat(64),
                width: 64,
                height: 48,
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            output: zoos_runner_protocol::ImageIntermediateOutputV2 {
                path: PathBuf::from("/tmp/output.png"),
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            parameters: zoos_runner_protocol::ImageUpscaleParametersV2 {
                semantic_model: ImageSemanticModelV2::Photo,
                requested_scale: 2,
                native_scale: 4,
                device: ImageDeviceV2::Cpu,
                backend_settings: ImageBackendSettingsV2::OrtCpu {
                    tile_size: 128,
                    intra_threads: 8,
                    inter_threads: 1,
                },
            },
        };
        assert!(request.validate().is_ok());
        assert!(validate_cpu_request(&request).is_ok());

        let mut gpu = request;
        gpu.parameters.device = ImageDeviceV2::Vulkan { index: 0 };
        gpu.parameters.backend_settings = ImageBackendSettingsV2::Vulkan {
            tile_size: 256,
            threads: "1:2:2".into(),
        };
        assert!(gpu.validate().is_ok());
        assert!(validate_cpu_request(&gpu).is_err());
    }
}
