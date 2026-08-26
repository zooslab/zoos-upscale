#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use image::{Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zoos_core::{
    Ffprobe, JobOrchestrator, JobStatus, MediaDescriptor, RationalRate, RunnerLaunchSpec,
    RunnerRegistry, VideoBackend, VideoPipelineVerification, VideoSettings,
};

const FFMPEG_ENV: &str = "ZOOS_M5_FFMPEG_ASSETS";
const RIFE_ENV: &str = "ZOOS_M5_RIFE_ASSETS";
const WRAPPER_ENV: &str = "ZOOS_M5_RIFE_WRAPPER";
const UPDATE_ENV: &str = "ZOOS_UPDATE_GOAL2_EVIDENCE";
const CASE_ENV: &str = "ZOOS_GOAL2_CASE";
const FFMPEG_SHA256: &str = "653e700a788f3376ebc3817a3dcda56e111111410f7edd8eea919c4089216d4e";
const FFPROBE_SHA256: &str = "edaf9c5f53aef960ceb5f779d986e7dea86ee549e6716a2c03b70010b88a4da6";
const RIFE_SHA256: &str = "d11429c72f0cddfb170fd131ee9373dc5329a5729c4382c0acfd40092e5ed19a";
const RIFE_PARAM_SHA256: &str = "724569596bcd1e7b9fa50455c604777ebed99746d2ef40aa86e31b5725f1053c";
const RIFE_BIN_SHA256: &str = "f334ed2260149ce0188a6dcf049844e8b0cdd912e01cbcfb63553157d2508958";

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Thresholds {
    schema_version: u32,
    gate_id: String,
    cpu_gpu: QualityLimit,
    scene_cut: SceneCutLimit,
    max_av_sync_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct QualityLimit {
    measured_max_abs_error: u8,
    measured_mean_abs_error: f64,
    measured_psnr_db: f64,
    max_abs_error: u8,
    max_mean_abs_error: f64,
    min_psnr_db: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SceneCutLimit {
    measured_midpoint_max_abs_error: u8,
    max_midpoint_abs_error: u8,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
struct Evidence {
    schema_version: u32,
    gate_id: String,
    platform: String,
    gpu: String,
    cpu: String,
    regression_only: bool,
    output_hash_policy: String,
    cases: BTreeMap<String, CaseEvidence>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
struct CaseEvidence {
    backend: String,
    source_frames: u64,
    output_frames: u64,
    source_rate: String,
    target_rate: String,
    output_sha256: Option<String>,
    audio_streams: u32,
    subtitle_streams: u32,
    chapter_count: u32,
    chunk_count: u32,
    scene_cut_count: u64,
}

#[derive(Debug, Deserialize)]
struct SuccessManifest {
    result: Option<String>,
    actual_video_backend: Option<VideoBackend>,
    actual_device: Option<String>,
    ffmpeg_sha256: Option<String>,
    ffprobe_sha256: Option<String>,
    rife_engine_sha256: Option<String>,
    rife_model_param_sha256: Option<String>,
    rife_model_bin_sha256: Option<String>,
    source_frames: Option<u64>,
    output_frames: Option<u64>,
    scene_cut_count: Option<u64>,
    chunk_count: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
struct FixtureSpec {
    id: &'static str,
    rate: RationalRate,
    seconds: u32,
    extension: &'static str,
    audio_tracks: u8,
    subtitle: bool,
    chapters: bool,
}

struct Runtime {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    rife: PathBuf,
    models: PathBuf,
    wrapper: PathBuf,
}

struct Completed {
    verification: VideoPipelineVerification,
    descriptor: MediaDescriptor,
    output: PathBuf,
}

#[derive(Debug)]
struct Metrics {
    max_abs_error: u8,
    mean_abs_error: f64,
    psnr_db: f64,
}

#[tokio::test]
#[ignore = "requires verified Goal 2 FFmpeg/RIFE assets and Apple M5 GPU"]
async fn apple_m5_goal2_vertical_video_gate() {
    let repository = workspace_root();
    let thresholds_path = repository.join("tests/hardware/apple-m5-goal2-thresholds.json");
    let evidence_path = repository.join("tests/hardware/apple-m5-goal2-evidence.json");
    let mut thresholds: Thresholds = read_json(&thresholds_path);
    let committed_evidence: Evidence = read_json(&evidence_path);
    assert_eq!(thresholds.schema_version, 1);
    assert_eq!(thresholds.gate_id, "apple-m5-goal2");
    assert_eq!(committed_evidence.gate_id, thresholds.gate_id);

    let runtime = runtime(&repository);
    let case_filter = env::var(CASE_ENV).ok();
    let mut temporary = Some(
        tempfile::Builder::new()
            .prefix("zoos-apple-m5-goal2-")
            .tempdir()
            .expect("hardware gate temporary directory must be created"),
    );
    let temporary_path = temporary.as_ref().unwrap().path().to_path_buf();
    if case_filter.is_some() {
        let kept = temporary.take().unwrap().keep();
        eprintln!("GOAL2_M5 debug_root={}", kept.display());
    }
    let inputs = temporary_path.join("inputs");
    let workspace = temporary_path.join("workspace");
    fs::create_dir(&inputs).expect("input directory must be created");
    let probe = Ffprobe::new(
        runtime.ffprobe.clone(),
        Duration::from_secs(30),
        Duration::from_secs(2),
    )
    .expect("ffprobe must configure");
    let orchestrator = orchestrator(&workspace, &runtime, probe.clone());

    let cases = [
        FixtureSpec {
            id: "mp4-video-only-25",
            rate: rate(25, 1),
            seconds: 10,
            extension: "mp4",
            audio_tracks: 0,
            subtitle: false,
            chapters: false,
        },
        FixtureSpec {
            id: "mov-audio-subtitle-30",
            rate: rate(30, 1),
            seconds: 10,
            extension: "mov",
            audio_tracks: 1,
            subtitle: true,
            chapters: false,
        },
        FixtureSpec {
            id: "mkv-multi-audio-subtitle-ntsc",
            rate: rate(30_000, 1_001),
            seconds: 10,
            extension: "mkv",
            audio_tracks: 2,
            subtitle: true,
            chapters: true,
        },
        FixtureSpec {
            id: "mkv-video-only-60s",
            rate: rate(25, 1),
            seconds: 60,
            extension: "mkv",
            audio_tracks: 0,
            subtitle: false,
            chapters: false,
        },
    ];

    let mut measured = BTreeMap::new();
    let mut short_gpu = None;
    for spec in cases {
        if case_filter
            .as_deref()
            .is_some_and(|filter| filter != spec.id)
        {
            continue;
        }
        let input = create_fixture(&runtime.ffmpeg, &inputs, spec);
        let source_hash = sha256_file(&input);
        let descriptor = probe.probe(&input).await.expect("source must probe");
        assert_eq!(descriptor.frame_rate, spec.rate);
        assert_eq!(descriptor.frame_count, expected_source_frames(spec));
        let repeat_count = if spec.id == "mp4-video-only-25" { 3 } else { 1 };
        for repeat in 0..repeat_count {
            let completed = run_job(
                &orchestrator,
                &probe,
                &workspace,
                descriptor.clone(),
                VideoBackend::VulkanGpu,
            )
            .await;
            assert_eq!(sha256_file(&input), source_hash);
            verify_completed(spec, &descriptor, &completed, &thresholds);
            eprintln!(
                "GOAL2_M5 case={} repeat={} output_sha={}",
                spec.id,
                repeat + 1,
                completed.verification.output_sha256
            );
            if repeat == 0 {
                measured.insert(
                    spec.id.into(),
                    evidence_for(VideoBackend::VulkanGpu, &completed),
                );
                if spec.id == "mp4-video-only-25" {
                    short_gpu = Some(completed);
                }
            }
        }
    }
    if case_filter.is_some() {
        return;
    }

    let cpu_spec = FixtureSpec {
        id: "mp4-video-only-cpu-25",
        rate: rate(25, 1),
        seconds: 2,
        extension: "mp4",
        audio_tracks: 0,
        subtitle: false,
        chapters: false,
    };
    let cpu_input = create_fixture(&runtime.ffmpeg, &inputs, cpu_spec);
    let cpu_source = probe
        .probe(&cpu_input)
        .await
        .expect("CPU source must probe");
    let cpu = run_job(
        &orchestrator,
        &probe,
        &workspace,
        cpu_source.clone(),
        VideoBackend::NcnnCpu,
    )
    .await;
    verify_completed(cpu_spec, &cpu_source, &cpu, &thresholds);
    measured.insert(
        cpu_spec.id.into(),
        evidence_for(VideoBackend::NcnnCpu, &cpu),
    );

    let compare_gpu = run_job(
        &orchestrator,
        &probe,
        &workspace,
        cpu_source,
        VideoBackend::VulkanGpu,
    )
    .await;
    let cpu_frames = decode_frames(&runtime.ffmpeg, &cpu.output, &temporary_path, "cpu");
    let gpu_frames = decode_frames(&runtime.ffmpeg, &compare_gpu.output, &temporary_path, "gpu");
    let metrics = compare_frame_sets(&cpu_frames, &gpu_frames);
    eprintln!(
        "GOAL2_M5 cpu_gpu max_abs={} mean_abs={:.8} psnr_db={:.4}",
        metrics.max_abs_error, metrics.mean_abs_error, metrics.psnr_db
    );
    if env::var_os(UPDATE_ENV).is_some() {
        thresholds.cpu_gpu.measured_max_abs_error = metrics.max_abs_error;
        thresholds.cpu_gpu.measured_mean_abs_error = metrics.mean_abs_error;
        thresholds.cpu_gpu.measured_psnr_db = metrics.psnr_db;
    }
    // VideoToolbox output is not byte deterministic. Keep the last measured values as evidence,
    // but use the explicit quality guard rather than exact equality for pass/fail.
    assert!(metrics.max_abs_error <= thresholds.cpu_gpu.max_abs_error);
    assert!(metrics.mean_abs_error <= thresholds.cpu_gpu.max_mean_abs_error);
    assert!(metrics.psnr_db >= thresholds.cpu_gpu.min_psnr_db);

    let scene_gpu = short_gpu.expect("short GPU case must complete");
    let decoded = decode_frames(&runtime.ffmpeg, &scene_gpu.output, &temporary_path, "scene");
    let source_cut = expected_source_frames(cases[0]) / 2;
    let midpoint = &decoded[(source_cut * 2 - 1) as usize];
    let next_original = &decoded[(source_cut * 2) as usize];
    let midpoint_error = max_abs_error(midpoint, next_original);
    eprintln!("GOAL2_M5 scene_midpoint_max_abs={midpoint_error}");
    if env::var_os(UPDATE_ENV).is_some() {
        thresholds.scene_cut.measured_midpoint_max_abs_error = midpoint_error;
    }
    assert!(midpoint_error <= thresholds.scene_cut.max_midpoint_abs_error);
    assert!(scene_gpu.verification.scene_cut_count > 0);

    cancellation_gate(&orchestrator, &runtime, &probe, &workspace, &inputs).await;
    assert_no_residual_processes(&runtime);

    let evidence = Evidence {
        schema_version: 1,
        gate_id: "apple-m5-goal2".into(),
        platform: "macOS arm64".into(),
        gpu: "gpu:0 Apple M5 / Vulkan".into(),
        cpu: "Apple M5 / ncnn CPU -1".into(),
        regression_only: true,
        output_hash_policy: "verified_per_run_not_pinned".into(),
        cases: measured,
    };
    if env::var_os(UPDATE_ENV).is_some() {
        write_json(&thresholds_path, &thresholds);
        write_json(&evidence_path, &evidence);
        panic!("Goal 2 evidence updated; rerun without {UPDATE_ENV}");
    }
    assert_eq!(evidence, committed_evidence);
}

fn orchestrator(workspace: &Path, runtime: &Runtime, probe: Ffprobe) -> JobOrchestrator {
    let launch = RunnerLaunchSpec::new("zoos-runner-rife", runtime.wrapper.clone())
        .expect("runner launch must configure")
        .with_arguments([
            OsString::from("--ffmpeg"),
            runtime.ffmpeg.clone().into_os_string(),
            OsString::from("--ffprobe"),
            runtime.ffprobe.clone().into_os_string(),
            OsString::from("--engine"),
            runtime.rife.clone().into_os_string(),
            OsString::from("--models"),
            runtime.models.clone().into_os_string(),
        ])
        .expect("runner arguments must configure");
    JobOrchestrator::with_runner_registry_and_media_probe(
        workspace,
        RunnerRegistry::with_runner_id(launch),
        probe,
        Duration::from_secs(10),
        Duration::from_secs(2),
    )
    .expect("orchestrator must initialize")
}

async fn run_job(
    orchestrator: &JobOrchestrator,
    probe: &Ffprobe,
    workspace: &Path,
    descriptor: MediaDescriptor,
    backend: VideoBackend,
) -> Completed {
    let source = descriptor.clone();
    let source_sha256 = sha256_file(&descriptor.input_path);
    let created = orchestrator
        .create_video_job(
            descriptor,
            source_sha256,
            VideoSettings { backend },
            backend,
        )
        .expect("video job must create");
    orchestrator
        .start_job(&created.job_id)
        .await
        .expect("video job must start");
    let terminal = wait_for_terminal(orchestrator, &created.job_id).await;
    assert_eq!(terminal.status, JobStatus::Completed, "{terminal:?}");
    let output = terminal.output_path.expect("completed path");
    let descriptor = probe
        .probe_output(&output)
        .await
        .expect("output must probe");
    let verification: VideoPipelineVerification =
        read_json(&workspace.join(&created.job_id).join("verification.json"));
    let manifest: SuccessManifest =
        read_json(&workspace.join(&created.job_id).join("manifest.json"));
    assert_eq!(manifest.result.as_deref(), Some("completed"));
    assert_eq!(manifest.actual_video_backend, Some(backend));
    assert_eq!(manifest.source_frames, Some(source.frame_count));
    assert_eq!(manifest.output_frames, Some(source.frame_count * 2));
    assert_eq!(manifest.scene_cut_count, Some(verification.scene_cut_count));
    assert_eq!(manifest.chunk_count, Some(verification.chunk_count));
    assert_eq!(manifest.ffmpeg_sha256.as_deref(), Some(FFMPEG_SHA256));
    assert_eq!(manifest.ffprobe_sha256.as_deref(), Some(FFPROBE_SHA256));
    assert_eq!(manifest.rife_engine_sha256.as_deref(), Some(RIFE_SHA256));
    assert_eq!(
        manifest.rife_model_param_sha256.as_deref(),
        Some(RIFE_PARAM_SHA256)
    );
    assert_eq!(
        manifest.rife_model_bin_sha256.as_deref(),
        Some(RIFE_BIN_SHA256)
    );
    match backend {
        VideoBackend::VulkanGpu => assert_eq!(manifest.actual_device.as_deref(), Some("gpu:0")),
        VideoBackend::NcnnCpu => assert_eq!(manifest.actual_device.as_deref(), Some("cpu")),
        VideoBackend::Auto => panic!("auto is not a selected hardware backend"),
    }
    assert_work_is_empty(&workspace.join(&created.job_id).join("work"));
    assert_no_partial(&output);
    Completed {
        verification,
        descriptor,
        output,
    }
}

fn verify_completed(
    spec: FixtureSpec,
    source: &MediaDescriptor,
    completed: &Completed,
    thresholds: &Thresholds,
) {
    let verification = &completed.verification;
    assert_eq!(verification.source_rate, spec.rate);
    assert_eq!(verification.target_rate, doubled(spec.rate));
    assert_eq!(verification.source_frames, source.frame_count);
    assert_eq!(verification.output_frames, source.frame_count * 2);
    assert_eq!(completed.descriptor.frame_count, source.frame_count * 2);
    assert_eq!(completed.descriptor.frame_rate, doubled(spec.rate));
    assert_eq!(verification.audio_streams, u32::from(spec.audio_tracks));
    assert_eq!(verification.subtitle_streams, u32::from(spec.subtitle));
    assert_eq!(verification.chapter_count, u32::from(spec.chapters));
    assert_eq!(completed.descriptor.streams.len(), source.streams.len());
    assert_eq!(completed.descriptor.chapters.len(), source.chapters.len());
    assert!(verification.chunk_count > 0);
    assert_eq!(verification.output_sha256, sha256_file(&completed.output));
    assert!(
        source
            .duration_ms
            .abs_diff(completed.descriptor.duration_ms)
            <= thresholds.max_av_sync_ms
    );
    for stream in &completed.descriptor.streams {
        if stream.kind == zoos_runner_protocol::MuxStreamKind::Audio {
            assert!(
                stream
                    .duration_ms
                    .abs_diff(completed.descriptor.duration_ms)
                    <= thresholds.max_av_sync_ms
            );
        }
    }
}

async fn cancellation_gate(
    orchestrator: &JobOrchestrator,
    runtime: &Runtime,
    probe: &Ffprobe,
    workspace: &Path,
    inputs: &Path,
) {
    let spec = FixtureSpec {
        id: "cancel-30",
        rate: rate(30, 1),
        seconds: 10,
        extension: "mp4",
        audio_tracks: 0,
        subtitle: false,
        chapters: false,
    };
    let input = create_fixture(&runtime.ffmpeg, inputs, spec);
    let descriptor = probe.probe(&input).await.expect("cancel source must probe");
    let source_sha256 = sha256_file(&input);
    let created = orchestrator
        .create_video_job(
            descriptor,
            source_sha256,
            VideoSettings {
                backend: VideoBackend::VulkanGpu,
            },
            VideoBackend::VulkanGpu,
        )
        .expect("cancel job must create");
    orchestrator
        .start_job(&created.job_id)
        .await
        .expect("start");
    let mut saw_process = false;
    for _ in 0..400 {
        let current = current_job(orchestrator, &created.job_id);
        if current.status == JobStatus::Running && managed_process_is_running(runtime) {
            saw_process = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        saw_process,
        "cancellation gate never observed the native runner"
    );
    orchestrator
        .cancel_job(&created.job_id)
        .await
        .expect("cancel");
    let terminal = wait_for_terminal(orchestrator, &created.job_id).await;
    assert_eq!(terminal.status, JobStatus::Cancelled);
    assert!(terminal.output_path.is_some_and(|path| !path.exists()));
    assert_work_is_empty(&workspace.join(&created.job_id).join("work"));
    assert_no_residual_processes(runtime);
}

fn create_fixture(ffmpeg: &Path, root: &Path, spec: FixtureSpec) -> PathBuf {
    let case = root.join(spec.id);
    let frames = case.join("frames");
    fs::create_dir_all(&frames).expect("frame directory must create");
    let frame_count = expected_source_frames(spec);
    for index in 0..frame_count {
        fixture_frame(index, frame_count)
            .save(frames.join(format!("{index:08}.png")))
            .expect("fixture frame must save");
    }
    let mut arguments = vec![
        OsString::from("-nostdin"),
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-n"),
        OsString::from("-framerate"),
        OsString::from(rate_text(spec.rate)),
        OsString::from("-start_number"),
        OsString::from("0"),
        OsString::from("-i"),
        frames.join("%08d.png").into_os_string(),
    ];
    for track in 0..spec.audio_tracks {
        let wav = case.join(format!("audio-{track}.wav"));
        write_wav(&wav, spec.seconds, 220 + u32::from(track) * 110);
        arguments.extend([OsString::from("-i"), wav.into_os_string()]);
    }
    let subtitle_input = if spec.subtitle {
        let path = case.join("subtitle.srt");
        fs::write(
            &path,
            format!(
                "1\n00:00:00,000 --> 00:00:{:02},000\nZoos Goal 2\n",
                spec.seconds - 1
            ),
        )
        .expect("subtitle must write");
        arguments.extend([OsString::from("-i"), path.into_os_string()]);
        Some(1 + u32::from(spec.audio_tracks))
    } else {
        None
    };
    let chapter_input = if spec.chapters {
        let path = case.join("chapters.ffmeta");
        fs::write(
            &path,
            format!(
                ";FFMETADATA1\ntitle=Goal 2 fixture\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND={}\ntitle=First\n",
                u64::from(spec.seconds) * 1_000
            ),
        )
        .expect("chapter metadata must write");
        arguments.extend([
            OsString::from("-f"),
            OsString::from("ffmetadata"),
            OsString::from("-i"),
            path.into_os_string(),
        ]);
        Some(1 + u32::from(spec.audio_tracks) + u32::from(spec.subtitle))
    } else {
        None
    };
    arguments.extend([OsString::from("-map"), OsString::from("0:v:0")]);
    for track in 0..spec.audio_tracks {
        arguments.extend([
            OsString::from("-map"),
            OsString::from(format!("{}:a:0", 1 + u32::from(track))),
        ]);
    }
    if let Some(index) = subtitle_input {
        arguments.extend([
            OsString::from("-map"),
            OsString::from(format!("{index}:s:0")),
        ]);
    }
    if let Some(index) = chapter_input {
        arguments.extend([
            OsString::from("-map_metadata"),
            OsString::from(index.to_string()),
            OsString::from("-map_chapters"),
            OsString::from(index.to_string()),
        ]);
    }
    arguments.extend([
        OsString::from("-c:v"),
        OsString::from("h264_videotoolbox"),
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
        OsString::from("-b:v"),
        OsString::from("1000000"),
    ]);
    if spec.audio_tracks > 0 {
        arguments.extend([
            OsString::from("-c:a"),
            OsString::from("aac"),
            OsString::from("-b:a"),
            OsString::from("96000"),
        ]);
    }
    if spec.subtitle {
        arguments.extend([
            OsString::from("-c:s"),
            OsString::from(if spec.extension == "mkv" {
                "subrip"
            } else {
                "mov_text"
            }),
        ]);
    }
    arguments.extend([
        OsString::from("-metadata"),
        OsString::from(format!("title=Zoos Goal 2 {}", spec.id)),
    ]);
    for track in 0..spec.audio_tracks {
        let language = if track == 0 { "kor" } else { "eng" };
        arguments.extend([
            OsString::from(format!("-metadata:s:a:{track}")),
            OsString::from(format!("language={language}")),
            OsString::from(format!("-metadata:s:a:{track}")),
            OsString::from(format!("title=Audio {}", track + 1)),
        ]);
    }
    if spec.subtitle {
        arguments.extend([
            OsString::from("-metadata:s:s:0"),
            OsString::from("language=kor"),
            OsString::from("-metadata:s:s:0"),
            OsString::from("title=Korean subtitle"),
        ]);
    }
    let output = case.join(format!("source.{}", spec.extension));
    arguments.push(output.clone().into_os_string());
    command_success(ffmpeg, &arguments);
    assert!(output.is_file());
    output
}

fn fixture_frame(index: u64, frame_count: u64) -> RgbImage {
    let after_cut = index >= frame_count / 2;
    let mut image = RgbImage::from_fn(64, 64, |x, y| {
        let motion = ((index * 3 + u64::from(x) + u64::from(y)) % 80) as u8;
        if after_cut {
            Rgb([170 + motion / 2, 30 + motion / 4, 190 + motion / 3])
        } else {
            Rgb([20 + motion / 3, 90 + motion / 2, 30 + motion / 4])
        }
    });
    let square_x = u32::try_from((index * 2) % 48).unwrap();
    for y in 24..40 {
        for x in square_x..square_x + 16 {
            image.put_pixel(
                x,
                y,
                if after_cut {
                    Rgb([0, 255, 255])
                } else {
                    Rgb([255, 255, 0])
                },
            );
        }
    }
    image
}

fn write_wav(path: &Path, seconds: u32, frequency: u32) {
    let sample_rate = 48_000u32;
    let samples = sample_rate * seconds;
    let data_bytes = samples * 2;
    let mut file = File::create(path).expect("WAV must create");
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
    file.write_all(b"WAVEfmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
    file.write_all(&2u16.to_le_bytes()).unwrap();
    file.write_all(&16u16.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_bytes.to_le_bytes()).unwrap();
    for sample in 0..samples {
        let phase = (sample % (sample_rate / frequency)) as f32 / (sample_rate / frequency) as f32;
        let value = ((phase * std::f32::consts::TAU).sin() * 4_000.0) as i16;
        file.write_all(&value.to_le_bytes()).unwrap();
    }
    file.sync_all().unwrap();
}

fn decode_frames(ffmpeg: &Path, input: &Path, root: &Path, name: &str) -> Vec<RgbImage> {
    let directory = root.join(format!("decoded-{name}"));
    fs::create_dir(&directory).expect("decoded directory must create");
    command_success(
        ffmpeg,
        &[
            OsString::from("-nostdin"),
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
            OsString::from("-n"),
            OsString::from("-i"),
            input.as_os_str().to_owned(),
            OsString::from("-map"),
            OsString::from("0:v:0"),
            OsString::from("-fps_mode"),
            OsString::from("passthrough"),
            OsString::from("-start_number"),
            OsString::from("0"),
            directory.join("%08d.png").into_os_string(),
        ],
    );
    let mut paths = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .iter()
        .map(|path| image::open(path).unwrap().into_rgb8())
        .collect()
}

fn compare_frame_sets(left: &[RgbImage], right: &[RgbImage]) -> Metrics {
    assert_eq!(left.len(), right.len());
    let mut max = 0u8;
    let mut sum = 0u64;
    let mut squared = 0f64;
    let mut count = 0u64;
    for (left, right) in left.iter().zip(right) {
        assert_eq!(left.dimensions(), right.dimensions());
        for (left, right) in left.as_raw().iter().zip(right.as_raw()) {
            let difference = left.abs_diff(*right);
            max = max.max(difference);
            sum += u64::from(difference);
            squared += f64::from(difference).powi(2);
            count += 1;
        }
    }
    let mean = sum as f64 / count as f64;
    let mse = squared / count as f64;
    Metrics {
        max_abs_error: max,
        mean_abs_error: mean,
        psnr_db: if mse == 0.0 {
            999.0
        } else {
            10.0 * ((255.0 * 255.0) / mse).log10()
        },
    }
}

fn max_abs_error(left: &RgbImage, right: &RgbImage) -> u8 {
    left.as_raw()
        .iter()
        .zip(right.as_raw())
        .map(|(left, right)| left.abs_diff(*right))
        .max()
        .unwrap_or(0)
}

fn evidence_for(backend: VideoBackend, completed: &Completed) -> CaseEvidence {
    let verification = &completed.verification;
    CaseEvidence {
        backend: match backend {
            VideoBackend::VulkanGpu => "vulkan_gpu",
            VideoBackend::NcnnCpu => "ncnn_cpu",
            VideoBackend::Auto => "auto",
        }
        .into(),
        source_frames: verification.source_frames,
        output_frames: verification.output_frames,
        source_rate: rate_text(verification.source_rate),
        target_rate: rate_text(verification.target_rate),
        output_sha256: None,
        audio_streams: verification.audio_streams,
        subtitle_streams: verification.subtitle_streams,
        chapter_count: verification.chapter_count,
        chunk_count: verification.chunk_count,
        scene_cut_count: verification.scene_cut_count,
    }
}

async fn wait_for_terminal(orchestrator: &JobOrchestrator, job_id: &str) -> zoos_core::JobSummary {
    for _ in 0..24_000 {
        let current = current_job(orchestrator, job_id);
        if current.status.is_terminal() {
            return current;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job did not reach terminal state: {job_id}");
}

fn current_job(orchestrator: &JobOrchestrator, job_id: &str) -> zoos_core::JobSummary {
    let jobs = orchestrator
        .list_jobs()
        .unwrap_or_else(|error| panic!("jobs must list: {error}"));
    jobs.into_iter()
        .find(|job| job.job_id == job_id)
        .unwrap_or_else(|| panic!("job must remain visible: {job_id}"))
}

fn assert_no_partial(final_path: &Path) {
    let parent = final_path.parent().expect("output parent");
    for entry in fs::read_dir(parent).expect("output directory") {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains(".partial."), "partial remained: {name}");
    }
}

fn assert_work_is_empty(work: &Path) {
    if work.exists() {
        let entries = fs::read_dir(work)
            .expect("work directory must be readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("work entries must be readable");
        assert!(
            entries.iter().all(|entry| {
                entry.file_name() == OsStr::new("runner-evidence.json")
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
            }),
            "unverified work artifacts remained: {entries:?}"
        );
    }
}

fn assert_no_residual_processes(runtime: &Runtime) {
    std::thread::sleep(Duration::from_millis(250));
    assert!(!managed_process_is_running(runtime));
}

fn managed_process_is_running(runtime: &Runtime) -> bool {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("ps must run");
    let text = String::from_utf8_lossy(&output.stdout);
    [&runtime.wrapper, &runtime.rife, &runtime.ffmpeg]
        .iter()
        .any(|path| {
            let needle = path.to_string_lossy();
            text.lines().any(|line| line.contains(needle.as_ref()))
        })
}

fn runtime(repository: &Path) -> Runtime {
    let ffmpeg_root = absolute_env_or(
        FFMPEG_ENV,
        repository.join(".cache/runtime-assets/ffmpeg-macos-arm64/9.0.1"),
    );
    let rife_root = absolute_env_or(
        RIFE_ENV,
        repository.join(".cache/runtime-assets/rife-ncnn-vulkan-macos/20221029/macos-universal"),
    );
    Runtime {
        ffmpeg: ffmpeg_root.join("bin/ffmpeg"),
        ffprobe: ffmpeg_root.join("bin/ffprobe"),
        rife: rife_root.join("bin/rife-ncnn-vulkan"),
        models: rife_root.join("models/rife-v4.6"),
        wrapper: absolute_env_or(
            WRAPPER_ENV,
            repository.join("target/debug/zoos-runner-rife-bin"),
        ),
    }
}

fn absolute_env_or(name: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(name).map_or(fallback, PathBuf::from)
}

fn expected_source_frames(spec: FixtureSpec) -> u64 {
    (u64::from(spec.rate.numerator) * u64::from(spec.seconds)
        + u64::from(spec.rate.denominator) / 2)
        / u64::from(spec.rate.denominator)
}

const fn rate(numerator: u32, denominator: u32) -> RationalRate {
    RationalRate {
        numerator,
        denominator,
    }
}

fn doubled(rate: RationalRate) -> RationalRate {
    RationalRate {
        numerator: rate.numerator * 2,
        denominator: rate.denominator,
    }
}

fn rate_text(rate: RationalRate) -> String {
    format!("{}/{}", rate.numerator, rate.denominator)
}

fn command_success(program: &Path, arguments: &[OsString]) {
    assert!(program.is_absolute());
    let output = Command::new(program)
        .args(arguments)
        .output()
        .expect("command must start");
    assert!(
        output.status.success(),
        "{} failed: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256_file(path: &Path) -> String {
    let mut file = File::open(path).expect("file must open for hashing");
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("file must hash");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    format!("{:x}", hasher.finalize())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    serde_json::from_slice(
        &fs::read(path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("invalid JSON {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) {
    let mut bytes = serde_json::to_vec_pretty(value).expect("JSON must serialize");
    bytes.push(b'\n');
    fs::write(path, bytes).expect("evidence JSON must write");
}
