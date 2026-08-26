use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zoos_runner_protocol::{
    FakeBehavior, FakeJobRequest, RunnerCapabilities, RunnerEvent, RunnerEventPayload,
};

const EXIT_SUCCESS: i32 = 0;
const EXIT_INVALID_INPUT: i32 = 10;
const EXIT_OUTPUT_WRITE: i32 = 40;
const EXIT_INTERNAL: i32 = 60;

pub fn run_cli(arguments: impl IntoIterator<Item = String>) -> i32 {
    let arguments = arguments.into_iter().collect::<Vec<_>>();

    match arguments.as_slice() {
        [flag, format] if flag == "--capabilities" && format == "--json" => print_capabilities(),
        [command, job_flag, job_path] if command == "run" && job_flag == "--job" => {
            run_job_file(Path::new(job_path), &mut io::stdout().lock())
        }
        [command] if command == "__grandchild-hang" => {
            ignore_termination_signal();
            hang_forever()
        }
        _ => {
            eprintln!("usage: zoos-runner-fake run --job <absolute-path>");
            EXIT_INVALID_INPUT
        }
    }
}

fn print_capabilities() -> i32 {
    println!("{}", serde_json::json!(RunnerCapabilities::fake()));
    EXIT_SUCCESS
}

pub fn run_job_file(job_path: &Path, output: &mut impl Write) -> i32 {
    let request = match read_request(job_path) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("invalid fake job: {error}");
            return EXIT_INVALID_INPUT;
        }
    };

    run_job(&request, output)
}

fn read_request(job_path: &Path) -> Result<FakeJobRequest, Box<dyn std::error::Error>> {
    if !job_path.is_absolute() {
        return Err("job path must be absolute".into());
    }

    let request: FakeJobRequest = serde_json::from_reader(File::open(job_path)?)?;
    request.validate()?;
    Ok(request)
}

pub fn run_job(request: &FakeJobRequest, output: &mut impl Write) -> i32 {
    if let Err(error) = request.validate() {
        eprintln!("invalid fake job: {error}");
        return EXIT_INVALID_INPUT;
    }

    let mut events = EventWriter::new(output, request.job_id.clone());
    if events
        .emit(RunnerEventPayload::Started {
            stage: "fake".into(),
        })
        .is_err()
    {
        return EXIT_INTERNAL;
    }

    match request.test_behavior {
        FakeBehavior::Success => run_success(request, &mut events, EXIT_SUCCESS),
        FakeBehavior::Failed => {
            let _ = events.emit(RunnerEventPayload::Failed {
                error_code: "FAKE_FAILURE".into(),
                message: "Fake runner was asked to fail".into(),
            });
            EXIT_INTERNAL
        }
        FakeBehavior::MalformedNdjson => {
            let _ = writeln!(events.output, "this-is-not-json");
            let _ = events.output.flush();
            EXIT_INTERNAL
        }
        FakeBehavior::Crash => std::process::abort(),
        FakeBehavior::Hang => hang_forever(),
        FakeBehavior::CompletedThenNonzero => run_success(request, &mut events, EXIT_INTERNAL),
        FakeBehavior::SpawnGrandchildAndHang => spawn_grandchild_and_hang(request),
    }
}

fn run_success(
    request: &FakeJobRequest,
    events: &mut EventWriter<'_, impl Write>,
    exit_code: i32,
) -> i32 {
    let started_at = Instant::now();

    for completed in 1..=request.parameters.steps {
        thread::sleep(Duration::from_millis(request.parameters.step_delay_ms));
        if events
            .emit(RunnerEventPayload::Progress {
                stage: "fake".into(),
                completed_units: u64::from(completed),
                total_units: u64::from(request.parameters.steps),
                unit: "step".into(),
                elapsed_ms: started_at.elapsed().as_millis() as u64,
            })
            .is_err()
        {
            return EXIT_INTERNAL;
        }
    }

    if let Err(error) = write_fake_output(&request.output.path, &request.job_id) {
        let _ = events.emit(RunnerEventPayload::Failed {
            error_code: "OUTPUT_WRITE_FAILED".into(),
            message: error.to_string(),
        });
        return EXIT_OUTPUT_WRITE;
    }

    if events
        .emit(RunnerEventPayload::Completed {
            output: request.output.clone(),
        })
        .is_err()
    {
        return EXIT_INTERNAL;
    }
    exit_code
}

