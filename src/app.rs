use std::collections::VecDeque;

use crate::mask::{MaskKind, ObjectMask};
use crate::models::{BucketInfo, ObjectInfo, StorageClassTier};
use crate::policy::MigrationPolicy;

const STATUS_LIMIT: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePane {
    Buckets,
    Objects,
    MaskEditor,
    Policies,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppMode {
    Browsing,
    EditingMask,
    Confirming,
    SelectingStorageClass,
    ShowingHelp,
    ViewingLog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageIntent {
    Transition,
    SavePolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskEditorField {
    Name,
    Pattern,
    Mode,
    Case,
}

impl MaskEditorField {
    pub fn next(self) -> Self {
        match self {
            MaskEditorField::Name => MaskEditorField::Pattern,
            MaskEditorField::Pattern => MaskEditorField::Mode,
            MaskEditorField::Mode => MaskEditorField::Case,
            MaskEditorField::Case => MaskEditorField::Name,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            MaskEditorField::Name => MaskEditorField::Case,
            MaskEditorField::Pattern => MaskEditorField::Name,
            MaskEditorField::Mode => MaskEditorField::Pattern,
            MaskEditorField::Case => MaskEditorField::Mode,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MaskDraft {
    pub name: String,
    pub pattern: String,
    pub kind: MaskKind,
    pub case_sensitive: bool,
}

impl Default for MaskDraft {
    fn default() -> Self {
        Self {
            name: "Untitled mask".into(),
            pattern: String::new(),
            kind: MaskKind::Prefix,
            case_sensitive: false,
        }
    }
}

pub enum PendingAction {
    Transition {
        target_class: StorageClassTier,
        restore_first: bool,
    },
    Restore {
        days: i32,
    },
    SavePolicy {
        target_class: StorageClassTier,
    },
}

pub struct App {
    pub buckets: Vec<BucketInfo>,
    pub objects: Vec<ObjectInfo>,
    pub filtered_objects: Vec<ObjectInfo>,
    pub selected_bucket: usize,
    pub selected_object: usize,
    pub status: VecDeque<String>,
    pub active_pane: ActivePane,
    pub mode: AppMode,
    pub mask_draft: MaskDraft,
    pub active_mask: Option<ObjectMask>,
    pub policies: Vec<MigrationPolicy>,
    pub pending_action: Option<PendingAction>,
    pub storage_class_cursor: usize,
    pub storage_intent: StorageIntent,
    pub mask_field: MaskEditorField,
}

impl App {
    pub fn new(policies: Vec<MigrationPolicy>) -> Self {
        Self {
            buckets: Vec::new(),
            objects: Vec::new(),
            filtered_objects: Vec::new(),
            selected_bucket: 0,
            selected_object: 0,
            status: VecDeque::with_capacity(STATUS_LIMIT),
            active_pane: ActivePane::Buckets,
            mode: AppMode::Browsing,
            mask_draft: MaskDraft::default(),
            active_mask: None,
            policies,
            pending_action: None,
            storage_class_cursor: 0,
            storage_intent: StorageIntent::Transition,
            mask_field: MaskEditorField::Pattern,
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
        self.buckets = buckets;
        self.selected_bucket = 0;
    }

    pub fn set_objects(&mut self, objects: Vec<ObjectInfo>) {
        self.objects = objects;
        self.filtered_objects = Vec::new();
        self.selected_object = 0;
    }

    pub fn apply_mask(&mut self, mask: Option<ObjectMask>) {
        self.active_mask = mask.clone();
        if let Some(mask) = mask {
            self.filtered_objects = self
                .objects
                .iter()
                .cloned()
                .filter(|obj| mask.matches(&obj.key))
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
            ActivePane::Objects => ActivePane::MaskEditor,
            ActivePane::MaskEditor => ActivePane::Policies,
            ActivePane::Policies => ActivePane::Buckets,
        };
    }

    pub fn previous_pane(&mut self) {
        self.active_pane = match self.active_pane {
            ActivePane::Buckets => ActivePane::Policies,
            ActivePane::Objects => ActivePane::Buckets,
            ActivePane::MaskEditor => ActivePane::Objects,
            ActivePane::Policies => ActivePane::MaskEditor,
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
}
