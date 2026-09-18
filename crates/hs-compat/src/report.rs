//! The translation report: what happened to every key the translator found
//! in a source `homeserver.yaml`, keyed by the classification in
//! `crate::classification`.

use std::fmt;

use crate::classification::Classification;

/// What happened to one key found in the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOutcome {
    /// The key as it appeared in the source (dotted for nested keys this
    /// translator understands, e.g. `experimental_features.msc3861`).
    pub key: String,
    /// Mapped, mapped-with-a-difference, unsupported, or (for a key this
    /// translator has never heard of) unrecognized.
    pub classification: OutcomeClassification,
    /// The native `hs-config` path(s) written to, if any.
    pub native: String,
    /// A human-readable note (the reason code for unsupported keys, or a
    /// caveat for keys translated with a documented difference).
    pub note: String,
}

/// [`Classification`] plus the one case that isn't in the static
/// translation table at all: a key the pinned Synapse inventory has never
/// seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeClassification {
    /// See [`Classification::Mapped`].
    Mapped,
    /// See [`Classification::MappedDiff`].
    MappedDiff,
    /// See [`Classification::Unsupported`].
    Unsupported,
    /// Not in `docs/synapse-inventory.md` as of the pinned Synapse release
    /// — either a typo, a very new option, or a private/undocumented one.
    /// Treated the same as `Unsupported` for the fail-closed check.
    Unrecognized,
}

impl From<Classification> for OutcomeClassification {
    fn from(c: Classification) -> Self {
        match c {
            Classification::Mapped => OutcomeClassification::Mapped,
            Classification::MappedDiff => OutcomeClassification::MappedDiff,
            Classification::Unsupported => OutcomeClassification::Unsupported,
        }
    }
}

impl fmt::Display for OutcomeClassification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OutcomeClassification::Mapped => "mapped",
            OutcomeClassification::MappedDiff => "mapped (diff)",
            OutcomeClassification::Unsupported => "unsupported",
            OutcomeClassification::Unrecognized => "unrecognized",
        })
    }
}

/// The full outcome of translating one `homeserver.yaml`: one
/// [`KeyOutcome`] per key found in the source (recursing one level into
/// `experimental_features`).
#[derive(Debug, Clone, Default)]
pub struct TranslationReport {
    /// One entry per source key, in the order they were encountered.
    pub outcomes: Vec<KeyOutcome>,
}

impl TranslationReport {
    /// An empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one key's outcome.
    pub fn record(
        &mut self,
        key: impl Into<String>,
        classification: OutcomeClassification,
        native: impl Into<String>,
        note: impl Into<String>,
    ) {
        self.outcomes.push(KeyOutcome {
            key: key.into(),
            classification,
            native: native.into(),
            note: note.into(),
        });
    }

    /// Every outcome that blocks translation unless
    /// `--allow-unsupported-synapse-config` is passed.
    pub fn blocking(&self) -> impl Iterator<Item = &KeyOutcome> {
        self.outcomes.iter().filter(|o| {
            matches!(
                o.classification,
                OutcomeClassification::Unsupported | OutcomeClassification::Unrecognized
            )
        })
    }

    /// True when [`Self::blocking`] would yield anything.
    pub fn has_blocking(&self) -> bool {
        self.blocking().next().is_some()
    }

    /// Renders a Markdown table, one row per key, in encounter order.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("| Key | Status | Native | Note |\n|---|---|---|---|\n");
        for o in &self.outcomes {
            use std::fmt::Write as _;
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                o.key, o.classification, o.native, o.note
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_includes_unsupported_and_unrecognized_only() {
        let mut report = TranslationReport::new();
        report.record(
            "server_name",
            OutcomeClassification::Mapped,
            "server.server_name",
            "",
        );
        report.record(
            "gc_thresholds",
            OutcomeClassification::Unsupported,
            "",
            "R-PY.",
        );
        report.record(
            "totally_made_up",
            OutcomeClassification::Unrecognized,
            "",
            "not in the inventory",
        );
        assert_eq!(report.blocking().count(), 2);
        assert!(report.has_blocking());
    }

    #[test]
    fn markdown_has_one_row_per_outcome_plus_header() {
        let mut report = TranslationReport::new();
        report.record(
            "server_name",
            OutcomeClassification::Mapped,
            "server.server_name",
            "",
        );
        let md = report.to_markdown();
        assert_eq!(md.lines().count(), 3);
        assert!(md.contains("server_name"));
    }
}
