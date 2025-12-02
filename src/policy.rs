use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::mask::ObjectMask;
use crate::models::StorageClassTier;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationPolicy {
    pub id: Uuid,
    pub bucket: String,
    pub mask: ObjectMask,
    pub target_storage_class: StorageClassTier,
    pub restore_before_transition: bool,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl MigrationPolicy {
    pub fn new(
        bucket: String,
        mask: ObjectMask,
        target_storage_class: StorageClassTier,
        restore_before_transition: bool,
        notes: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            bucket,
            mask,
            target_storage_class,
            restore_before_transition,
            notes,
            created_at: Utc::now(),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PolicyFile {
    policies: Vec<MigrationPolicy>,
}

pub struct PolicyStore {
    path: PathBuf,
    pub policies: Vec<MigrationPolicy>,
}

impl PolicyStore {
    pub fn load_or_default() -> Result<Self> {
        let path = default_store_path()?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            return Ok(Self {
                path,
                policies: Vec::new(),
            });
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read policy file at {}", path.to_string_lossy()))?;
        let file: PolicyFile = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse policy file {}", path.display()))?;
        Ok(Self {
            path,
            policies: file.policies,
        })
    }

    pub fn save(&self) -> Result<()> {
        let data = PolicyFile {
            policies: self.policies.clone(),
        };
        let contents = serde_json::to_string_pretty(&data)?;
        fs::write(&self.path, contents)
            .with_context(|| format!("failed to save policies to {}", self.path.display()))?;
        Ok(())
    }

    pub fn add(&mut self, policy: MigrationPolicy) -> Result<()> {
        self.policies.push(policy);
        self.save()
    }
}

fn default_store_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("com", "s3-migration-manager", "s3-migration-manager")
        .context("could not resolve configuration directory")?;
    Ok(dirs.config_dir().join("policies.json"))
}
