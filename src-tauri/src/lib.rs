mod commands;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use commands::{
    ImageRuntime, cpu_model_asset_directory, cpu_runtime_asset_directory, runtime_asset_directory,
};
use tauri::Manager;
use zoos_core::{JobKind, JobOrchestrator, RunnerLaunchSpec, RunnerRegistry};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(
            |app, _arguments, _working_directory| {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            },
        ))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let runtime = resolve_image_runtime(app.handle())?;
            let gpu_launch =
                RunnerLaunchSpec::new("zoos-runner-realesrgan", runtime.gpu_wrapper_path.clone())?
                    .with_arguments([
                        OsString::from("--engine"),
                        runtime.gpu_engine_path().into_os_string(),
                        OsString::from("--models"),
                        runtime.gpu_models_path().into_os_string(),
                    ])?;
            let cpu_launch =
                RunnerLaunchSpec::new("zoos-runner-ort", runtime.cpu_wrapper_path.clone())?
                    .with_arguments([
                        OsString::from("--runtime"),
                        runtime.cpu_runtime_path().into_os_string(),
                        OsString::from("--models"),
                        runtime.cpu_models_path().into_os_string(),
                    ])?;
            let mut runners = RunnerRegistry::with_runner(JobKind::ImageUpscale, gpu_launch);
            runners.register_runner(cpu_launch);

            #[cfg(debug_assertions)]
            let runners = {
                let mut runners = runners;
                runners.register(
                    JobKind::FakeValidation,
                    RunnerLaunchSpec::new("zoos-runner-fake", resolve_fake_runner_path()?)?,
                );
                runners
            };

            let workspace_root = app.path().app_data_dir()?.join("job-workspaces");
            let orchestrator = JobOrchestrator::with_runner_registry(
                workspace_root,
                runners,
                Duration::from_secs(5),
                Duration::from_secs(2),
            )?;
            app.manage(runtime);
            app.manage(orchestrator);
            Ok(())
        });

    #[cfg(debug_assertions)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        commands::get_image_engine_status,
        commands::pick_and_create_image_job,
        commands::pick_and_create_image_batch,
        commands::list_jobs,
        commands::start_job,
        commands::cancel_job,
        commands::cancel_batch,
        commands::create_fake_job,
    ]);

    #[cfg(not(debug_assertions))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        commands::get_image_engine_status,
        commands::pick_and_create_image_job,
        commands::pick_and_create_image_batch,
        commands::list_jobs,
        commands::start_job,
        commands::cancel_job,
        commands::cancel_batch,
    ]);

    builder
        .run(tauri::generate_context!())
        .expect("failed to run Zoos Upscale");
}

fn resolve_image_runtime<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<ImageRuntime, Box<dyn std::error::Error>> {
    let gpu_wrapper_path = resolve_sibling_or_development_runner("zoos-runner-realesrgan")?;
    let cpu_wrapper_path = resolve_sibling_or_development_runner("zoos-runner-ort")?;

    #[cfg(debug_assertions)]
    let gpu_install_directory = match std::env::var_os("ZOOS_RUNTIME_ASSETS_DIR") {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err("ZOOS_RUNTIME_ASSETS_DIR must be absolute".into());
            }
            path
        }
        None => runtime_asset_directory(&workspace_root()?.join(".cache/runtime-assets")),
    };

    #[cfg(debug_assertions)]
    let (cpu_runtime_directory, cpu_model_directory) = {
        let root = workspace_root()?;
        let runtime = std::env::var_os("ZOOS_ORT_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| cpu_runtime_asset_directory(&root.join(".cache/runtime-assets")));
        let models = std::env::var_os("ZOOS_ORT_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| cpu_model_asset_directory(&root.join(".cache/model-assets")));
        if !runtime.is_absolute() || !models.is_absolute() {
            return Err("Goal 1B asset directories must be absolute".into());
        }
        (runtime, models)
    };

    #[cfg(not(debug_assertions))]
    let gpu_install_directory =
        runtime_asset_directory(&app.path().app_cache_dir()?.join("runtime-assets"));

    #[cfg(not(debug_assertions))]
    let cpu_runtime_directory =
        cpu_runtime_asset_directory(&app.path().app_cache_dir()?.join("runtime-assets"));

    #[cfg(not(debug_assertions))]
    let cpu_model_directory =
        cpu_model_asset_directory(&app.path().app_cache_dir()?.join("model-assets"));

    #[cfg(debug_assertions)]
    let _ = app;

    Ok(ImageRuntime {
        gpu_wrapper_path,
        gpu_install_directory,
        cpu_wrapper_path,
        cpu_runtime_directory,
        cpu_model_directory,
    })
}

fn resolve_sibling_or_development_runner(
    name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let file_name = executable_name(name);
    let current_executable = std::env::current_exe()?;
    let sibling = current_executable
        .parent()
        .ok_or("application executable has no parent directory")?
        .join(&file_name);
    if sibling.is_file() || !cfg!(debug_assertions) {
        return Ok(sibling);
    }

    Ok(workspace_root()?
        .join("target/debug")
        .join(format!("{name}-bin{}", std::env::consts::EXE_SUFFIX)))
}

#[cfg(debug_assertions)]
fn resolve_fake_runner_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    resolve_sibling_or_development_runner("zoos-runner-fake")
}

