//! Who owns the shape of a thing: GitOps config, or the API.
//!
//! The distinction is load-bearing for everything the reconciler touches —
//! channels and service accounts today (design §11). A **managed** object is
//! declared in config and put back the way config says at boot and on `SIGHUP`, so
//! the API must refuse to reshape or delete it: otherwise an operator deletes a
//! channel, watches it resurrect, and spends an afternoon on it. A **dynamic**
//! object was created through the API or the UI, and the reconciler never touches
//! it.

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, Result};

/// Where an object came from, and therefore who may change it.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Declared in GitOps config and reconciled from it (design §11). Shown with a
    /// `managed` badge in the UI, with the config-driven fields read-only.
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

    /// Refuse an API mutation that GitOps would immediately undo.
    ///
    /// `what` names the object in the message, so the operator is told where to go
    /// instead of only being told no.
    pub fn ensure_mutable_through_api(self, what: &'static str) -> Result<()> {
        match self {
            Self::Dynamic => Ok(()),
            Self::Managed => Err(DomainError::Malformed {
                field: what,
                reason: "is managed by GitOps — change it in config",
            }),
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
}
