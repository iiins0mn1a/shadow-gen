//! Backup and restore `/dev/shm` shared memory files used by Shadow.
//!
//! Shadow and its managed processes communicate through files in `/dev/shm`.
//! CRIU does not save MAP_SHARED file contents by default; it expects the
//! backing files to be present at restore time. This module copies those files
//! to/from a checkpoint directory so that the IPC channels are consistent after
//! a CRIU restore.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Backup all `/dev/shm/shadow_shmemfile_*` files to `backup_dir`.
///
/// The files are copied (not moved) so that the running simulation is not
/// affected when using `--leave-running` during checkpoint.
pub fn backup_shmem_files(shmem_paths: &[PathBuf], backup_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(backup_dir).with_context(|| {
        format!(
            "Failed to create shmem backup directory: {}",
            backup_dir.display()
        )
    })?;

    for src in shmem_paths {
        let filename = src
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("No filename in shmem path: {}", src.display()))?;
        let dst = backup_dir.join(filename);
        std::fs::copy(src, &dst).with_context(|| {
            format!(
                "Failed to backup shmem file {} -> {}",
                src.display(),
                dst.display()
            )
        })?;
        log::info!("Backed up shmem: {} -> {}", src.display(), dst.display());
    }

    Ok(())
}

/// Restore all shmem files from `backup_dir` back to `/dev/shm`.
///
/// Returns the list of restored file paths in `/dev/shm`.
pub fn restore_shmem_files(backup_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut restored = Vec::new();

    let entries = std::fs::read_dir(backup_dir).with_context(|| {
        format!(
            "Failed to read shmem backup directory: {}",
            backup_dir.display()
        )
    })?;

    for entry in entries {
        let entry = entry?;
        let filename = entry.file_name();
        let filename_str = filename.to_string_lossy();
        if !filename_str.starts_with("shadow_shmemfile_") {
            continue;
        }

        let dst = PathBuf::from("/dev/shm").join(&filename);
        std::fs::copy(entry.path(), &dst).with_context(|| {
            format!(
                "Failed to restore shmem file {} -> {}",
                entry.path().display(),
                dst.display()
            )
        })?;
        log::info!(
            "Restored shmem: {} -> {}",
            entry.path().display(),
            dst.display()
        );
        restored.push(dst);
    }

    Ok(restored)
}

/// Collect all `/dev/shm/shadow_shmemfile_*` paths that currently exist.
///
/// This is a fallback when we cannot parse `/proc/pid/maps` (e.g. when the
/// managed processes have already been killed).
pub fn collect_all_shadow_shmem_files() -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let entries = std::fs::read_dir("/dev/shm")?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("shadow_shmemfile_") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}
