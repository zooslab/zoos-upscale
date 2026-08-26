use std::fs;
use std::process::Command;

use zoos_runner_protocol::{
    FakeBehavior, FakeJobRequest, FakeParameters, FakeTask, RunnerCapabilities, RunnerEvent,
    RunnerInput, RunnerOutput,
};

#[test]
fn cli_reports_typed_capabilities() {
    let result = Command::new(env!("CARGO_BIN_EXE_zoos-runner-fake-bin"))
        .args(["--capabilities", "--json"])
        .output()
        .expect("runner must start");

    assert!(result.status.success());
    let capabilities: RunnerCapabilities =
        serde_json::from_slice(&result.stdout).expect("capabilities must be valid JSON");
    capabilities
        .validate()
        .expect("capabilities must satisfy the contract");
    assert_eq!(capabilities.runner_id, "zoos-runner-fake");
}

#[test]
fn cli_handles_unicode_and_space_paths_without_a_shell() {
    let directory = tempfile::tempdir().expect("temporary directory must be created");
    let workspace = directory.path().join("한글 workspace with spaces");
    fs::create_dir_all(&workspace).expect("workspace must be created");

    let input = workspace.join("입력 file.txt");
    fs::write(&input, "fake input").expect("input must be written");
    let output = workspace.join("final/결과 file.txt");
    let request = FakeJobRequest {
        protocol_version: 1,
        job_id: "unicode-job".into(),
        task: FakeTask::Fake,
        input: RunnerInput { path: input },
        output: RunnerOutput {
            path: output.clone(),
        },
        parameters: FakeParameters {
            steps: 2,
            step_delay_ms: 0,
        },
        test_behavior: FakeBehavior::Success,
    };
    let job_path = workspace.join("runner-job.json");
    fs::write(
        &job_path,
        serde_json::to_vec_pretty(&request).expect("request must serialize"),
    )
    .expect("job must be written");

    let result = Command::new(env!("CARGO_BIN_EXE_zoos-runner-fake-bin"))
        .args(["run", "--job"])
        .arg(&job_path)
        .output()
        .expect("runner must start");

    assert!(result.status.success());
    assert!(output.exists());
    let events = String::from_utf8(result.stdout).expect("stdout must be UTF-8");
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<RunnerEvent>(line).expect("valid NDJSON event"))
        .collect::<Vec<_>>();
    assert_eq!(events.first().expect("started event").sequence, 1);
    assert!(events.last().expect("completed event").is_terminal());
}
