//! Safe process APIs: no pre-exec closures or unsafe code in the worker.

use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::process::Stdio;

use bergsmith_protocol::cluster::ResourceLimits;
use rustix::process::{Pid, Signal};
use tokio::process::{Child, Command};

use super::{WorkerError, duration};

#[derive(Default)]
pub(super) struct Escalation {
    deadlines: Option<(tokio::time::Instant, tokio::time::Instant)>,
    term_sent: bool,
    kill_sent: bool,
}

impl Escalation {
    pub(super) fn next_signal(
        &mut self,
        now: tokio::time::Instant,
        grace: Option<prost_types::Duration>,
    ) -> Option<Signal> {
        if self.kill_sent {
            return None;
        }
        let (term, kill) = *self.deadlines.get_or_insert_with(|| {
            let grace = if grace.is_some_and(|grace| grace.seconds == 0 && grace.nanos == 0) {
                std::time::Duration::ZERO
            } else {
                duration(grace).unwrap_or(std::time::Duration::from_secs(3))
            }
            .min(std::time::Duration::from_secs(30));
            (now + grace, now + grace + std::time::Duration::from_secs(2))
        });
        if now >= kill && !self.kill_sent {
            self.kill_sent = true;
            Some(Signal::KILL)
        } else if now >= term && !self.term_sent {
            self.term_sent = true;
            Some(Signal::TERM)
        } else {
            None
        }
    }
}

/// Arms parent-death protection before starting any runtime threads.
///
/// Call only in the task subprocess, immediately after parsing `exec` arguments.
/// On Linux this sets `PR_SET_PDEATHSIG` and checks the spawn/parent-death race.
///
/// # Errors
/// Returns an I/O error if protection cannot be installed or the parent changed.
pub fn prepare_subprocess(parent_pid: u32) -> Result<(), WorkerError> {
    #[cfg(target_os = "linux")]
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))
        .map_err(std::io::Error::from)?;
    if parent_pid == 0
        || u32::try_from(Pid::as_raw(rustix::process::getppid())).ok() != Some(parent_pid)
    {
        return Err(WorkerError::Invalid("task supervisor disappeared"));
    }
    Ok(())
}

/// Moves a single-threaded Linux bootstrap into its prepared cgroup, then
/// replaces its address space before starting the task's runtime or allocator.
///
/// # Errors
/// Returns an I/O error if attachment or re-execution fails. Never falls back
/// to executing outside the configured cgroup.
#[cfg(target_os = "linux")]
pub fn bootstrap(socket: &Path, parent_pid: u32, cgroup: &Path) -> Result<(), WorkerError> {
    use std::os::unix::process::CommandExt;
    prepare_subprocess(parent_pid)?;
    std::fs::write(cgroup.join("cgroup.procs"), std::process::id().to_string())?;
    let error = std::process::Command::new(std::env::current_exe()?)
        .arg("exec")
        .arg("--socket")
        .arg(socket)
        .arg("--parent-pid")
        .arg(parent_pid.to_string())
        .env_clear()
        .exec();
    Err(error.into())
}

/// The creating thread must live as long as the child: Linux PDEATHSIG follows
/// that thread, even when other supervisor threads are still alive.
fn launch(mut command: Command) -> Result<(Child, std::sync::mpsc::Sender<()>), WorkerError> {
    let runtime = tokio::runtime::Handle::current();
    let (ready, child) = std::sync::mpsc::sync_channel(1);
    let (stop, stopped) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("task-launcher".into())
        .spawn(move || {
            let _runtime = runtime.enter();
            if ready.send(command.spawn()).is_ok() {
                let _ = stopped.recv();
            }
        })?;
    let child = child
        .recv()
        .map_err(|_| WorkerError::Invalid("task launcher stopped"))??;
    Ok((child, stop))
}

pub(super) struct Process {
    pub(super) child: Child,
    pid: Pid,
    _launcher: Option<std::sync::mpsc::Sender<()>>,
    #[cfg(target_os = "linux")]
    cgroup: PathBuf,
}

