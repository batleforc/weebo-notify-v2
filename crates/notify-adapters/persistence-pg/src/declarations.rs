//! What GitOps last applied (design §11, §13, D28).

use notify_ports::{Declaration, DeclarationKind, DeclarationStore, PortResult};
use sqlx::Row as _;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::fail;

#[async_trait::async_trait]
impl DeclarationStore for PgStore {
    async fn snapshot(&self, kind: DeclarationKind) -> PortResult<Vec<Declaration>> {
        // Ordered here rather than by the caller: `diff` sorts what it returns, but
        // an unordered read would still make two identical passes produce different
        // intermediate states, which is the kind of thing that only shows up under
        // a flaky test.
        let rows = sqlx::query(
            "SELECT key, snapshot FROM managed_declarations
              WHERE kind = $1
              ORDER BY key",
        )
        .bind(kind.as_str())
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("declaration", error))?;

        rows.iter()
            .map(|row| {
                Ok(Declaration {
                    key: row.try_get("key").map_err(|e| fail("declaration", e))?,
                    snapshot: row
                        .try_get("snapshot")
                        .map_err(|e| fail("declaration", e))?,
                })
            })
            .collect()
    }

    async fn apply(
        &self,
        kind: DeclarationKind,
        declarations: &[Declaration],
        at: OffsetDateTime,
    ) -> PortResult<()> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("declaration", error))?;

        // Delete-then-insert inside one transaction, rather than an upsert plus a
        // sweep of what is missing. A withdrawn trust policy has to *stop* being
        // stored — an upsert-only store would go on reporting it as present for
        // ever — and doing it in two statements outside a transaction would leave a
        // window where a concurrent pass sees the file as empty.
        sqlx::query("DELETE FROM managed_declarations WHERE kind = $1")
            .bind(kind.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("declaration", error))?;

        for declaration in declarations {
            sqlx::query(
                "INSERT INTO managed_declarations (kind, key, snapshot, applied_at)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(kind.as_str())
            .bind(&declaration.key)
            .bind(&declaration.snapshot)
            .bind(at)
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("declaration", error))?;
        }

        transaction
            .commit()
            .await
            .map_err(|error| fail("declaration", error))?;

        Ok(())
    }
}
