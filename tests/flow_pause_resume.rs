//! Offline end-to-end test of Phase-3 durability: a flow pauses at an `approval`
//! / `wait` node, persists a checkpoint, and resumes to completion. No LLM — the
//! approval/wait/end runners are deterministic.
//!
//! Everything is redirected into a tempdir via `METALCRAFT_DATA_DIR`, so the
//! user's real data dir is never touched.

use metalcraft_agent::flow_exec::{resume_flow, run_flow_v2};
use metalcraft_agent::paths;
use metalcraft_agent::runtime::AgentRuntimeContext;
use metalcraft_flows::{save_flow, SavedFlow};
use serde_json::json;

fn approval_flow() -> SavedFlow {
    serde_json::from_value(json!({
        "spec_version": "2",
        "id": "approval-test",
        "name": "Approval test",
        "created_at": "2026-07-27T00:00:00Z",
        "updated_at": "2026-07-27T00:00:00Z",
        "enabled": false,
        "flow": {
            "nodes": [
                { "id": "entry", "node_type": "entry", "data": { "schedule_type": "manual" } },
                { "id": "gate", "node_type": "approval", "data": { "message": "Proceed?", "choices": ["approve", "reject"] } },
                { "id": "approved", "node_type": "end", "data": { "status": "approved" } },
                { "id": "rejected", "node_type": "end", "data": { "status": "rejected" } }
            ],
            "edges": [
                { "id": "e0", "source": "entry", "target": "gate" },
                { "id": "e1", "source": "gate", "target": "approved", "source_handle": "approve" },
                { "id": "e2", "source": "gate", "target": "rejected", "source_handle": "reject" }
            ]
        }
    }))
    .unwrap()
}

fn wait_flow() -> SavedFlow {
    serde_json::from_value(json!({
        "spec_version": "2",
        "id": "wait-test",
        "name": "Wait test",
        "created_at": "2026-07-27T00:00:00Z",
        "updated_at": "2026-07-27T00:00:00Z",
        "enabled": false,
        "flow": {
            "nodes": [
                { "id": "entry", "node_type": "entry", "data": { "schedule_type": "manual" } },
                { "id": "hold", "node_type": "wait", "data": { "duration": "1h" } },
                { "id": "done", "node_type": "end", "data": { "status": "done" } }
            ],
            "edges": [
                { "id": "e0", "source": "entry", "target": "hold" },
                { "id": "e1", "source": "hold", "target": "done", "source_handle": "after" }
            ]
        }
    }))
    .unwrap()
}

fn ctx() -> AgentRuntimeContext {
    AgentRuntimeContext {
        personas_dir: paths::personas_dir(),
        skills_dir: paths::skills_dir(),
        api_key: String::new(),
    }
}

#[tokio::test]
async fn pause_and_resume_approval_and_wait() {
    let tmp = tempfile::tempdir().unwrap();
    // Redirect all data paths into the tempdir BEFORE any paths call (memoized).
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", tmp.path());
    }
    let ctx = ctx();

    // --- approval: pause, then resume with each decision ---------------------
    save_flow(&paths::flows_dir(), &approval_flow()).unwrap();

    // Approve path.
    let paused = run_flow_v2(&ctx, approval_flow(), ".", "coding-agent", "m", &json!({}))
        .await
        .unwrap();
    assert_eq!(paused.status, "paused", "trace: {:?}", paused.steps);
    assert!(!paused.run_id.is_empty());

    // The checkpoint is persisted.
    let run = metalcraft_agent::flow_runs::load_run(&paths::runs_dir(), &paused.run_id).unwrap();
    assert_eq!(run.status, "paused");
    assert_eq!(run.current_node_id, "gate");
    assert_eq!(run.pause.as_ref().unwrap().reason, "approval");
    assert_eq!(run.pause.unwrap().resume_handles, vec!["approve", "reject"]);

    let approved = resume_flow(&ctx, &paused.run_id, "approve", None).await.unwrap();
    assert_eq!(approved.status, "completed");
    assert_eq!(approved.steps.last().unwrap().node_id, "approved");
    // Resuming again is refused (no longer paused).
    assert!(resume_flow(&ctx, &paused.run_id, "approve", None).await.is_err());

    // Reject path (a fresh run of the same flow).
    let paused2 = run_flow_v2(&ctx, approval_flow(), ".", "coding-agent", "m", &json!({}))
        .await
        .unwrap();
    let rejected = resume_flow(&ctx, &paused2.run_id, "reject", None).await.unwrap();
    assert_eq!(rejected.steps.last().unwrap().node_id, "rejected");

    // --- wait: pauses with a wake_at, resumes via "after" --------------------
    save_flow(&paths::flows_dir(), &wait_flow()).unwrap();
    let waited = run_flow_v2(&ctx, wait_flow(), ".", "coding-agent", "m", &json!({}))
        .await
        .unwrap();
    assert_eq!(waited.status, "paused");
    let wrun = metalcraft_agent::flow_runs::load_run(&paths::runs_dir(), &waited.run_id).unwrap();
    assert_eq!(wrun.pause.as_ref().unwrap().reason, "wait");
    assert!(wrun.pause.unwrap().wake_at.is_some());

    let resumed = resume_flow(&ctx, &waited.run_id, "after", None).await.unwrap();
    assert_eq!(resumed.status, "completed");
    assert_eq!(resumed.steps.last().unwrap().node_id, "done");
}
