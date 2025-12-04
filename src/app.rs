use std::collections::VecDeque;

use crate::mask::{MaskKind, ObjectMask};
use crate::models::{BucketInfo, ObjectInfo, StorageClassTier};

const STATUS_LIMIT: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePane {
    Buckets,
    Objects,
    MaskEditor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppMode {
    Browsing,
    EditingMask,
    Confirming,
    SelectingStorageClass,
    ShowingHelp,
    ViewingLog,
    CredentialError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageIntent {
    Transition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskEditorField {
    Pattern,
    Mode,
    Case,
}

impl MaskEditorField {
    pub fn next(self) -> Self {
        match self {
            MaskEditorField::Pattern => MaskEditorField::Mode,
            MaskEditorField::Mode => MaskEditorField::Case,
            MaskEditorField::Case => MaskEditorField::Pattern,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            MaskEditorField::Pattern => MaskEditorField::Case,
            MaskEditorField::Mode => MaskEditorField::Pattern,
            MaskEditorField::Case => MaskEditorField::Mode,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MaskDraft {
    pub pattern: String,
    pub kind: MaskKind,
    pub case_sensitive: bool,
    pub cursor_pos: usize,
}

impl Default for MaskDraft {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            kind: MaskKind::Prefix,
            case_sensitive: false,
            cursor_pos: 0,
        }
    }
}

pub enum PendingAction {
    Transition { target_class: StorageClassTier },
    Restore { days: i32 },
}

pub struct App {
    pub buckets: Vec<BucketInfo>,
    pub all_buckets: Vec<BucketInfo>,
    pub objects: Vec<ObjectInfo>,
    pub filtered_objects: Vec<ObjectInfo>,
    pub selected_bucket: usize,
    pub selected_object: usize,
    pub selected_region: Option<String>,
    pub available_regions: Vec<String>,
    pub status: VecDeque<String>,
    pub active_pane: ActivePane,
    pub mode: AppMode,
    pub mask_draft: MaskDraft,
    pub active_mask: Option<ObjectMask>,
    pub pending_action: Option<PendingAction>,
    pub storage_class_cursor: usize,
    pub storage_intent: StorageIntent,
    pub mask_field: MaskEditorField,
    pub last_bucket_change: Option<std::time::Instant>,
    pub pending_bucket_load: bool,
    // Pagination state
    pub total_object_count: Option<usize>,
    pub continuation_token: Option<String>,
    pub is_loading_objects: bool,
}

impl App {
    pub fn new() -> Self {
        let available_regions = vec![
            "All Regions".to_string(),
            "us-east-1".to_string(),
            "us-east-2".to_string(),
            "us-west-1".to_string(),
            "us-west-2".to_string(),
            "eu-west-1".to_string(),
            "eu-west-2".to_string(),
            "eu-west-3".to_string(),
            "eu-central-1".to_string(),
            "ap-northeast-1".to_string(),
            "ap-northeast-2".to_string(),
            "ap-southeast-1".to_string(),
            "ap-southeast-2".to_string(),
            "ap-south-1".to_string(),
            "sa-east-1".to_string(),
            "ca-central-1".to_string(),
        ];
        Self {
            buckets: Vec::new(),
            all_buckets: Vec::new(),
            objects: Vec::new(),
            filtered_objects: Vec::new(),
            selected_bucket: 0,
            selected_object: 0,
            selected_region: None,
            available_regions,
            status: VecDeque::with_capacity(STATUS_LIMIT),
            active_pane: ActivePane::Buckets,
            mode: AppMode::Browsing,
            mask_draft: MaskDraft::default(),
            active_mask: None,
            pending_action: None,
            storage_class_cursor: 0,
            storage_intent: StorageIntent::Transition,
            mask_field: MaskEditorField::Pattern,
            last_bucket_change: None,
            pending_bucket_load: false,
            total_object_count: None,
            continuation_token: None,
            is_loading_objects: false,
        }
    }

    pub fn selected_bucket_name(&self) -> Option<&str> {
        self.buckets
            .get(self.selected_bucket)
            .map(|b| b.name.as_str())
    }

    pub fn selected_object(&self) -> Option<&ObjectInfo> {
        self.active_objects().get(self.selected_object)
    }

    pub fn active_objects(&self) -> &[ObjectInfo] {
        if self.active_mask.is_some() {
            &self.filtered_objects
        } else {
            &self.objects
        }
    }

    pub fn set_buckets(&mut self, buckets: Vec<BucketInfo>) {
        self.all_buckets = buckets;
        self.apply_region_filter();
    }

    pub fn apply_region_filter(&mut self) {
        if let Some(ref region) = self.selected_region {
            if region == "All Regions" {
                self.buckets = self.all_buckets.clone();
            } else {
                self.buckets = self
                    .all_buckets
                    .iter()
                    .filter(|b| b.region.as_ref() == Some(region))
                    .cloned()
                    .collect();
            }
        } else {
            self.buckets = self.all_buckets.clone();
        }
        self.selected_bucket = 0;
    }

    pub fn set_region(&mut self, region: Option<String>) {
        self.selected_region = region;
        self.apply_region_filter();
    }

    pub fn get_current_region_display(&self) -> String {
        self.selected_region
            .clone()
            .unwrap_or_else(|| "All Regions".to_string())
    }

    pub fn set_objects(&mut self, objects: Vec<ObjectInfo>) {
        self.objects = objects;
        self.filtered_objects = Vec::new();
        self.selected_object = 0;
    }

    pub fn append_objects(&mut self, mut new_objects: Vec<ObjectInfo>) {
        self.objects.append(&mut new_objects);
        // Reapply mask if active
        if let Some(mask) = &self.active_mask {
            self.filtered_objects = self
                .objects
                .iter()
                .filter(|&obj| mask.matches(&obj.key))
                .cloned()
                .collect();
        }
    }

    pub fn reset_pagination(&mut self) {
        self.objects.clear();
        self.filtered_objects.clear();
        self.total_object_count = None;
        self.continuation_token = None;
        self.is_loading_objects = false;
        self.selected_object = 0;
    }

    pub fn has_more_objects(&self) -> bool {
        self.continuation_token.is_some()
    }

    pub fn should_load_more(&self) -> bool {
        // Load more if we're near the end (within last 50 items)
        let threshold = 50;
        let current_pos = self.selected_object;
        let loaded_count = self.objects.len();

        if loaded_count == 0 {
            return false;
        }

        // If we have a mask and few matches, load more
        if let Some(_mask) = &self.active_mask {
            let match_count = self.filtered_objects.len();
            if match_count < 100 && self.has_more_objects() {
                return true;
            }
        }

        // If scrolling near end and more available
        current_pos + threshold >= loaded_count && self.has_more_objects()
    }

    pub fn apply_mask(&mut self, mask: Option<ObjectMask>) {
        self.active_mask = mask.clone();
        if let Some(mask) = mask {
            self.filtered_objects = self
                .objects
                .iter()
                .filter(|&obj| mask.matches(&obj.key))
                .cloned()
                .collect();
            self.selected_object = 0;
            if self.filtered_objects.is_empty() {
                self.push_status("Mask applied but matched no objects");
            } else {
                self.push_status(&format!(
                    "Mask '{}' matched {} objects",
                    mask.name,
                    self.filtered_objects.len()
                ));
            }
        } else {
            self.filtered_objects.clear();
            self.push_status("Cleared mask filter");
        }
    }

    pub fn next_pane(&mut self) {
        self.active_pane = match self.active_pane {
            ActivePane::Buckets => ActivePane::Objects,
            ActivePane::Objects => ActivePane::Buckets,
            ActivePane::MaskEditor => ActivePane::Buckets,
        };
    }

    pub fn previous_pane(&mut self) {
        self.active_pane = match self.active_pane {
            ActivePane::Buckets => ActivePane::Objects,
            ActivePane::Objects => ActivePane::Buckets,
            ActivePane::MaskEditor => ActivePane::Buckets,
        };
    }

    pub fn push_status(&mut self, status: &str) {
        if self.status.len() == STATUS_LIMIT {
            self.status.pop_front();
        }
        self.status.push_back(status.to_string());
    }

    pub fn cycle_mask_kind(&mut self) {
        self.mask_draft.kind = match self.mask_draft.kind {
            MaskKind::Prefix => MaskKind::Suffix,
            MaskKind::Suffix => MaskKind::Contains,
            MaskKind::Contains => MaskKind::Regex,
            MaskKind::Regex => MaskKind::Prefix,
        };
    }

    pub fn cycle_mask_kind_backwards(&mut self) {
        self.mask_draft.kind = match self.mask_draft.kind {
            MaskKind::Prefix => MaskKind::Regex,
            MaskKind::Suffix => MaskKind::Prefix,
            MaskKind::Contains => MaskKind::Suffix,
            MaskKind::Regex => MaskKind::Contains,
        };
    }

    pub fn toggle_mask_case(&mut self) {
        self.mask_draft.case_sensitive = !self.mask_draft.case_sensitive;
    }

    pub fn set_mode(&mut self, mode: AppMode) {
        self.mode = mode;
    }

    pub fn focus_mask_field(&mut self, field: MaskEditorField) {
        self.mask_field = field;
    }

    pub fn next_mask_field(&mut self) {
        self.mask_field = self.mask_field.next();
    }

    pub fn previous_mask_field(&mut self) {
        self.mask_field = self.mask_field.previous();
    }

    /// Check if any of the targeted objects need restoration
    /// (i.e., they are in Glacier storage class and not already restored)
    pub fn any_targets_need_restoration(&self) -> bool {
        let objects = if self.active_mask.is_some() {
            &self.filtered_objects
        } else if let Some(obj) = self.objects.get(self.selected_object) {
            std::slice::from_ref(obj)
        } else {
            return false;
        };

        objects.iter().any(|obj| {
            matches!(
                obj.storage_class,
                StorageClassTier::GlacierFlexibleRetrieval | StorageClassTier::GlacierDeepArchive
            ) && !matches!(
                obj.restore_state,
                Some(crate::models::RestoreState::Available)
                    | Some(crate::models::RestoreState::InProgress { .. })
            )
        })
    }

    /// Get count of objects that need restore (not already restored/restoring)
    pub fn count_objects_needing_restore(&self) -> usize {
        let objects = if self.active_mask.is_some() {
            &self.filtered_objects
        } else if let Some(obj) = self.objects.get(self.selected_object) {
            std::slice::from_ref(obj)
        } else {
            return 0;
        };

        objects
            .iter()
            .filter(|obj| {
                matches!(
                    obj.storage_class,
                    StorageClassTier::GlacierFlexibleRetrieval
                        | StorageClassTier::GlacierDeepArchive
                ) && !matches!(
                    obj.restore_state,
                    Some(crate::models::RestoreState::Available)
                        | Some(crate::models::RestoreState::InProgress { .. })
                )
            })
            .count()
    }

    /// Get count of objects already being restored
    pub fn count_objects_restoring(&self) -> usize {
        let objects = if self.active_mask.is_some() {
            &self.filtered_objects
        } else if let Some(obj) = self.objects.get(self.selected_object) {
            std::slice::from_ref(obj)
        } else {
            return 0;
        };

        objects
            .iter()
            .filter(|obj| {
                matches!(
                    obj.restore_state,
                    Some(crate::models::RestoreState::InProgress { .. })
                )
            })
            .count()
    }
}
