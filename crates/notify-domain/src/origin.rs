//! Who owns the shape of a thing (§11): **managed** objects are reconciled from
//! GitOps config and read-only through the API; **dynamic** ones the reconciler never touches.

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, Result};

/// Where an object came from, and therefore who may change it.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Declared in GitOps config and reconciled from it (§11); read-only in the UI.
    Managed,
    /// Created through the API or the UI. The reconciler leaves it alone.
    Dynamic,
}

impl Origin {
    /// Whether the reconciler owns this object.
    #[must_use]
    pub const fn is_managed(self) -> bool {
        matches!(self, Self::Managed)
    }

    /// Refuse an API mutation that GitOps would immediately undo, as a 409 naming `what`.
    pub fn ensure_mutable_through_api(self, what: &'static str) -> Result<()> {
        match self {
            Self::Dynamic => Ok(()),
            Self::Managed => Err(DomainError::ManagedByConfig { what }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dynamic_objects_may_be_reshaped_through_the_api() {
        assert!(
            Origin::Dynamic
                .ensure_mutable_through_api("channel")
                .is_ok()
        );
        assert!(
            Origin::Managed
                .ensure_mutable_through_api("channel")
                .is_err(),
            "deleting it through the API would just have GitOps put it back"
        );
        assert!(Origin::Managed.is_managed());
        assert!(!Origin::Dynamic.is_managed());
    }

    #[test]
    fn being_managed_is_a_conflict_and_not_a_malformed_request() {
        // The status §8 documents, and the code a client branches on.
        let refused = Origin::Managed
            .ensure_mutable_through_api("channel.policy")
            .unwrap_err();

        assert_eq!(refused.code(), "origin.managed");
        assert_eq!(refused.kind(), crate::ErrorKind::Conflict);
    }
}
