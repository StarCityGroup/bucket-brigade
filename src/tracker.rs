use anyhow::Result;
use std::fs;
use std::path::PathBuf;

use crate::models::{RestoreState, TrackedRestoreRequest};

pub struct RestoreTracker {
    file_path: PathBuf,
    requests: Vec<TrackedRestoreRequest>,
}

impl RestoreTracker {
    pub fn new() -> Result<Self> {
        let config_dir = directories::ProjectDirs::from("com", "bucket-brigade", "bucket-brigade")
            .map(|dirs| dirs.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        fs::create_dir_all(&config_dir)?;
        let file_path = config_dir.join("restore_requests.json");

        let requests = if file_path.exists() {
            let content = fs::read_to_string(&file_path)?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(Self {
            file_path,
            requests,
        })
    }

    pub fn add_request(&mut self, bucket: String, key: String, days: i32) {
        let now = chrono::Utc::now().to_rfc3339();
        self.requests.push(TrackedRestoreRequest {
            bucket,
            key,
            requested_at: now,
            days,
            current_status: RestoreState::InProgress { expiry: None },
        });
        let _ = self.save();
    }

    /// Update the status of a tracked restore request
    #[allow(dead_code)]
    pub fn update_status(&mut self, bucket: &str, key: &str, status: RestoreState) {
        if let Some(req) = self
            .requests
            .iter_mut()
            .find(|r| r.bucket == bucket && r.key == key)
        {
            req.current_status = status.clone();

            // Remove completed requests after they've been available for a while
            if matches!(status, RestoreState::Available) {
                // Could add logic here to remove old available requests
            }
        }
        let _ = self.save();
    }

    /// Get only active (in-progress) restore requests
    #[allow(dead_code)]
    pub fn get_active_requests(&self) -> Vec<TrackedRestoreRequest> {
        self.requests
            .iter()
            .filter(|r| {
                !matches!(
                    r.current_status,
                    RestoreState::Available | RestoreState::Expired
                )
            })
            .cloned()
            .collect()
    }

    pub fn get_all_requests(&self) -> &[TrackedRestoreRequest] {
        &self.requests
    }

    /// Remove completed or expired restore requests from tracking
    #[allow(dead_code)]
    pub fn remove_completed(&mut self) {
        self.requests.retain(|r| {
            !matches!(
                r.current_status,
                RestoreState::Available | RestoreState::Expired
            )
        });
        let _ = self.save();
    }

    fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.requests)?;
        fs::write(&self.file_path, json)?;
        Ok(())
    }
}
