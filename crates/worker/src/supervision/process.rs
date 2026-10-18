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
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear()
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
                return Err(error.into());
            }
            command.arg("--bootstrap-cgroup").arg(&path);
            path
        };
        #[cfg(not(target_os = "linux"))]
        let _ = (limits, cgroup_root);
        let (child, launcher) = launch(command)?;
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
            let _ = std::fs::remove_dir(&self.cgroup);
        }
    }
}

pub(super) fn apply_limits(limits: &ResourceLimits) -> Result<(), WorkerError> {
    let seconds = duration(limits.max_duration)?.as_secs().saturating_add(1);
    let cpu_seconds = seconds
        .saturating_mul(u64::from(limits.cpu_millis))
        .div_ceil(1000)
        .max(1);
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
    }
}
