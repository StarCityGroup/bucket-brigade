use std::borrow::Cow;
use std::fmt;

use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MaskKind {
    Prefix,
    Suffix,
    Contains,
    Regex,
}

impl fmt::Display for MaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            MaskKind::Prefix => "Prefix",
            MaskKind::Suffix => "Suffix",
            MaskKind::Contains => "Contains",
            MaskKind::Regex => "Regex",
        };
        f.write_str(label)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectMask {
    pub name: String,
    pub pattern: String,
    pub kind: MaskKind,
    pub case_sensitive: bool,
}

impl ObjectMask {
    pub fn matches(&self, key: &str) -> bool {
        match self.kind {
            MaskKind::Regex => self.regex_match(key),
            MaskKind::Prefix => normalized_cmp(self, key, Comparison::Prefix),
            MaskKind::Suffix => normalized_cmp(self, key, Comparison::Suffix),
            MaskKind::Contains => normalized_cmp(self, key, Comparison::Contains),
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} ({:?}: {})",
            self.name,
            self.kind,
            if self.case_sensitive {
                self.pattern.clone()
            } else {
                format!("{} (insensitive)", self.pattern)
            }
        )
    }

    fn regex_match(&self, key: &str) -> bool {
        RegexBuilder::new(&self.pattern)
            .case_insensitive(!self.case_sensitive)
            .build()
            .map(|re| re.is_match(key))
            .unwrap_or(false)
    }
}

enum Comparison {
    Prefix,
    Suffix,
    Contains,
}

fn normalized<'a>(mask: &ObjectMask, input: &'a str) -> Cow<'a, str> {
    if mask.case_sensitive {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(input.to_lowercase())
    }
}

fn normalized_cmp(mask: &ObjectMask, key: &str, comparison: Comparison) -> bool {
    let key = normalized(mask, key);
    let pattern = normalized(mask, &mask.pattern);
    match comparison {
        Comparison::Prefix => key.starts_with(pattern.as_ref()),
        Comparison::Suffix => key.ends_with(pattern.as_ref()),
        Comparison::Contains => key.contains(pattern.as_ref()),
    }
}
