use std::path::Path;
use std::time::Duration;

use zoos_core::{FakeBehavior, JobOrchestrator, JobStatus, JobSummary};

fn orchestrator(root: &Path, activity_timeout: Duration) -> JobOrchestrator {
    JobOrchestrator::new(
        root,
        env!("CARGO_BIN_EXE_zoos-runner-fake-bin").into(),
        activity_timeout,
        Duration::from_millis(100),
    )
    .expect("orchestrator must be created")
}

async fn run_scenario(
    behavior: FakeBehavior,
    activity_timeout: Duration,
) -> (tempfile::TempDir, JobOrchestrator, JobSummary) {
    let directory = tempfile::tempdir().expect("temporary directory must be created");
    let orchestrator = orchestrator(directory.path(), activity_timeout);
    let created = orchestrator
        .create_fake_job(behavior)
        .expect("job must be created");
    orchestrator
        .start_job(&created.job_id)
        .await
        .expect("job must start");
    let terminal = wait_for_terminal(&orchestrator, &created.job_id).await;
    (directory, orchestrator, terminal)
}

async fn wait_for_terminal(orchestrator: &JobOrchestrator, job_id: &str) -> JobSummary {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let job = orchestrator
                .list_jobs()
                .expect("jobs must load")
                .into_iter()
                .find(|job| job.job_id == job_id)
                .expect("job must exist");
            if job.status.is_terminal() {
                break job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("job must reach a terminal state")
}

#[tokio::test]
async fn success_reaches_completed_after_output_verification() {
    let (directory, _, job) = run_scenario(FakeBehavior::Success, Duration::from_secs(1)).await;

    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.progress_percent, 100);
    assert!(job.error.is_none());
    assert!(
        directory
            .path()
            .join(&job.job_id)
            .join("final/result.txt")
            .is_file()
    );
}

#[tokio::test]
async fn explicit_failure_is_structured_and_has_no_output() {
    let (directory, _, job) = run_scenario(FakeBehavior::Failed, Duration::from_secs(1)).await;

    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.error.expect("error must exist").code, "FAKE_FAILURE");
    assert!(
        !directory
            .path()
            .join(&job.job_id)
            .join("final/result.txt")
            .exists()
    );
}

#[tokio::test]
async fn malformed_ndjson_is_a_protocol_failure() {
    let (_, _, job) = run_scenario(FakeBehavior::MalformedNdjson, Duration::from_secs(1)).await;

    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(
        job.error.expect("error must exist").code,
        "PROTOCOL_VIOLATION"
    );
}

#[tokio::test]
async fn abnormal_process_exit_is_a_runner_crash() {
    let (_, _, job) = run_scenario(FakeBehavior::Crash, Duration::from_secs(1)).await;

    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.error.expect("error must exist").code, "RUNNER_CRASHED");
}

#[tokio::test]
async fn hang_is_terminated_after_activity_timeout() {
    let (_, _, job) = run_scenario(FakeBehavior::Hang, Duration::from_millis(150)).await;

    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(
        job.error.expect("error must exist").code,
        "RUNNER_TIMED_OUT"
    );
}

#[tokio::test]
async fn completed_event_with_nonzero_exit_is_rejected_and_cleaned() {
    let (directory, _, job) =
        run_scenario(FakeBehavior::CompletedThenNonzero, Duration::from_secs(1)).await;

    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(
        job.error.expect("error must exist").code,
        "PROTOCOL_VIOLATION"
    );
    assert!(
        !directory
            .path()
            .join(&job.job_id)
            .join("final/result.txt")
            .exists()
    );
}

#[tokio::test]
async fn user_cancel_reaches_cancelled() {
    let directory = tempfile::tempdir().expect("temporary directory must be created");
    let orchestrator = orchestrator(directory.path(), Duration::from_secs(5));
    let job = orchestrator
        .create_fake_job(FakeBehavior::Hang)
        .expect("job must be created");
    orchestrator
        .start_job(&job.job_id)
        .await
        .expect("job must start");

    orchestrator
        .cancel_job(&job.job_id)
        .await
        .expect("job must accept cancellation");
    let terminal = wait_for_terminal(&orchestrator, &job.job_id).await;

    assert_eq!(terminal.status, JobStatus::Cancelled);
    assert!(terminal.error.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_a_sigterm_ignoring_grandchild() {
    let directory = tempfile::tempdir().expect("temporary directory must be created");
    let orchestrator = orchestrator(directory.path(), Duration::from_secs(5));
    let job = orchestrator
        .create_fake_job(FakeBehavior::SpawnGrandchildAndHang)
        .expect("job must be created");
    orchestrator
        .start_job(&job.job_id)
        .await
        .expect("job must start");

    let pid_path = directory.path().join(&job.job_id).join("grandchild.pid");
    let grandchild_pid = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_path) {
                break pid.parse::<i32>().expect("pid must be numeric");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("grandchild pid must be recorded");

    orchestrator
        .cancel_job(&job.job_id)
        .await
        .expect("job must accept cancellation");
    let terminal = wait_for_terminal(&orchestrator, &job.job_id).await;
    assert_eq!(terminal.status, JobStatus::Cancelled);

    tokio::time::timeout(Duration::from_secs(2), async {
        while process_exists(grandchild_pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("grandchild must not survive cancellation");
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 does not deliver a signal; it only checks whether the pid exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