fn executable_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn workspace_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_directory = Path::new(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest_directory
        .parent()
        .ok_or("src-tauri has no workspace parent")?
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_cache_layout_is_pinned_to_the_catalog_version() {
        let root = Path::new("/tmp/runtime-cache");
        assert_eq!(
            runtime_asset_directory(root),
            root.join("realesrgan-ncnn-vulkan-macos/0.2.5.0/macos-universal")
        );
        assert_eq!(
            cpu_runtime_asset_directory(root),
            root.join("onnxruntime-macos-arm64/1.29.0")
        );
        assert_eq!(
            cpu_model_asset_directory(Path::new("/tmp/model-cache")),
            Path::new("/tmp/model-cache/realesrgan-onnx/goal1b-v1")
        );
    }

    #[test]
    fn image_runner_arguments_are_explicit_absolute_paths() {
        let runtime = ImageRuntime {
            gpu_wrapper_path: PathBuf::from("/tmp/gpu-wrapper"),
            gpu_install_directory: PathBuf::from("/tmp/gpu-assets"),
            cpu_wrapper_path: PathBuf::from("/tmp/cpu-wrapper"),
            cpu_runtime_directory: PathBuf::from("/tmp/cpu-runtime"),
            cpu_model_directory: PathBuf::from("/tmp/cpu-models"),
        };
        let gpu = RunnerLaunchSpec::new("zoos-runner-realesrgan", runtime.gpu_wrapper_path.clone())
            .expect("absolute wrapper")
            .with_arguments([
                OsString::from("--engine"),
                runtime.gpu_engine_path().into_os_string(),
                OsString::from("--models"),
                runtime.gpu_models_path().into_os_string(),
            ])
            .expect("fixed arguments");
        assert_eq!(
            gpu.arguments,
            vec![
                OsString::from("--engine"),
                OsString::from("/tmp/gpu-assets/bin/realesrgan-ncnn-vulkan"),
                OsString::from("--models"),
                OsString::from("/tmp/gpu-assets/models"),
            ]
        );
        let cpu = RunnerLaunchSpec::new("zoos-runner-ort", runtime.cpu_wrapper_path.clone())
            .expect("absolute wrapper")
            .with_arguments([
                OsString::from("--runtime"),
                runtime.cpu_runtime_path().into_os_string(),
                OsString::from("--models"),
                runtime.cpu_models_path().into_os_string(),
            ])
            .expect("fixed arguments");
        assert_eq!(
            cpu.arguments,
            vec![
                OsString::from("--runtime"),
                OsString::from("/tmp/cpu-runtime/lib/libonnxruntime.1.29.0.dylib"),
                OsString::from("--models"),
                OsString::from("/tmp/cpu-models/models"),
            ]
        );
    }
}