impl Process {
    pub(super) fn spawn(
        binary: &Path,
        socket: &Path,
        limits: &ResourceLimits,
        cgroup_root: Option<&Path>,
    ) -> Result<Self, WorkerError> {
        let mut command = Command::new(binary);
        command
            .arg("exec")
            .arg("--socket")
            .arg(socket)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::null())
            // Drained continuously by `forward_output`, so the child never blocks.
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            // Forwarded lines are already structured; keep them free of escapes.
            .env("NO_COLOR", "1")
            .process_group(0)
            .kill_on_drop(true);
        #[cfg(target_os = "linux")]
        let cgroup = {
            let root = cgroup_root.ok_or(WorkerError::Invalid(
                "linux requires a delegated cgroup v2 root",
            ))?;
            let path = root.join(format!(
                "bergsmith-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7()
            ));
            let setup = || -> Result<(), std::io::Error> {
                std::fs::create_dir(&path)?;
                // Existing controller files prove this is a delegated cgroup,
                // rather than an ordinary directory with ineffective limits.
                for control in [
                    "memory.max",
                    "memory.swap.max",
                    "cpu.max",
                    "pids.max",
                    "cgroup.procs",
                ] {
                    std::fs::metadata(path.join(control))?;
                }
                std::fs::write(path.join("memory.max"), limits.memory_bytes.to_string())?;
                std::fs::write(path.join("memory.swap.max"), "0")?;
                std::fs::write(
                    path.join("cpu.max"),
                    format!("{} 1000000", u64::from(limits.cpu_millis) * 1000),
                )?;
                std::fs::write(path.join("pids.max"), "128")?;
                Ok(())
            };
            if let Err(error) = setup() {
                // Never leak a partially configured cgroup; it holds no task yet.
                let _ = std::fs::remove_dir(&path);
                return Err(error.into());
            }
            command.arg("--bootstrap-cgroup").arg(&path);
            path
        };
        #[cfg(not(target_os = "linux"))]
        let _ = (limits, cgroup_root);
        let (child, launcher) = match launch(command) {
            Ok(launched) => launched,
            Err(error) => {
                #[cfg(target_os = "linux")]
                let _ = std::fs::remove_dir(&cgroup);
                return Err(error);
            }
        };
        let pid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
            .ok_or(WorkerError::Invalid("invalid child process id"))?;
        Ok(Self {
            child,
            pid,
            _launcher: Some(launcher),
            #[cfg(target_os = "linux")]
            cgroup,
        })
    }

    /// Forwards the child's stdout and stderr into the supervisor's tracing,
    /// tagged with the attempt. The task client receives no service token or
    /// catalog access, and never logs its storage credentials.
    pub(super) fn forward_output(&mut self, attempt_id: &str) {
        if let Some(stdout) = self.child.stdout.take() {
            let attempt_id = attempt_id.to_owned();
            tokio::spawn(forward_lines(stdout, move |line| {
                tracing::info!(target: "bergsmith_worker::task", attempt_id = %attempt_id, stream = "stdout", "{line}");
            }));
        }
        if let Some(stderr) = self.child.stderr.take() {
            let attempt_id = attempt_id.to_owned();
            tokio::spawn(forward_lines(stderr, move |line| {
                tracing::warn!(target: "bergsmith_worker::task", attempt_id = %attempt_id, stream = "stderr", "{line}");
            }));
        }
    }

    pub(super) fn signal(&self, signal: Signal) {
        let _ = rustix::process::kill_process_group(self.pid, signal);
    }

    pub(super) fn resource_failure(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string(self.cgroup.join("memory.events")).is_ok_and(|events| {
                events.lines().any(|line| {
                    line.strip_prefix("oom_kill ")
                        .is_some_and(|count| count != "0")
                })
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self.pid;
            false
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Also kill descendants after a task leader exits or supervision is aborted.
        self.signal(Signal::KILL);
        #[cfg(target_os = "linux")]
        {
            // Escaped descendants share the cgroup, not the process group.
            kill_cgroup(&self.cgroup);
            // Removal fails with EBUSY until killed members finish exiting.
            // Retry from a dedicated thread; never block an async worker.
            if std::fs::remove_dir(&self.cgroup).is_err_and(|error| busy(&error)) {
                let path = self.cgroup.clone();
                let cleanup = std::thread::Builder::new()
                    .name("cgroup-cleanup".into())
                    .spawn(move || {
                        if let Err(error) = remove_cgroup(&path, 50) {
                            tracing::warn!(cgroup = %path.display(), error_code = error.raw_os_error(), "task cgroup could not be removed");
                        }
                    });
                if cleanup.is_err() {
                    tracing::warn!(cgroup = %self.cgroup.display(), "task cgroup cleanup could not start");
                }
            }
        }
    }
}

/// Reads `reader` to its end, passing each line (without its terminator) to
/// `sink`. Lines are cut at `MAX_LINE` bytes, so memory stays bounded even for
/// output that never contains a newline.
async fn forward_lines(reader: impl tokio::io::AsyncRead + Unpin, mut sink: impl FnMut(&str)) {
    use tokio::io::AsyncBufReadExt;
    const MAX_LINE: usize = 8 * 1024;
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = Vec::with_capacity(256);
    loop {
        let Ok(chunk) = reader.fill_buf().await else {
            break;
        };
        if chunk.is_empty() {
            break;
        }
        let room = MAX_LINE - line.len();
        let newline = chunk.iter().take(room).position(|byte| *byte == b'\n');
        let taken = newline.map_or(chunk.len().min(room), |index| index + 1);
        line.extend_from_slice(&chunk[..newline.unwrap_or(taken)]);
        reader.consume(taken);
        if newline.is_some() || line.len() >= MAX_LINE {
            sink(String::from_utf8_lossy(&line).trim_end_matches('\r'));
            line.clear();
        }
    }
    if !line.is_empty() {
        sink(String::from_utf8_lossy(&line).trim_end_matches('\r'));
    }
}

#[cfg(target_os = "linux")]
fn busy(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::BUSY.raw_os_error())
}

