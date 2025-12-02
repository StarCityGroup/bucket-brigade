use serde::{Deserialize, Serialize};

use aws_sdk_s3::types::{ObjectStorageClass, StorageClass};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BucketInfo {
    pub name: String,
    pub region: Option<String>,
    pub creation_date: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: i64,
    pub last_modified: Option<String>,
    pub storage_class: StorageClassTier,
    pub restore_state: Option<RestoreState>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RestoreState {
    Available,
    InProgress { expiry: Option<String> },
    Expired,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StorageClassTier {
    Standard,
    IntelligentTiering,
    StandardIa,
    OneZoneIa,
    GlacierInstantRetrieval,
    GlacierFlexibleRetrieval,
    GlacierDeepArchive,
    ReducedRedundancy,
    Unknown(String),
}

impl StorageClassTier {
    pub const SELECTABLE: [StorageClassTier; 7] = [
        StorageClassTier::Standard,
        StorageClassTier::IntelligentTiering,
        StorageClassTier::StandardIa,
        StorageClassTier::OneZoneIa,
        StorageClassTier::GlacierInstantRetrieval,
        StorageClassTier::GlacierFlexibleRetrieval,
        StorageClassTier::GlacierDeepArchive,
    ];

    pub fn selectable() -> &'static [StorageClassTier] {
        &Self::SELECTABLE
    }

    pub fn label(&self) -> &str {
        match self {
            StorageClassTier::Standard => "STANDARD",
            StorageClassTier::IntelligentTiering => "INTELLIGENT_TIERING",
            StorageClassTier::StandardIa => "STANDARD_IA",
            StorageClassTier::OneZoneIa => "ONEZONE_IA",
            StorageClassTier::GlacierInstantRetrieval => "GLACIER_IR",
            StorageClassTier::GlacierFlexibleRetrieval => "GLACIER",
            StorageClassTier::GlacierDeepArchive => "DEEP_ARCHIVE",
            StorageClassTier::ReducedRedundancy => "REDUCED_REDUNDANCY",
            StorageClassTier::Unknown(label) => label.as_str(),
        }
    }

    pub fn to_sdk(&self) -> Option<StorageClass> {
        match self {
            StorageClassTier::Standard => Some(StorageClass::Standard),
            StorageClassTier::IntelligentTiering => Some(StorageClass::IntelligentTiering),
            StorageClassTier::StandardIa => Some(StorageClass::StandardIa),
            StorageClassTier::OneZoneIa => Some(StorageClass::OnezoneIa),
            StorageClassTier::GlacierInstantRetrieval => Some(StorageClass::GlacierIr),
            StorageClassTier::GlacierFlexibleRetrieval => Some(StorageClass::Glacier),
            StorageClassTier::GlacierDeepArchive => Some(StorageClass::DeepArchive),
            StorageClassTier::ReducedRedundancy => Some(StorageClass::ReducedRedundancy),
            StorageClassTier::Unknown(_) => None,
        }
    }
}

impl From<Option<ObjectStorageClass>> for StorageClassTier {
    fn from(value: Option<ObjectStorageClass>) -> Self {
        match value {
            Some(ObjectStorageClass::Standard) | None => StorageClassTier::Standard,
            Some(ObjectStorageClass::IntelligentTiering) => StorageClassTier::IntelligentTiering,
            Some(ObjectStorageClass::StandardIa) => StorageClassTier::StandardIa,
            Some(ObjectStorageClass::OnezoneIa) => StorageClassTier::OneZoneIa,
            Some(ObjectStorageClass::GlacierIr) => StorageClassTier::GlacierInstantRetrieval,
            Some(ObjectStorageClass::Glacier) => StorageClassTier::GlacierFlexibleRetrieval,
            Some(ObjectStorageClass::DeepArchive) => StorageClassTier::GlacierDeepArchive,
            Some(ObjectStorageClass::ReducedRedundancy) => StorageClassTier::ReducedRedundancy,
            Some(other) => StorageClassTier::Unknown(other.as_str().to_string()),
        }
    }
}

impl From<Option<StorageClass>> for StorageClassTier {
    fn from(value: Option<StorageClass>) -> Self {
        match value {
            Some(StorageClass::Standard) | None => StorageClassTier::Standard,
            Some(StorageClass::IntelligentTiering) => StorageClassTier::IntelligentTiering,
            Some(StorageClass::StandardIa) => StorageClassTier::StandardIa,
            Some(StorageClass::OnezoneIa) => StorageClassTier::OneZoneIa,
            Some(StorageClass::GlacierIr) => StorageClassTier::GlacierInstantRetrieval,
            Some(StorageClass::Glacier) => StorageClassTier::GlacierFlexibleRetrieval,
            Some(StorageClass::DeepArchive) => StorageClassTier::GlacierDeepArchive,
            Some(StorageClass::ReducedRedundancy) => StorageClassTier::ReducedRedundancy,
            Some(other) => StorageClassTier::Unknown(other.as_str().to_string()),
        }
    }
}
