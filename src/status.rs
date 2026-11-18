use tower_lsp::lsp_types::DiagnosticSeverity;

/// A whole-feature condition worth surfacing at the top of every file.
///
/// Policy: a feature that cannot work at all is an ERROR; a feature that
/// works with reduced fidelity is a WARNING. Both use the same message
/// shape so the state is always visible and greppable:
/// `<feature>: unavailable|degraded — <detail>`.
#[derive(Clone, Debug)]
pub struct FeatureIssue {
    pub feature: &'static str,
    pub severity: DiagnosticSeverity,
    pub detail: String,
}

impl FeatureIssue {
    pub fn unavailable(feature: &'static str, detail: impl Into<String>) -> Self {
        Self {
            feature,
            severity: DiagnosticSeverity::ERROR,
            detail: detail.into(),
        }
    }

    pub fn degraded(feature: &'static str, detail: impl Into<String>) -> Self {
        Self {
            feature,
            severity: DiagnosticSeverity::WARNING,
            detail: detail.into(),
        }
    }

    pub fn message(&self) -> String {
        let state = match self.severity {
            DiagnosticSeverity::ERROR => "unavailable",
            _ => "degraded",
        };
        format!("{}: {state} — {}", self.feature, self.detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_standardized() {
        let issue = FeatureIssue::unavailable("bindings", "no bindings dir");
        assert_eq!(issue.message(), "bindings: unavailable — no bindings dir");
        assert_eq!(issue.severity, DiagnosticSeverity::ERROR);
        let issue = FeatureIssue::degraded("dtschema", "pip install dtschema");
        assert_eq!(issue.message(), "dtschema: degraded — pip install dtschema");
        assert_eq!(issue.severity, DiagnosticSeverity::WARNING);
    }
}
