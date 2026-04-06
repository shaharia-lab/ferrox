use std::time::Duration;

use tracing::{error, info, warn};

use crate::db::audit_repo::AuditRepository;
use crate::db::client_repo::ClientRepository;
use crate::db::models::AuditEvent;

/// Spawn the periodic budget enforcement background task.
///
/// Every `interval`, the task:
/// 1. Resets budget periods that have expired (daily/monthly rollover)
/// 2. Finds active clients whose token usage exceeds their budget
/// 3. Revokes those clients and writes `BudgetExceeded` audit entries
pub fn spawn_budget_checker(db: sqlx::PgPool, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // skip the immediate first tick
        loop {
            ticker.tick().await;
            if let Err(e) = run_budget_check(&db).await {
                error!(error = %e, "budget checker failed");
            }
        }
    });
}

async fn run_budget_check(db: &sqlx::PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let client_repo = ClientRepository::new(db);
    let audit_repo = AuditRepository::new(db);

    // Step 1: Reset expired budget periods (daily/monthly rollover).
    match client_repo.reset_expired_budgets().await {
        Ok(0) => {}
        Ok(n) => info!(count = n, "budget checker: reset expired budget periods"),
        Err(e) => warn!(error = %e, "budget checker: failed to reset budget periods"),
    }

    // Step 2: Find and revoke over-budget clients.
    let over_budget = client_repo.find_over_budget().await?;

    for client in &over_budget {
        info!(
            client = %client.name,
            client_id = %client.id,
            budget = ?client.token_budget,
            used = client.tokens_used,
            "budget checker: revoking over-budget client"
        );

        if let Err(e) = client_repo.revoke(client.id).await {
            error!(
                client_id = %client.id,
                error = %e,
                "budget checker: failed to revoke client"
            );
            continue;
        }

        let meta = serde_json::json!({
            "token_budget": client.token_budget,
            "tokens_used": client.tokens_used,
            "reason": "token budget exceeded"
        });
        if let Err(e) = audit_repo
            .record(Some(client.id), &AuditEvent::BudgetExceeded, Some(&meta))
            .await
        {
            error!(
                client_id = %client.id,
                error = %e,
                "budget checker: failed to write audit entry"
            );
        }
    }

    if !over_budget.is_empty() {
        info!(
            count = over_budget.len(),
            "budget checker: revoked over-budget clients"
        );
    }

    Ok(())
}
