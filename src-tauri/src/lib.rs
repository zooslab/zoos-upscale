mod commands;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use commands::{ImageRuntime, runtime_asset_directory};
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
            let image_launch =
                RunnerLaunchSpec::new("zoos-runner-realesrgan", runtime.wrapper_path.clone())?
                    .with_arguments([
                        OsString::from("--engine"),
                        runtime.engine_path().into_os_string(),
                        OsString::from("--models"),
                        runtime.models_path().into_os_string(),
                    ])?;
            let runners = RunnerRegistry::with_runner(JobKind::ImageUpscale, image_launch);

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
        commands::list_jobs,
        commands::start_job,
        commands::cancel_job,
        commands::create_fake_job,
    ]);

    #[cfg(not(debug_assertions))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        commands::get_image_engine_status,
        commands::pick_and_create_image_job,
        commands::list_jobs,
        commands::start_job,
        commands::cancel_job,
    ]);

    builder
        .run(tauri::generate_context!())
        .expect("failed to run Zoos Upscale");
}

fn resolve_image_runtime<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<ImageRuntime, Box<dyn std::error::Error>> {
    let wrapper_path = resolve_sibling_or_development_runner("zoos-runner-realesrgan")?;

    #[cfg(debug_assertions)]
    let install_directory = match std::env::var_os("ZOOS_RUNTIME_ASSETS_DIR") {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err("ZOOS_RUNTIME_ASSETS_DIR must be absolute".into());
            }
            path
        }
        None => runtime_asset_directory(&workspace_root()?.join(".cache/runtime-assets")),
    };

    #[cfg(not(debug_assertions))]
    let install_directory =
        runtime_asset_directory(&app.path().app_cache_dir()?.join("runtime-assets"));

    #[cfg(debug_assertions)]
    let _ = app;

    Ok(ImageRuntime {
        wrapper_path,
        install_directory,
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
    }

    #[test]
    fn image_runner_arguments_are_explicit_absolute_paths() {
        let runtime = ImageRuntime {
            wrapper_path: PathBuf::from("/tmp/wrapper"),
            install_directory: PathBuf::from("/tmp/assets"),
        };
        let launch = RunnerLaunchSpec::new("zoos-runner-realesrgan", runtime.wrapper_path.clone())
            .expect("absolute wrapper")
            .with_arguments([
                OsString::from("--engine"),
                runtime.engine_path().into_os_string(),
                OsString::from("--models"),
                runtime.models_path().into_os_string(),
            ])
            .expect("fixed arguments");
        assert_eq!(
            launch.arguments,
            vec![
                OsString::from("--engine"),
                OsString::from("/tmp/assets/bin/realesrgan-ncnn-vulkan"),
                OsString::from("--models"),
                OsString::from("/tmp/assets/models"),
            ]
        );
    }
}