fn write_fake_output(path: &Path, job_id: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output path has no parent"))?;
    fs::create_dir_all(parent)?;

    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "output already exists",
        ));
    }

    let partial = parent.join(format!(".{job_id}.partial"));
    let result = (|| {
        let file = File::options()
            .create_new(true)
            .write(true)
            .open(&partial)?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "Zoos Upscale fake output for {job_id}")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(&partial, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(partial);
    }
    result
}

fn spawn_grandchild_and_hang(request: &FakeJobRequest) -> i32 {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("could not resolve fake runner path: {error}");
            return EXIT_INTERNAL;
        }
    };

    match Command::new(executable)
        .arg("__grandchild-hang")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let pid_path = request
                .output
                .path
                .parent()
                .and_then(Path::parent)
                .map(|workspace| workspace.join("grandchild.pid"));
            if let Some(pid_path) = pid_path
                && let Err(error) = fs::write(&pid_path, child.id().to_string())
            {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("could not record fake grandchild pid: {error}");
                return EXIT_INTERNAL;
            }
            eprintln!("spawned fake grandchild pid={}", child.id());
        }
        Err(error) => {
            eprintln!("could not spawn fake grandchild: {error}");
            return EXIT_INTERNAL;
        }
    }

    hang_forever()
}

fn hang_forever() -> i32 {
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(unix)]
fn ignore_termination_signal() {
    // SAFETY: this is a dedicated test-only process. Ignoring SIGTERM intentionally verifies
    // that the host escalates process-group cancellation to SIGKILL after its grace period.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
}

#[cfg(not(unix))]
fn ignore_termination_signal() {}

struct EventWriter<'a, W> {
    output: &'a mut W,
    job_id: String,
    next_sequence: u64,
}

impl<'a, W: Write> EventWriter<'a, W> {
    fn new(output: &'a mut W, job_id: String) -> Self {
        Self {
            output,
            job_id,
            next_sequence: 1,
        }
    }

    fn emit(&mut self, payload: RunnerEventPayload) -> io::Result<()> {
        let event = RunnerEvent::new(self.next_sequence, self.job_id.clone(), payload);
        serde_json::to_writer(&mut *self.output, &event)?;
        writeln!(self.output)?;
        self.output.flush()?;
        self.next_sequence += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use zoos_runner_protocol::{FakeParameters, FakeTask, RunnerInput, RunnerOutput};

    use super::*;

    fn request(root: &Path, behavior: FakeBehavior) -> FakeJobRequest {
        FakeJobRequest {
            protocol_version: 1,
            job_id: "test-job".into(),
            task: FakeTask::Fake,
            input: RunnerInput {
                path: root.join("input.txt"),
            },
            output: RunnerOutput {
                path: root.join("final/result.txt"),
            },
            parameters: FakeParameters {
                steps: 2,
                step_delay_ms: 0,
            },
            test_behavior: behavior,
        }
    }

    #[test]
    fn success_emits_ordered_events_and_writes_output() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let request = request(directory.path(), FakeBehavior::Success);
        let mut output = Vec::new();

        let exit_code = run_job(&request, &mut output);
        let events = String::from_utf8(output).expect("events must be UTF-8");
        let events = events
            .lines()
            .map(|line| serde_json::from_str::<RunnerEvent>(line).expect("valid event"))
            .collect::<Vec<_>>();

        assert_eq!(exit_code, EXIT_SUCCESS);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[3].sequence, 4);
        assert!(events[3].is_terminal());
        assert!(request.output.path.exists());
    }

    #[test]
    fn failed_scenario_has_no_final_output() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let request = request(directory.path(), FakeBehavior::Failed);
        let mut output = Vec::new();

        let exit_code = run_job(&request, &mut output);

        assert_eq!(exit_code, EXIT_INTERNAL);
        assert!(!request.output.path.exists());
        assert!(
            String::from_utf8(output)
                .expect("events must be UTF-8")
                .contains("\"event\":\"failed\"")
        );
    }
}
