//! CRIU (Checkpoint/Restore In Userspace) integration for Shadow.
//!
//! This module provides functions to checkpoint and restore the real OS
//! processes that Shadow manages via fork+exec. It shells out to the `criu`
//! command-line tool.
//!
//! # Prerequisites
//!
//! - `criu` must be installed and available in `$PATH` (or set via `CRIU_BIN`).
//! - The `criu` binary must have `CAP_CHECKPOINT_RESTORE`, `CAP_SYS_ADMIN`,
//!   and `CAP_SYS_PTRACE` capabilities (or equivalent).
//!
//! # Limitations
//!
//! - After `criu restore`, the restored process gets a **new PID**. The
//!   caller must update all internal PID references and re-establish
//!   shared-memory IPC channels.
//! - File descriptors pointing to Shadow's shared-memory segments need
//!   special handling (see `restore_process`).

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

fn criu_bin_required() -> anyhow::Result<String> {
    std::env::var("CRIU_BIN").with_context(|| {
        "CRIU_BIN is not set. This build requires an explicit CRIU binary path. \
Set CRIU_BIN=/path/to/criu (see criu-demo/run.sh)."
    })
}

fn format_cmd_for_logs(cmd: &Command) -> String {
    let prog = cmd.get_program().to_string_lossy();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    format!("{prog} {}", args.join(" "))
}

fn criu_network_lock_method() -> Option<String> {
    std::env::var("CRIU_NETWORK_LOCK")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Check whether CRIU is available on this system.
pub fn criu_available() -> bool {
    let Ok(criu_bin) = criu_bin_required() else {
        return false;
    };
    Command::new(criu_bin)
        .arg("check")
        .arg("--unprivileged")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Checkpoint (dump) a single process tree rooted at `pid`.
///
/// The CRIU images are written to `images_dir`. The process is frozen with
/// `SIGSTOP` before dumping and resumed with `SIGCONT` afterwards (unless
/// `leave_running` is false, in which case CRIU kills the process after
/// dumping -- useful when we plan to restore from the image later).
///
/// # Arguments
///
/// * `pid` - The root PID of the process tree to checkpoint.
/// * `images_dir` - Directory where CRIU writes its image files.
pub fn checkpoint_process(pid: i32, images_dir: &Path, leave_running: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(images_dir).with_context(|| {
        format!(
            "Failed to create CRIU images directory: {}",
            images_dir.display()
        )
    })?;

    let criu_bin = criu_bin_required()?;
    let dump_log = images_dir.join("criu-dump.log");

    let mut cmd = Command::new(criu_bin);
    cmd.arg("dump")
        .arg("--unprivileged")
        .arg("--shell-job")
        .arg("--tcp-established")
        .arg("--tree")
        .arg(pid.to_string())
        .arg("--images-dir")
        .arg(images_dir)
        .arg("--log-file")
        .arg(&dump_log);
    let network_lock_method = criu_network_lock_method();
    if let Some(method) = network_lock_method.as_ref() {
        cmd.arg("--network-lock").arg(method);
    }

    // Shadow sometimes needs to continue running after checkpoint, so this flag
    // is intentionally retained even though criu-demo doesn't use it.
    if leave_running {
        cmd.arg("--leave-running");
    }

    log::info!(
        "CRIU dump (aligned): pid={}, images_dir={}, leave_running={}, network_lock={:?}, cmd={}",
        pid,
        images_dir.display(),
        leave_running,
        network_lock_method,
        format_cmd_for_logs(&cmd),
    );

    let output = cmd
        .output()
        .context("Failed to execute `criu dump` command")?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "criu dump failed for pid {pid}: exit={}\ncmd: {}\nlog_file: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            format_cmd_for_logs(&cmd),
            dump_log.display(),
            stdout,
            stderr
        );
    }

    log::info!("CRIU dump succeeded for pid {}", pid);
    Ok(())
}