/// Kills every member of a task cgroup (cgroup v2 `cgroup.kill`, Linux 5.14+).
/// Opening without `create` leaves an ordinary directory untouched.
#[cfg(target_os = "linux")]
fn kill_cgroup(path: &Path) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .open(path.join("cgroup.kill"))
    {
        let _ = file.write_all(b"1");
    }
}

/// Removes an owned task cgroup, retrying while killed members finish exiting.
/// Blocking: call only from a dedicated or blocking thread.
#[cfg(target_os = "linux")]
fn remove_cgroup(path: &Path, attempts: u32) -> std::io::Result<()> {
    let mut delay = std::time::Duration::from_millis(5);
    for attempt in 1..=attempts {
        match std::fs::remove_dir(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if busy(&error) && attempt < attempts => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(500));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Whether a process with this PID exists in the worker's PID namespace.
#[cfg(target_os = "linux")]
pub(super) fn process_alive(pid: i32) -> bool {
    Pid::from_raw(pid).is_some_and(|pid| {
        !matches!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        )
    })
}

/// Removes empty `bergsmith-<pid>-<uuid>` cgroups left by an earlier run.
///
/// Called at supervisor startup, before any task exists, so a cgroup named
/// with this process's PID (a reused PID) or a PID that no longer exists is
/// stale. Removal is a plain `rmdir`: the kernel refuses a populated cgroup,
/// so a cgroup that still contains processes is kept and logged, never
/// killed. Returns how many cgroups were removed.
///
/// # Errors
/// Returns an I/O error if the root cannot be listed.
#[cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(dead_code, reason = "cgroup sweeping runs only on linux")
)]
pub(super) fn sweep_stale_cgroups(
    root: &Path,
    alive: impl Fn(i32) -> bool,
) -> std::io::Result<usize> {
    let own = i32::try_from(std::process::id()).ok();
    let mut removed = 0;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix("bergsmith-"))
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<i32>().ok())
        else {
            continue;
        };
        if Some(pid) != own && alive(pid) {
            continue;
        }
        match std::fs::remove_dir(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(
                cgroup = %entry.path().display(),
                error_code = error.raw_os_error(),
                "stale task cgroup could not be removed"
            ),
        }
    }
    Ok(removed)
}

/// The task's `RLIMIT_CPU` in seconds: a backstop that must never fire before
/// the wall-clock duration limit.
///
/// `RLIMIT_CPU` counts CPU time summed over all threads. Linux throttles the
/// cgroup to `cpu_millis`, so CPU time cannot exceed `cpu_millis × duration`.
/// Other platforms cannot throttle, so a multi-threaded task may use every
/// available core; the bound then allows `parallelism × duration`.
#[must_use]
fn cpu_time_limit(max_duration: std::time::Duration, cpu_millis: u32, parallelism: u64) -> u64 {
    let seconds = max_duration.as_secs().saturating_add(1);
    let cores_millis = if cfg!(target_os = "linux") {
        u64::from(cpu_millis)
    } else {
        u64::from(cpu_millis).max(parallelism.saturating_mul(1000))
    };
    seconds.saturating_mul(cores_millis).div_ceil(1000).max(1)
}

