use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartupProgress {
    pub pid: u32,
    pub seq: u64,
    pub phase: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

#[derive(Debug)]
pub struct StartupReporter {
    path: PathBuf,
    seq: u64,
}

impl StartupReporter {
    pub fn new(tag: &str) -> Self {
        Self {
            path: smfs_core::daemon::startup_path(tag),
            seq: 0,
        }
    }

    pub fn report(&mut self, phase: &str, message: impl Into<String>) -> Result<()> {
        self.write(phase, message.into(), None, None)
    }

    pub fn report_counts(
        &mut self,
        phase: &str,
        message: impl Into<String>,
        loaded: usize,
        total: usize,
    ) -> Result<()> {
        self.write(phase, message.into(), Some(loaded), Some(total))
    }

    fn write(
        &mut self,
        phase: &str,
        message: String,
        loaded: Option<usize>,
        total: Option<usize>,
    ) -> Result<()> {
        self.seq += 1;
        let progress = StartupProgress {
            pid: std::process::id(),
            seq: self.seq,
            phase: phase.to_string(),
            message,
            loaded,
            total,
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&progress)?)?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

pub fn read_progress(tag: &str) -> Option<(String, StartupProgress)> {
    let body = std::fs::read_to_string(smfs_core::daemon::startup_path(tag)).ok()?;
    let progress = serde_json::from_str(&body).ok()?;
    Some((body, progress))
}
