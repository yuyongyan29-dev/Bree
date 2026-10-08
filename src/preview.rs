//! Shared read-only plan preparation used by both the TUI and CLI.
use crate::{
    model::Snapshot,
    policy::{CleanupPlan, PolicyContext, evaluate},
    storage::Store,
};

pub fn prepare(store: &Store, snapshot: &Snapshot) -> Result<CleanupPlan, String> {
    let _execution = store.execution_lock()?;
    // Read under the execution lane rather than reusing a possibly old UI state.
    let state = store.load()?;
    let plan = evaluate(snapshot, &state, &PolicyContext { state_valid: true });
    store.append_record(
        "dry_run_prepared",
        serde_json::json!({
            "plan_id": plan.plan_id,
            "policy_version": plan.policy_version,
            "rule_revision": plan.rule_revision,
            "sampled_at_unix_ms": snapshot.sampled_at_unix_ms,
            "read_only": true,
            "automatic_count": plan.automatic_count,
            "pending_count": plan.pending_count,
            "protected_count": plan.protected_count,
        }),
    )?;
    Ok(plan)
}