pub(super) fn apply_limits(limits: &ResourceLimits) -> Result<(), WorkerError> {
    let parallelism = std::thread::available_parallelism()
        .map_or(1, |cores| u64::try_from(cores.get()).unwrap_or(u64::MAX));
    let cpu_seconds = cpu_time_limit(
        duration(limits.max_duration)?,
        limits.cpu_millis,
        parallelism,
    );
    let set = |resource, value| {
        rustix::process::setrlimit(
            resource,
            rustix::process::Rlimit {
                current: Some(value),
                maximum: Some(value),
            },
        )
        .map_err(|error| {
            tracing::error!(
                ?resource,
                error_code = error.raw_os_error(),
                "task resource limit could not be applied"
            );
            WorkerError::Invalid("task resource limit rejected")
        })
    };
    rustix::process::setrlimit(
        rustix::process::Resource::Cpu,
        rustix::process::Rlimit {
            current: Some(cpu_seconds),
            maximum: Some(cpu_seconds.saturating_add(1)),
        },
    )
    .map_err(|error| {
        tracing::error!(
            error_code = error.raw_os_error(),
            "task cpu limit could not be applied"
        );
        WorkerError::Invalid("task cpu limit rejected")
    })?;
    set(rustix::process::Resource::Core, 0)?;
    #[cfg(not(target_os = "linux"))]
    {
        // Darwin reserves large allocator arenas before this RPC. Its VM data
        // limit cannot be lowered beneath those reservations. Bound additional
        // growth here; the independent supervisor samples actual resident bytes.
        let mut system = sysinfo::System::new();
        let pid = sysinfo::Pid::from_u32(std::process::id());
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            sysinfo::ProcessRefreshKind::nothing()
                .with_memory()
                .without_tasks(),
        );
        let baseline = system
            .process(pid)
            .ok_or(WorkerError::Invalid("task memory usage unavailable"))?
            .virtual_memory();
        let bound = baseline
            .checked_add(limits.memory_bytes)
            .ok_or(WorkerError::Invalid("memory limit overflow"))?;
        set(rustix::process::Resource::Data, bound)?;
    }
    Ok(())
}

