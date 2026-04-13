//! Checkpoint storage backends.
//!
//! Checkpoints can be saved to and loaded from the filesystem or kept in
//! memory for fast restore during state-space exploration.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;

use super::snapshot_types::SimulationCheckpoint;

/// Trait for checkpoint persistence backends.
pub trait CheckpointStore: Send + Sync {
    fn save(&self, label: &str, checkpoint: &SimulationCheckpoint) -> anyhow::Result<()>;
    fn load(&self, label: &str) -> anyhow::Result<SimulationCheckpoint>;
    fn list(&self) -> anyhow::Result<Vec<String>>;
    fn delete(&self, label: &str) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Filesystem store
// ---------------------------------------------------------------------------

pub struct FilesystemStore {
    base_dir: PathBuf,
}

impl FilesystemStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let base_dir = base_dir.into();
        std::fs::create_dir_all(&base_dir).with_context(|| {
            format!(
                "Failed to create checkpoint directory: {}",
                base_dir.display()
            )
        })?;
        Ok(Self { base_dir })
    }

    fn path_for(&self, label: &str) -> PathBuf {
        self.base_dir.join(format!("{label}.checkpoint.json"))
    }
}

impl CheckpointStore for FilesystemStore {
    fn save(&self, label: &str, checkpoint: &SimulationCheckpoint) -> anyhow::Result<()> {
        let path = self.path_for(label);
        let file = std::fs::File::create(&path)
            .with_context(|| format!("Failed to create checkpoint file: {}", path.display()))?;
        serde_json::to_writer_pretty(file, checkpoint)
            .with_context(|| format!("Failed to serialize checkpoint to: {}", path.display()))?;
        log::info!("Checkpoint '{}' saved to {}", label, path.display());
        Ok(())
    }

    fn load(&self, label: &str) -> anyhow::Result<SimulationCheckpoint> {
        let path = self.path_for(label);
        let file = std::fs::File::open(&path)
            .with_context(|| format!("Checkpoint file not found: {}", path.display()))?;
        let checkpoint: SimulationCheckpoint =
            serde_json::from_reader(file).with_context(|| {
                format!("Failed to deserialize checkpoint from: {}", path.display())
            })?;
        log::info!("Checkpoint '{}' loaded from {}", label, path.display());
        Ok(checkpoint)
    }

    fn list(&self) -> anyhow::Result<Vec<String>> {
        let mut labels = Vec::new();
        for entry in std::fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Some(label) = name.strip_suffix(".checkpoint.json") {
                    labels.push(label.to_string());
                }
            }
        }
        labels.sort();
        Ok(labels)
    }

    fn delete(&self, label: &str) -> anyhow::Result<()> {
        let path = self.path_for(label);
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to delete checkpoint: {}", path.display()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// In-memory store (for fast state-space exploration)
// ---------------------------------------------------------------------------

pub struct InMemoryStore {
    checkpoints: std::sync::Mutex<HashMap<String, SimulationCheckpoint>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            checkpoints: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl CheckpointStore for InMemoryStore {
    fn save(&self, label: &str, checkpoint: &SimulationCheckpoint) -> anyhow::Result<()> {
        self.checkpoints
            .lock()
            .unwrap()
            .insert(label.to_string(), checkpoint.clone());
        log::info!("Checkpoint '{}' saved in memory", label);
        Ok(())
    }

    fn load(&self, label: &str) -> anyhow::Result<SimulationCheckpoint> {
        self.checkpoints
            .lock()
            .unwrap()
            .get(label)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Checkpoint '{}' not found in memory", label))
    }

    fn list(&self) -> anyhow::Result<Vec<String>> {
        let mut labels: Vec<_> = self.checkpoints.lock().unwrap().keys().cloned().collect();
        labels.sort();
        Ok(labels)
    }

    fn delete(&self, label: &str) -> anyhow::Result<()> {
        self.checkpoints.lock().unwrap().remove(label);
        Ok(())
    }
}
