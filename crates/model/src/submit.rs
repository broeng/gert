use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Outcome of a submit requirement (`SubmitRequirementResultInfo.status`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubmitRequirementStatus {
    Satisfied,
    Unsatisfied,
    Overridden,
    NotApplicable,
    Error,
    Forced,
    Timeout,
    Unknown(String),
}

impl SubmitRequirementStatus {
    /// Maps Gerrit's wire value (`SATISFIED`, `NOT_APPLICABLE`, ...).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "SATISFIED" => Self::Satisfied,
            "UNSATISFIED" => Self::Unsatisfied,
            "OVERRIDDEN" => Self::Overridden,
            "NOT_APPLICABLE" => Self::NotApplicable,
            "ERROR" => Self::Error,
            "FORCED" => Self::Forced,
            "TIMEOUT" => Self::Timeout,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// True if this requirement does not block submission.
    pub fn is_ok(&self) -> bool {
        matches!(
            self,
            Self::Satisfied | Self::Overridden | Self::NotApplicable | Self::Forced
        )
    }
}

/// Evaluation state of one expression (`SubmitRequirementExpressionInfo.status`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExpressionStatus {
    Pass,
    Fail,
    Error,
    NotEvaluated,
    Timeout,
    Unknown(String),
}

impl ExpressionStatus {
    /// Maps Gerrit's wire value (`PASS`, `FAIL`, ...).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "PASS" => Self::Pass,
            "FAIL" => Self::Fail,
            "ERROR" => Self::Error,
            "NOT_EVALUATED" => Self::NotEvaluated,
            "TIMEOUT" => Self::Timeout,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

/// The result of evaluating one query expression of a submit requirement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpressionResult {
    pub expression: String,
    pub fulfilled: bool,
    pub status: Option<ExpressionStatus>,
    /// Predicates of the expression that matched / did not match.
    pub passing_atoms: Vec<String>,
    pub failing_atoms: Vec<String>,
    /// Explanation per atom, for atoms that provide one.
    pub atom_explanations: BTreeMap<String, String>,
    pub error_message: Option<String>,
}

/// A submit requirement and its state on one change
/// (`SubmitRequirementResultInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitRequirement {
    pub name: String,
    pub description: Option<String>,
    pub status: SubmitRequirementStatus,
    /// True for requirements Gerrit derives from the old label-based submit
    /// rules rather than from the project configuration.
    pub is_legacy: bool,
    /// Whether the requirement applies to the change at all.
    pub applicability: Option<ExpressionResult>,
    /// The condition that must hold for the requirement to be satisfied.
    pub submittability: Option<ExpressionResult>,
    /// If it holds, the requirement counts as overridden.
    pub override_result: Option<ExpressionResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_wire_and_blocking() {
        use SubmitRequirementStatus::*;
        assert_eq!(SubmitRequirementStatus::from_wire("SATISFIED"), Satisfied);
        assert_eq!(
            SubmitRequirementStatus::from_wire("NOT_APPLICABLE"),
            NotApplicable
        );
        let u = SubmitRequirementStatus::from_wire("PAUSED");
        assert_eq!(u, Unknown("PAUSED".into()));
        assert!(Satisfied.is_ok() && Forced.is_ok() && Overridden.is_ok());
        assert!(!Unsatisfied.is_ok() && !Error.is_ok() && !u.is_ok());
    }

    #[test]
    fn expression_status_from_wire() {
        assert_eq!(ExpressionStatus::from_wire("PASS"), ExpressionStatus::Pass);
        assert_eq!(
            ExpressionStatus::from_wire("NOT_EVALUATED"),
            ExpressionStatus::NotEvaluated
        );
        assert_eq!(
            ExpressionStatus::from_wire("?"),
            ExpressionStatus::Unknown("?".into())
        );
    }
}
