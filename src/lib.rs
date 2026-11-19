// SPDX-License-Identifier: MIT OR Apache-2.0
#![forbid(unsafe_code)]

//! # SigmaOS Renamer File
//!
//! A single & batch file renamer inspired by pyRenamer, designed for the
//! Omarchy desktop environment running on Hyprland with the Catppuccin Mocha
//! colour palette. Provides rule-based rename plans, conflict detection, undo
//! history, and live pattern preview — all with zero external dependencies.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};

// ─── Error types ────────────────────────────────────────────────────────────

/// Errors produced by the renamer domain.
#[derive(Debug, PartialEq)]
pub enum RenamerError {
    /// A rename would produce a name already used by another entry.
    Conflict { source: PathBuf, target: PathBuf },
    /// The rename rule configuration is invalid.
    InvalidRule(String),
    /// The supplied path has no file-name component.
    NoFileName(PathBuf),
    /// An undo was requested but the history is empty.
    EmptyHistory,
    /// Validation failed for the given reason.
    Validation(String),
}

impl fmt::Display for RenamerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { source, target } => write!(
                f,
                "conflict: '{}' would collide with '{}'",
                source.display(),
                target.display()
            ),
            Self::InvalidRule(msg) => write!(f, "invalid rule: {msg}"),
            Self::NoFileName(p) => write!(f, "no file name in path '{}'", p.display()),
            Self::EmptyHistory => write!(f, "undo history is empty"),
            Self::Validation(msg) => write!(f, "validation error: {msg}"),
        }
    }
}

impl std::error::Error for RenamerError {}

// ─── Rename rule types ───────────────────────────────────────────────────────

/// All supported rename transformations.
#[derive(Debug, Clone, PartialEq)]
pub enum RenameRule {
    /// Replace every occurrence of `find` with `replace` in the stem.
    FindReplace { find: String, replace: String },
    /// Prepend a sequential counter, e.g. `001_` or `photo_001`.
    Numbering {
        prefix: String,
        start: u32,
        step: u32,
        width: usize,
        suffix: String,
    },
    /// Change letter casing of the stem.
    Case(CaseKind),
    /// Trim leading/trailing whitespace and optionally inner runs.
    Trim { collapse_inner: bool },
    /// Insert `text` at a byte `position` in the stem (0 = start).
    Insert { text: String, position: usize },
}

/// Casing variants.
#[derive(Debug, Clone, PartialEq)]
pub enum CaseKind {
    Lower,
    Upper,
    TitleCase,
    SentenceCase,
}

impl fmt::Display for RenameRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FindReplace { find, replace } => {
                write!(f, "FindReplace({find:?} → {replace:?})")
            }
            Self::Numbering { prefix, start, step, width, suffix } => write!(
                f,
                "Numbering(prefix={prefix:?}, start={start}, step={step}, width={width}, suffix={suffix:?})"
            ),
            Self::Case(k) => write!(f, "Case({k:?})"),
            Self::Trim { collapse_inner } => write!(f, "Trim(collapse={collapse_inner})"),
            Self::Insert { text, position } => write!(f, "Insert({text:?} @ {position})"),
        }
    }
}

impl RenameRule {
    /// Validate the rule's own parameters, returning an error if misconfigured.
    pub fn validate(&self) -> Result<(), RenamerError> {
        match self {
            Self::FindReplace { find, .. } if find.is_empty() => {
                Err(RenamerError::InvalidRule("find pattern must not be empty".into()))
            }
            Self::Numbering { step, .. } if *step == 0 => {
                Err(RenamerError::InvalidRule("numbering step must be ≥ 1".into()))
            }
            Self::Numbering { width, .. } if *width > 20 => {
                Err(RenamerError::InvalidRule("numbering width must be ≤ 20".into()))
            }
            _ => Ok(()),
        }
    }

    /// Apply this rule to a file *stem* (no extension), returning the new stem.
    /// For `Numbering` the `index` is the 0-based position of the file in the batch.
    pub fn apply_to_stem(&self, stem: &str, index: u32) -> String {
        match self {
            Self::FindReplace { find, replace } => stem.replace(find.as_str(), replace.as_str()),
            Self::Numbering { prefix, start, step, width, suffix } => {
                let n = start + index * step;
                format!("{prefix}{n:0>width$}{suffix}{stem}", width = width)
            }
            Self::Case(kind) => apply_case(stem, kind),
            Self::Trim { collapse_inner } => {
                let trimmed = stem.trim();
                if *collapse_inner {
                    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
                } else {
                    trimmed.to_owned()
                }
            }
            Self::Insert { text, position } => {
                let pos = (*position).min(stem.len());
                let mut s = stem.to_owned();
                s.insert_str(pos, text);
                s
            }
        }
    }
}

