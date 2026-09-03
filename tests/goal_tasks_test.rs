//! A goal's plan, from an empty document to a finished one, through the tools a
//! tick actually calls.
//!
//! What this guards is the promise that made tasks worth building: **a plan
//! cannot be silently lost, and a step cannot be closed without evidence.** The
//! old plan was markdown the model rewrote every tick, so a dropped row looked
//! exactly like a plan that never had it. Here the model only ever names the row
//! it means, so this test reads the list back after every write and checks the
//! rows nobody mentioned are still exactly as they were.
//!
//! It also pins the parallelism: rows with no dependencies are ready *at the
//! same time*, and a task waiting on a build does not stop its siblings.
//!
//! **One test function on purpose.** `paths::data_dir()` caches
//! `METALCRAFT_DATA_DIR` in a `OnceLock`, so two `#[test]`s in one binary would
//! silently share whichever dir was set first — the same reason
//! `goal_lifecycle_test` is one test.

use std::fs;

use metalcraft::Tool;
use metalcraft_agent::goal_tasks::{self, TaskStatus};
use metalcraft_agent::goals::{self, Goal, GoalKind, GoalStatus};
use metalcraft_agent::tools::goal as goal_tools;
use metalcraft_agent::tools::goal_task as task_tools;

fn a_goal(id: &str) -> Goal {
    Goal {
        id: id.into(),
        title: "Limiter".into(),
        goal: "Ship the token-bucket limiter in rust4ai/foo".into(),
        kind: GoalKind::Build,
        instance_id: "inst_test".into(),
        agent_preset: "general-agent".into(),
        workspace: goals::Workspace::default(),
        status: GoalStatus::Active,
        blocked_reason: None,
        heartbeat: goals::Heartbeat::default(),
        io: metalcraft_agent::scheduled_tasks::IoBinding::Unbound,
        journal_chat_id: None,
        rails: goals::Rails::default(),
        counters: goals::Counters::default(),
        pending_run: None,
        models: goals::ModelTiers::default(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: String::new(),
    }
}

#[tokio::test]
async fn a_plan_is_records_that_cannot_be_lost() {
    let data_dir = std::env::temp_dir().join(format!("mc-agent-tasks-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    fs::create_dir_all(&data_dir).unwrap();

    let goal = a_goal(&goals::new_id());
    goals::save(&goal).expect("save");
    goals::write_scratchpad(&goal.id, &goals::seed_scratchpad(&goal)).expect("seed");

    // ── a goal with no tasks still reads as unplanned ────────────────────────
    assert!(!goal_tasks::exists(&goal.id));
    assert_eq!(goal.progress(), goals::Progress { done: 0, total: 0 });

    // ── the planning tick writes the whole plan in one call ──────────────────
    let add = task_tools::TaskAddTool::new(goal.id.clone());
    add.call(serde_json::json!({
        "tasks": [
            { "title": "Survey the existing limiter", "mutates_workspace": false },
            { "title": "Read how the middleware calls it", "mutates_workspace": false },
            // Depends on both surveys by *index*, because a plan describes its
            // own shape before its rows have ids.
            { "title": "Write the token bucket", "deps": ["0", "1"], "assignee": "coding-agent",
              "gate": "cargo test --all" },
        ]
    }))
    .await
    .expect("a plan is written in one call");

    let tasks = goal_tasks::list(&goal.id);
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[2].deps, vec!["t1".to_string(), "t2".to_string()]);
    assert_eq!(tasks[2].assignee.as_deref(), Some("coding-agent"));
    // The safe default: a task nobody said anything about is assumed to write.
    assert!(tasks[2].mutates_workspace);
    assert!(!tasks[0].mutates_workspace);

    // ── the two independent rows are ready AT THE SAME TIME ──────────────────
    // This is the parallelism the whole design is for: one tick, two delegates.
    let ready = goal_tasks::ready(&tasks);
    assert_eq!(
        ready.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        vec!["t1", "t2"],
        "independent tasks must be startable together"
    );

    // ── the plan the tick is shown is rendered from the records ──────────────
    let frame = metalcraft_agent::goal_tick::tick_frame(
        &goals::get(&goal.id).unwrap(),
        metalcraft_agent::goal_tick::TickKind::Work,
        2,
    );
    assert!(frame.contains("**t1** [ready]"), "{frame}");
    assert!(frame.contains("**t3** [todo] Write the token bucket (after t1, t2)"), "{frame}");
    assert!(
        !frame.contains("No plan yet"),
        "the rendered list must replace the placeholder, not sit beside it"
    );

    // ── done needs proof ─────────────────────────────────────────────────────
    let done = task_tools::TaskDoneTool::new(goal.id.clone());
    assert!(
        done.call(serde_json::json!({ "id": "t1", "evidence_kind": "note", "evidence": "" }))
            .await
            .is_err(),
        "a task cannot be closed on an empty claim"
    );
    done.call(serde_json::json!({
        "id": "t1", "evidence_kind": "commit", "evidence": "a1b2c3d"
    }))
    .await
    .expect("with proof it closes");

    // Everything nobody mentioned is untouched — the property the markdown plan
    // could never offer.
    let after = goal_tasks::list(&goal.id);
    assert_eq!(after.len(), 3);
    assert_eq!(after[0].status, TaskStatus::Done);
    assert_eq!(after[1].status, TaskStatus::Todo);
    assert_eq!(after[2].status, TaskStatus::Todo);
    assert_eq!(after[2].deps, vec!["t1".to_string(), "t2".to_string()]);
    assert_eq!(goals::get(&goal.id).unwrap().progress(), goals::Progress { done: 1, total: 3 });

    // ── a task parks without stopping the goal ───────────────────────────────
    let block = task_tools::TaskBlockTool::new(goal.id.clone());
    block
        .call(serde_json::json!({ "id": "t2", "reason": "needs the staging API key" }))
        .await
        .expect("one task can be parked");
    assert_eq!(
        goals::get(&goal.id).unwrap().status,
        GoalStatus::Active,
        "parking a task must not stop the goal — that is what goal_block is for"
    );
    assert!(
        goals::read_scratchpad(&goal.id)
            .unwrap()
            .contains("staging API key"),
        "a blocked task is visible to someone reading the goal"
    );

    // ── a run is owed by one task, and only that task waits ──────────────────
    let await_run = goal_tools::GoalAwaitRunTool::new(goal.id.clone());
    await_run
        .call(serde_json::json!({
            "workspace_id": "ws_1", "run_id": "r_88", "what": "cargo test --all", "task_id": "t3"
        }))
        .await
        .expect("a task can own its run");
    let waiting = goal_tasks::list(&goal.id);
    let t3 = goal_tasks::get(&waiting, "t3").unwrap();
    assert_eq!(t3.status, TaskStatus::Waiting);
    assert_eq!(t3.pending_run.unwrap().run_id, "r_88");
    assert!(
        goals::get(&goal.id).unwrap().pending_run.is_none(),
        "a task's run is the task's, not the goal's"
    );
    // The goal looks again soon because something of its is running.
    assert_eq!(goals::get(&goal.id).unwrap().tick_interval_minutes(), 5);

    // ── a gate refuses a task whose run has not gone green ───────────────────
    assert!(
        done.call(serde_json::json!({
            "id": "t3", "evidence_kind": "commit", "evidence": "deadbee"
        }))
        .await
        .is_err(),
        "a gated task cannot be closed before its gate passes"
    );

    // ── the goal cannot say it is done while its plan is open ────────────────
    let complete = goal_tools::GoalCompleteTool::new(goal.id.clone());
    let e = complete
        .call(serde_json::json!({ "summary": "shipped it" }))
        .await
        .expect_err("open tasks refuse a completion");
    assert!(format!("{e}").contains("still open"), "{e}");

    // ── dropping what reality made pointless frees what waited on it ─────────
    let drop = task_tools::TaskDropTool::new(goal.id.clone());
    drop.call(serde_json::json!({
        "id": "t2", "why": "the middleware was deleted upstream"
    }))
    .await
    .expect("a review tick prunes");
    let pruned = goal_tasks::list(&goal.id);
    assert_eq!(goal_tasks::progress(&pruned), goals::Progress { done: 1, total: 2 });

    // ── a cycle is refused rather than wedging the goal forever ──────────────
    let update = task_tools::TaskUpdateTool::new(goal.id.clone());
    assert!(
        update
            .call(serde_json::json!({ "id": "t1", "deps": ["t3"] }))
            .await
            .is_err(),
        "t1 → t3 → t1 would mean nothing could ever start"
    );
    // ...and the refusal changed nothing.
    assert!(goal_tasks::get(&goal_tasks::list(&goal.id), "t1").unwrap().deps.is_empty());

    let _ = fs::remove_dir_all(&data_dir);
}