/// Restore a previously checkpointed process from CRIU images.
///
/// Returns the new PID of the restored process. The caller is responsible
/// for updating internal PID mappings and re-establishing IPC channels.
///
/// # Arguments
///
/// * `images_dir` - Directory containing the CRIU image files.
/// * `pidfile` - Optional path where CRIU will write the new root PID.
pub fn restore_process(images_dir: &Path, pidfile: Option<&Path>) -> anyhow::Result<i32> {
    let criu_bin = criu_bin_required()?;

    let pidfile_path = pidfile.map(PathBuf::from).unwrap_or_else(|| {
        // Align with criu-demo behavior: keep pidfile outside the images
        // directory to avoid conflicts between attempts/runs.
        let dir_name = images_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("criu_images");
        std::env::temp_dir().join(format!(
            "shadow-criu-restore-{}-{}.pid",
            std::process::id(),
            dir_name
        ))
    });

    // CRIU may treat an existing pidfile as an error in some versions
    // (demo explicitly removes the pidfile before restore).
    let _ = std::fs::remove_file(&pidfile_path);

    let restore_log = images_dir.join("criu-restore.log");

    let mut cmd = Command::new(criu_bin);
    cmd.arg("restore")
        .arg("--unprivileged")
        .arg("--tcp-established")
        .arg("--leave-stopped")
        .arg("--restore-detached")
        .arg("--restore-sibling")
        .arg("--images-dir")
        .arg(images_dir)
        .arg("--log-file")
        .arg(&restore_log)
        .arg("--pidfile")
        .arg(&pidfile_path);
    let network_lock_method = criu_network_lock_method();
    if let Some(method) = network_lock_method.as_ref() {
        cmd.arg("--network-lock").arg(method);
    }

    log::info!(
        "CRIU restore (aligned): images_dir={}, pidfile={}, network_lock={:?}, cmd={}",
        images_dir.display(),
        pidfile_path.display(),
        network_lock_method,
        format_cmd_for_logs(&cmd),
    );

    let output = cmd
        .output()
        .context("Failed to execute `criu restore` command")?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "criu restore failed: exit={}\ncmd: {}\nlog_file: {}\npidfile: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            format_cmd_for_logs(&cmd),
            restore_log.display(),
            pidfile_path.display(),
            stdout,
            stderr
        );
    }

    let pid_str = std::fs::read_to_string(&pidfile_path)
        .with_context(|| format!("Failed to read CRIU pidfile: {}", pidfile_path.display()))?;
    let new_pid: i32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("Failed to parse PID from pidfile: '{pid_str}'"))?;

    log::info!("CRIU restore succeeded: new pid={}", new_pid);
    Ok(new_pid)
}

/// Convenience: checkpoint all managed processes.
///
/// For each `(native_pid, host_id, process_id)` tuple, creates a subdirectory
/// under `base_dir` and calls [`checkpoint_process`].
///
/// Returns a list of `(host_id, process_id, images_dir)`.
pub fn checkpoint_all_processes(
    processes: &[(i32, u32, u32)],
    base_dir: &Path,
    leave_running: bool,
) -> anyhow::Result<Vec<(u32, u32, PathBuf)>> {
    let mut results = Vec::new();
    for &(pid, host_id, process_id) in processes {
        let dir = base_dir.join(format!("host_{host_id}_proc_{process_id}"));
        checkpoint_process(pid, &dir, leave_running)?;
        results.push((host_id, process_id, dir));
    }
    Ok(results)
}

/// Convenience: restore all managed processes from their image directories.
///
/// Returns a list of `(host_id, process_id, new_pid)`.
pub fn restore_all_processes(
    images: &[(u32, u32, PathBuf)],
) -> anyhow::Result<Vec<(u32, u32, i32)>> {
    let mut results = Vec::new();
    for (host_id, process_id, dir) in images {
        let new_pid = restore_process(dir, None)?;
        results.push((*host_id, *process_id, new_pid));
    }
    Ok(results)
}

/// Collect `/dev/shm` file paths that a process has mapped via MAP_SHARED.
///
/// Parses `/proc/<pid>/maps` looking for mappings to `/dev/shm/shadow_shmemfile_*`.
/// Returns deduplicated paths.
pub fn collect_shmem_paths_for_pid(pid: i32) -> anyhow::Result<Vec<PathBuf>> {
    let maps_path = format!("/proc/{pid}/maps");
    let file =
        std::fs::File::open(&maps_path).with_context(|| format!("Failed to open {maps_path}"))?;
    let reader = std::io::BufReader::new(file);

    let mut paths = std::collections::BTreeSet::new();
    for line in reader.lines() {
        let line = line?;
        if let Some(idx) = line.find("/dev/shm/shadow_shmemfile_") {
            let path_str = line[idx..].trim();
            paths.insert(PathBuf::from(path_str));
        }
    }
    Ok(paths.into_iter().collect())
}

/// Collect all `/dev/shm/shadow_shmemfile_*` paths mapped by Shadow itself.
pub fn collect_shadow_shmem_paths() -> anyhow::Result<Vec<PathBuf>> {
    let pid = std::process::id() as i32;
    collect_shmem_paths_for_pid(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_criu_available_does_not_panic() {
        let _ = criu_available();
    }

    // CRIU_BIN is required by this build; no default-path test.

    #[test]
    fn test_collect_shadow_shmem_paths_does_not_panic() {
        let _ = collect_shadow_shmem_paths();
    }
}