fn apply_case(s: &str, kind: &CaseKind) -> String {
    match kind {
        CaseKind::Lower => s.to_lowercase(),
        CaseKind::Upper => s.to_uppercase(),
        CaseKind::TitleCase => s
            .split_whitespace()
            .map(|w| {
                let mut chars = w.chars();
                match chars.next() {
                    None => String::new(),
                    Some(c) => {
                        let upper: String = c.to_uppercase().collect();
                        upper + &chars.as_str().to_lowercase()
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        CaseKind::SentenceCase => {
            let mut chars = s.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => {
                    let first: String = c.to_uppercase().collect();
                    first + &chars.as_str().to_lowercase()
                }
            }
        }
    }
}

// ─── Rename plan ─────────────────────────────────────────────────────────────

/// A single rename operation: original path → proposed new path.
#[derive(Debug, Clone, PartialEq)]
pub struct RenameEntry {
    pub original: PathBuf,
    pub proposed: PathBuf,
}

/// A validated rename plan ready to execute (or preview).
#[derive(Debug, Default)]
pub struct RenamePlan {
    entries: Vec<RenameEntry>,
}

impl RenamePlan {
    /// Number of rename operations in the plan.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over all entries.
    pub fn entries(&self) -> &[RenameEntry] {
        &self.entries
    }

    /// Return a human-readable diff table (original → proposed).
    pub fn preview(&self) -> String {
        self.entries
            .iter()
            .map(|e| {
                format!(
                    "{} → {}",
                    e.original.display(),
                    e.proposed.display()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ─── Conflict detector ───────────────────────────────────────────────────────

/// Inspect a plan for name collisions.
pub fn detect_conflicts(plan: &RenamePlan) -> Vec<RenamerError> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut errors = Vec::new();

    for entry in &plan.entries {
        if !seen.insert(entry.proposed.clone()) {
            errors.push(RenamerError::Conflict {
                source: entry.original.clone(),
                target: entry.proposed.clone(),
            });
        }
    }
    errors
}

// ─── Plan builder ────────────────────────────────────────────────────────────

/// Builds a `RenamePlan` from a list of paths and an ordered chain of rules.
#[derive(Debug, Default)]
pub struct PlanBuilder {
    rules: Vec<RenameRule>,
}

impl PlanBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a rule to the chain (rules are applied left-to-right).
    pub fn add_rule(mut self, rule: RenameRule) -> Result<Self, RenamerError> {
        rule.validate()?;
        self.rules.push(rule);
        Ok(self)
    }

    /// Build a `RenamePlan` for the given paths.
    pub fn build(&self, paths: &[PathBuf]) -> Result<RenamePlan, RenamerError> {
        let mut entries = Vec::with_capacity(paths.len());

        for (idx, path) in paths.iter().enumerate() {
            let file_name = path
                .file_name()
                .ok_or_else(|| RenamerError::NoFileName(path.clone()))?
                .to_string_lossy();

            let (stem, ext) = split_stem_ext(&file_name);

            let new_stem = self.rules.iter().fold(stem.to_owned(), |acc, rule| {
                rule.apply_to_stem(&acc, idx as u32)
            });

            let new_name = if ext.is_empty() {
                new_stem
            } else {
                format!("{new_stem}.{ext}")
            };

            let proposed = path.with_file_name(new_name);
            entries.push(RenameEntry {
                original: path.clone(),
                proposed,
            });
        }

        Ok(RenamePlan { entries })
    }
}

/// Split a filename into (stem, extension), handling dotfiles correctly.
fn split_stem_ext(name: &str) -> (&str, &str) {
    // Dotfiles: ".bashrc" → stem=".bashrc", ext=""
    if name.starts_with('.') && name.chars().filter(|&c| c == '.').count() == 1 {
        return (name, "");
    }
    match name.rfind('.') {
        None => (name, ""),
        Some(0) => (name, ""),
        Some(pos) => (&name[..pos], &name[pos + 1..]),
    }
}

// ─── Undo history ────────────────────────────────────────────────────────────

/// Tracks executed rename operations so they can be undone.
#[derive(Debug, Default)]
pub struct UndoHistory {
    stack: VecDeque<Vec<RenameEntry>>,
    max_depth: usize,
}

impl UndoHistory {
    /// Create a history limited to `max_depth` batches.
    pub fn new(max_depth: usize) -> Self {
        Self {
            stack: VecDeque::new(),
            max_depth: max_depth.max(1),
        }
    }

    /// Push an executed plan's entries onto the undo stack.
    pub fn push(&mut self, entries: Vec<RenameEntry>) {
        if self.stack.len() == self.max_depth {
            self.stack.pop_front();
        }
        self.stack.push_back(entries);
    }

    /// Pop the most recent batch, returning the reversed entries (new→old).
    pub fn pop_undo(&mut self) -> Result<Vec<RenameEntry>, RenamerError> {
        self.stack.pop_back().map(|batch| {
            batch
                .into_iter()
                .map(|e| RenameEntry {
                    original: e.proposed,
                    proposed: e.original,
                })
                .collect()
        }).ok_or(RenamerError::EmptyHistory)
    }

    /// How many undo levels are currently available.
    pub fn depth(&self) -> usize {
        self.stack.len()
    }
}

// ─── Pattern preview ─────────────────────────────────────────────────────────

/// Preview how a single filename transforms under a rule chain without building
/// a full plan.
pub fn preview_name(
    file_name: &str,
    rules: &[RenameRule],
    index: u32,
) -> String {
    let (stem, ext) = split_stem_ext(file_name);
    let new_stem = rules.iter().fold(stem.to_owned(), |acc, rule| {
        rule.apply_to_stem(&acc, index)
    });
    if ext.is_empty() {
        new_stem
    } else {
        format!("{new_stem}.{ext}")
    }
}

// ─── Rename statistics ───────────────────────────────────────────────────────

/// Summary statistics for a rename plan.
#[derive(Debug, Default, PartialEq)]
pub struct PlanStats {
    pub total: usize,
    pub changed: usize,
    pub unchanged: usize,
}

impl PlanStats {
    pub fn from_plan(plan: &RenamePlan) -> Self {
        let changed = plan
            .entries()
            .iter()
            .filter(|e| e.original != e.proposed)
            .count();
        Self {
            total: plan.len(),
            changed,
            unchanged: plan.len() - changed,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // ── 1. FindReplace rule ──────────────────────────────────────────────────
    #[test]
    fn test_find_replace_applies_substitution() {
        // Arrange
        let rule = RenameRule::FindReplace {
            find: "IMG".into(),
            replace: "photo".into(),
        };
        // Act
        let result = rule.apply_to_stem("IMG_1234", 0);
        // Assert
        assert_eq!(result, "photo_1234");
    }

    // ── 2. FindReplace empty pattern is invalid ──────────────────────────────
    #[test]
    fn test_find_replace_empty_pattern_is_invalid() {
        // Arrange
        let rule = RenameRule::FindReplace {
            find: String::new(),
            replace: "x".into(),
        };
        // Act
        let result = rule.validate();
        // Assert
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RenamerError::InvalidRule(_)));
    }

    // ── 3. Numbering rule ────────────────────────────────────────────────────
    #[test]
    fn test_numbering_pads_and_sequences_correctly() {
        // Arrange
        let rule = RenameRule::Numbering {
            prefix: "".into(),
            start: 1,
            step: 1,
            width: 3,
            suffix: "_".into(),
        };
        // Act
        let first = rule.apply_to_stem("vacation", 0);
        let second = rule.apply_to_stem("beach", 1);
        // Assert
        assert_eq!(first, "001_vacation");
        assert_eq!(second, "002_beach");
    }

    // ── 4. Case rules ────────────────────────────────────────────────────────
    #[test]
    fn test_case_title_capitalises_words() {
        // Arrange
        let rule = RenameRule::Case(CaseKind::TitleCase);
        // Act
        let result = rule.apply_to_stem("hello world from omarchy", 0);
        // Assert
        assert_eq!(result, "Hello World From Omarchy");
    }

    // ── 5. Trim collapses inner whitespace ───────────────────────────────────
    #[test]
    fn test_trim_collapses_inner_whitespace() {
        // Arrange
        let rule = RenameRule::Trim { collapse_inner: true };
        // Act
        let result = rule.apply_to_stem("  my  file   name  ", 0);
        // Assert
        assert_eq!(result, "my file name");
    }

    // ── 6. Insert rule ───────────────────────────────────────────────────────
    #[test]
    fn test_insert_at_position_zero_is_prefix() {
        // Arrange
        let rule = RenameRule::Insert {
            text: "DRAFT_".into(),
            position: 0,
        };
        // Act
        let result = rule.apply_to_stem("report", 0);
        // Assert
        assert_eq!(result, "DRAFT_report");
    }

    // ── 7. Plan builder produces correct proposed paths ───────────────────────
    #[test]
    fn test_plan_builder_produces_correct_proposed_paths() {
        // Arrange
        let paths = vec![
            pb("/home/user/IMG_001.jpg"),
            pb("/home/user/IMG_002.jpg"),
        ];
        let builder = PlanBuilder::new()
            .add_rule(RenameRule::FindReplace {
                find: "IMG".into(),
                replace: "photo".into(),
            })
            .unwrap();
        // Act
        let plan = builder.build(&paths).unwrap();
        // Assert
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.entries()[0].proposed, pb("/home/user/photo_001.jpg"));
        assert_eq!(plan.entries()[1].proposed, pb("/home/user/photo_002.jpg"));
    }

    // ── 8. Conflict detection catches duplicate targets ───────────────────────
    #[test]
    fn test_conflict_detection_catches_duplicates() {
        // Arrange — two different sources that map to the same target
        let plan = RenamePlan {
            entries: vec![
                RenameEntry {
                    original: pb("/tmp/a.txt"),
                    proposed: pb("/tmp/same.txt"),
                },
                RenameEntry {
                    original: pb("/tmp/b.txt"),
                    proposed: pb("/tmp/same.txt"),
                },
            ],
        };
        // Act
        let conflicts = detect_conflicts(&plan);
        // Assert
        assert_eq!(conflicts.len(), 1);
        assert!(matches!(conflicts[0], RenamerError::Conflict { .. }));
    }

    // ── 9. Undo history reverses entries ─────────────────────────────────────
    #[test]
    fn test_undo_history_reverses_entries() {
        // Arrange
        let mut history = UndoHistory::new(10);
        let entries = vec![RenameEntry {
            original: pb("/tmp/old.txt"),
            proposed: pb("/tmp/new.txt"),
        }];
        history.push(entries);
        // Act
        let undo_entries = history.pop_undo().unwrap();
        // Assert
        assert_eq!(undo_entries[0].original, pb("/tmp/new.txt"));
        assert_eq!(undo_entries[0].proposed, pb("/tmp/old.txt"));
    }

    // ── 10. Empty undo stack returns EmptyHistory error ───────────────────────
    #[test]
    fn test_empty_undo_history_returns_error() {
        // Arrange
        let mut history = UndoHistory::new(5);
        // Act
        let result = history.pop_undo();
        // Assert
        assert_eq!(result.unwrap_err(), RenamerError::EmptyHistory);
    }

    // ── 11. preview_name works without a full plan ────────────────────────────
    #[test]
    fn test_preview_name_no_plan_needed() {
        // Arrange
        let rules = vec![
            RenameRule::Case(CaseKind::Lower),
            RenameRule::FindReplace {
                find: " ".into(),
                replace: "_".into(),
            },
        ];
        // Act
        let preview = preview_name("My Document.pdf", &rules, 0);
        // Assert
        assert_eq!(preview, "my_document.pdf");
    }

    // ── 12. PlanStats counts changed vs unchanged entries ─────────────────────
    #[test]
    fn test_plan_stats_counts_correctly() {
        // Arrange
        let plan = RenamePlan {
            entries: vec![
                RenameEntry { original: pb("/a.txt"), proposed: pb("/a.txt") }, // unchanged
                RenameEntry { original: pb("/b.txt"), proposed: pb("/B.txt") }, // changed
                RenameEntry { original: pb("/c.txt"), proposed: pb("/d.txt") }, // changed
            ],
        };
        // Act
        let stats = PlanStats::from_plan(&plan);
        // Assert
        assert_eq!(stats.total, 3);
        assert_eq!(stats.changed, 2);
        assert_eq!(stats.unchanged, 1);
    }

    // ── 13. split_stem_ext handles dotfiles ───────────────────────────────────
    #[test]
    fn test_split_stem_ext_handles_dotfiles() {
        // Arrange / Act / Assert
        let (stem, ext) = split_stem_ext(".bashrc");
        assert_eq!(stem, ".bashrc");
        assert_eq!(ext, "");

        let (stem2, ext2) = split_stem_ext("archive.tar.gz");
        assert_eq!(stem2, "archive.tar");
        assert_eq!(ext2, "gz");

        let (stem3, ext3) = split_stem_ext("README");
        assert_eq!(stem3, "README");
        assert_eq!(ext3, "");
    }

    // ── 14. UndoHistory respects max depth (oldest batch evicted) ─────────────
    #[test]
    fn test_undo_history_max_depth_evicts_oldest() {
        // Arrange
        let mut history = UndoHistory::new(2);
        history.push(vec![RenameEntry { original: pb("/1.txt"), proposed: pb("/a.txt") }]);
        history.push(vec![RenameEntry { original: pb("/2.txt"), proposed: pb("/b.txt") }]);
        history.push(vec![RenameEntry { original: pb("/3.txt"), proposed: pb("/c.txt") }]); // evicts batch 1
        // Act
        let d = history.depth();
        let last = history.pop_undo().unwrap();
        // Assert
        assert_eq!(d, 2);
        assert_eq!(last[0].original, pb("/c.txt")); // most recent (reversed)
    }
}
