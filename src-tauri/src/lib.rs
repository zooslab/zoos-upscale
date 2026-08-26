#[cfg(debug_assertions)]
mod commands;

#[cfg(debug_assertions)]
use std::path::PathBuf;
#[cfg(debug_assertions)]
use std::time::Duration;

#[cfg(debug_assertions)]
use tauri::Manager;
#[cfg(debug_assertions)]
use zoos_core::JobOrchestrator;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    #[cfg(debug_assertions)]
    let builder = builder
        .setup(|app| {
            let workspace_root = app.path().app_data_dir()?.join("job-workspaces");
            let orchestrator = JobOrchestrator::new(
                workspace_root,
                resolve_fake_runner_path()?,
                Duration::from_secs(5),
                Duration::from_secs(2),
            )?;
            app.manage(orchestrator);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_fake_job,
            commands::list_jobs,
            commands::start_job,
            commands::cancel_job
        ]);

    builder
        .run(tauri::generate_context!())
        .expect("failed to run Zoos Upscale");
}

#[cfg(debug_assertions)]
fn resolve_fake_runner_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let file_name = if cfg!(windows) {
        "zoos-runner-fake.exe"
    } else {
        "zoos-runner-fake"
    };
    let current_executable = std::env::current_exe()?;
    let sibling = current_executable
        .parent()
        .ok_or("application executable has no parent directory")?
        .join(file_name);
    if sibling.is_file() {
        return Ok(sibling);
    }

    let manifest_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_directory
        .parent()
        .ok_or("src-tauri has no workspace parent")?;
    let development_runner = workspace_root.join("target/debug").join(file_name);
    Ok(development_runner)
}
