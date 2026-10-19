//! Hold a real native task's metadata read without adding production test hooks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

use super::{DEADLINE, Harness};

pub(super) struct NativeGate {
    path: PathBuf,
    backup: PathBuf,
}

impl NativeGate {
    pub(super) fn new(location: &str) -> Self {
        let path = url::Url::parse(location).unwrap().to_file_path().unwrap();
        let backup = path.with_extension("held.json");
        std::fs::rename(&path, &backup).unwrap();
        let gate = Self { path, backup };
        assert!(
            std::process::Command::new("mkfifo")
                .args(["-m", "600"])
                .arg(&gate.path)
                .status()
                .unwrap()
                .success()
        );
        gate
    }

    pub(super) async fn reader(&self, run: &mut Harness) -> tokio::net::unix::pipe::Sender {
        // A nonblocking writer opens only once a native reader has opened the
        // FIFO. Keep it open without writing so the native read stays pending.
        let result = tokio::time::timeout(DEADLINE, async {
            let mut observation = tokio::time::interval(Duration::from_millis(20));
            loop {
                observation.tick().await;
                run.assert_alive();
                match tokio::net::unix::pipe::OpenOptions::new().open_sender(&self.path) {
                    Ok(sender) => break sender,
                    Err(error)
                        if error.raw_os_error() == Some(rustix::io::Errno::NXIO.raw_os_error()) => {
                    }
                    Err(error) => panic!("native metadata gate failed: {error}"),
                }
            }
        })
        .await;
        result.expect("native subprocess must reach its pinned metadata read")
    }

    pub(super) fn restore(&self) {
        std::fs::rename(&self.backup, &self.path).unwrap();
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) async fn release_reader(&self, mut writer: tokio::net::unix::pipe::Sender) {
        use tokio::io::AsyncWriteExt;
        // Development platforms have no PDEATHSIG. Finish this deliberately
        // blocked synchronous filesystem read so the task can observe the
        // vanished supervisor through its control stream and exit.
        let _ = writer
            .write_all(&std::fs::read(&self.backup).unwrap())
            .await;
    }
}

impl Drop for NativeGate {
    fn drop(&mut self) {
        if self.backup.exists() {
            // Also release an already-open or opening reader if an assertion
            // unwinds before the normal crash sequence. RDWR never waits for
            // a peer; retaining it across rename closes the path-entry race.
            let _wake = rustix::fs::open(
                &self.path,
                rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            );
            let _ = std::fs::rename(&self.backup, &self.path);
        }
    }
}

pub(super) async fn child(parent: u32) -> Pid {
    tokio::task::spawn_blocking(move || {
        let mut system = System::new();
        // Linux exposes threads as tasks with their process as parent. Count
        // actual subprocesses only, using the worker memory sampler pattern.
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
        let children: Vec<_> = system
            .processes()
            .values()
            .filter(|process| process.parent() == Some(Pid::from_u32(parent)))
            .map(sysinfo::Process::pid)
            .collect();
        assert_eq!(children.len(), 1, "one real native task must be running");
        children[0]
    })
    .await
    .unwrap()
}

pub(super) async fn exited(pid: Pid, directory: &Path) {
    let result = tokio::time::timeout(DEADLINE, async {
        let mut observation = tokio::time::interval(Duration::from_millis(20));
        loop {
            observation.tick().await;
            let alive = tokio::task::spawn_blocking(move || {
                let mut system = System::new();
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing().without_tasks(),
                );
                system
                    .process(pid)
                    .is_some_and(|process| process.status() != ProcessStatus::Zombie)
            })
            .await
            .unwrap();
            if !alive {
                break;
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "native task survived its supervisor; logs: {}",
        directory.display()
    );
}