/// Samples only the task PID, without command lines or environment variables.
/// The supervisor runs this blocking platform query outside the async runtime.
#[cfg(not(target_os = "linux"))]
pub(super) fn resident_memory(pid: u32) -> Result<u64, WorkerError> {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::Pid::from_u32(pid);
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing()
            .with_memory()
            .without_tasks(),
    );
    system
        .process(pid)
        .map(sysinfo::Process::memory)
        .ok_or(WorkerError::Invalid("task memory usage unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[test]
    fn parent_death_probe() {
        use std::io::{BufRead, Write};
        let Ok(parent) = std::env::var("BERGSMITH_PARENT_DEATH_PROBE") else {
            return;
        };
        prepare_subprocess(parent.parse().unwrap()).unwrap();
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().lock().read_line(&mut input).unwrap();
        assert_eq!(input, "probe\n");
        println!("parent remains alive");
        std::io::stdout().flush().unwrap();
    }

    #[tokio::test]
    async fn launcher_survives_the_thread_that_requested_the_child() {
        use tokio::io::AsyncWriteExt;
        let runtime = tokio::runtime::Handle::current();
        let caller = std::thread::spawn(move || {
            let _runtime = runtime.enter();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "supervision::process::tests::parent_death_probe",
                    "--nocapture",
                ])
                .env(
                    "BERGSMITH_PARENT_DEATH_PROBE",
                    std::process::id().to_string(),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true);
            launch(command).unwrap()
        });
        let (mut child, _launcher) = caller.join().unwrap();
        // The requesting thread has exited before the child arms PDEATHSIG.
        // Its actual creating thread must still be alive throughout the probe.
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"probe\n")
            .await
            .unwrap();
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("parent remains alive"));
    }

    #[test]
    fn cancellation_honors_grace_and_sends_each_escalation_once() {
        use std::time::Duration;
        let mut escalation = Escalation::default();
        let now = tokio::time::Instant::now();
        let grace = Some(prost_types::Duration {
            seconds: 3,
            nanos: 0,
        });
        assert_eq!(escalation.next_signal(now, grace), None);
        assert_eq!(
            escalation.next_signal(now + Duration::from_secs(2), grace),
            None
        );
        assert_eq!(
            escalation.next_signal(now + Duration::from_secs(3), grace),
            Some(Signal::TERM)
        );
        assert_eq!(
            escalation.next_signal(now + Duration::from_secs(4), grace),
            None
        );
        assert_eq!(
            escalation.next_signal(now + Duration::from_secs(5), grace),
            Some(Signal::KILL)
        );
        assert_eq!(
            escalation.next_signal(now + Duration::from_secs(6), grace),
            None
        );
        let mut immediate = Escalation::default();
        assert_eq!(
            immediate.next_signal(now, Some(prost_types::Duration::default())),
            Some(Signal::TERM)
        );
    }

    #[tokio::test]
    async fn a_task_ignoring_sigterm_is_killed_as_a_process_group() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; printf 'ready\\n'; while :; do :; done"])
            .stdout(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let pid = Pid::from_raw(i32::try_from(child.id().unwrap()).unwrap()).unwrap();
        let mut process = Process {
            child,
            pid,
            _launcher: None,
            #[cfg(target_os = "linux")]
            cgroup: PathBuf::new(),
        };
        let mut reader = BufReader::new(stdout);
        let mut ready = String::new();
        reader.read_line(&mut ready).await.unwrap();
        assert_eq!(ready, "ready\n");
        process.signal(Signal::TERM);
        process.signal(Signal::KILL);
        let status = tokio::time::timeout(std::time::Duration::from_secs(3), process.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_ordinary_directory_cannot_substitute_for_a_delegated_cgroup() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("task");
        std::fs::write(&script, "#!/bin/sh\nexec /bin/sleep 600\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let limits = ResourceLimits {
            memory_bytes: 1024,
            cpu_millis: 1000,
            ..Default::default()
        };
        assert!(
            Process::spawn(
                &script,
                &root.path().join("socket"),
                &limits,
                Some(root.path())
            )
            .is_err()
        );
        // The partially configured cgroup directory is removed, not leaked.
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("bergsmith-")
        }));
    }

    #[test]
    fn startup_sweep_removes_only_empty_cgroups_of_dead_or_reused_pids() {
        let root = tempfile::tempdir().unwrap();
        let own = std::process::id();
        let dead = root.path().join("bergsmith-999999-a");
        let reused = root.path().join(format!("bergsmith-{own}-b"));
        let live = root.path().join("bergsmith-1-c");
        let populated = root.path().join("bergsmith-999998-d");
        let unrelated = root.path().join("other-999999-e");
        for directory in [&dead, &reused, &live, &populated, &unrelated] {
            std::fs::create_dir(directory).unwrap();
        }
        // A non-empty directory stands in for a cgroup that still has members.
        std::fs::write(populated.join("member"), "").unwrap();
        let removed = sweep_stale_cgroups(root.path(), |pid| pid == 1).unwrap();
        assert_eq!(removed, 2);
        assert!(!dead.exists() && !reused.exists());
        assert!(live.exists() && populated.exists() && unrelated.exists());
    }

    #[test]
    fn cpu_rlimit_never_fires_before_the_duration_limit() {
        let duration = std::time::Duration::from_secs(30);
        if cfg!(target_os = "linux") {
            // The cgroup throttles CPU to cpu_millis, bounding total CPU time.
            assert_eq!(cpu_time_limit(duration, 1000, 8), 31);
            assert_eq!(cpu_time_limit(duration, 500, 8), 16);
        } else {
            // Unthrottled threads may use every core for the whole duration.
            assert_eq!(cpu_time_limit(duration, 1000, 8), 248);
            assert_eq!(cpu_time_limit(duration, 16_000, 8), 496);
        }
        assert_eq!(cpu_time_limit(std::time::Duration::ZERO, 1, 1), 1);
    }

    #[tokio::test]
    async fn child_output_is_forwarded_line_by_line_with_bounded_lines() {
        let long = "x".repeat(8 * 1024 + 10);
        let input = format!("first\r\nsecond\n{long}\ntail");
        let mut lines = Vec::new();
        forward_lines(input.as_bytes(), |line| lines.push(line.to_owned())).await;
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "first");
        assert_eq!(lines[1], "second");
        assert_eq!(lines[2].len(), 8 * 1024);
        assert_eq!(lines[3], "x".repeat(10));
        assert_eq!(lines[4], "tail");
    }
}
