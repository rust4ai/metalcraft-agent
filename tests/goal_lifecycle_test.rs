//! One goal, from creation to completion, through the pieces that actually run
//! it: the store, the scratchpad, and the four `goal_*` tools.
//!
//! What this is really guarding is the loop's memory. A goal is a hundred fresh
//! conversations that share nothing but one markdown file, so if the tools do
//! not write that file — or a rewrite quietly loses the plan — every tick after
//! it works on the wrong thing while still looking busy. That failure is
//! invisible from the outside, which is why it is worth a test that reads the
//! document back after every write.
//!
//! **One test function on purpose.** `paths::data_dir()` caches
//! `METALCRAFT_DATA_DIR` in a `OnceLock`, so two `#[test]`s in one binary would
//! silently share whichever dir was set first — the same reason
//! `agent_instance_lifecycle` and `memory_layers_test` are each one test.

use std::fs;

use metalcraft::Tool;
use metalcraft_agent::goal_tick::{self, TickKind};
use metalcraft_agent::goals::{self, Goal, GoalKind, GoalStatus};
use metalcraft_agent::tools::goal as goal_tools;

fn a_goal(id: &str) -> Goal {
    Goal {
        id: id.into(),
        title: "Billing".into(),
        goal: "Ship Stripe billing in rust4ai/foo".into(),
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
async fn a_goal_lives_through_its_scratchpad() {
    let data_dir = std::env::temp_dir().join(format!("mc-agent-goal-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    fs::create_dir_all(&data_dir).unwrap();

    // ── created ──────────────────────────────────────────────────────────────
    let goal = a_goal(&goals::new_id());
    goals::save(&goal).expect("save");
    goals::write_scratchpad(&goal.id, &goals::seed_scratchpad(&goal)).expect("seed");

    let loaded = goals::get(&goal.id).expect("round-trips");
    assert_eq!(loaded.goal, goal.goal);
    assert_eq!(goals::active_count(), 1);

    // A goal with no plan yet plans, and is due immediately — nobody who just
    // asked for something wants to wait half an hour for it to start.
    let pad = goals::read_scratchpad(&goal.id).unwrap();
    assert!(pad.contains("Ship Stripe billing"), "the goal is in its own scratchpad");
    assert_eq!(goal_tick::tick_kind(&loaded, &pad), TickKind::Plan);
    assert!(goal_tick::is_due(&loaded, chrono::Utc::now()));
    assert_eq!(goal_tick::due(chrono::Utc::now()).len(), 1);

    // ── the planning tick writes a plan ──────────────────────────────────────
    let write = goal_tools::GoalScratchpadWriteTool::new(goal.id.clone());
    let planned = goals::replace_section(
        &pad,
        "Plan",
        "- [ ] 1. Schema + migration\n- [ ] 2. Checkout endpoint",
    );
    write
        .call(serde_json::json!({ "markdown": planned }))
        .await
        .expect("plan written");

    let pad = goals::read_scratchpad(&goal.id).unwrap();
    assert_eq!(goals::progress_of(&pad), goals::Progress { done: 0, total: 2 });
    assert_eq!(goal_tick::tick_kind(&loaded, &pad), TickKind::Work);
    // the previous version is kept — grooming is the one op that can destroy a goal
    assert!(!goals::snapshots(&goal.id).is_empty(), "the write snapshotted");

    // ── a work tick logs what it did ─────────────────────────────────────────
    let note = goal_tools::GoalNoteTool::new(goal.id.clone());
    note.call(serde_json::json!({ "section": "Log", "text": "t1: migration 0004, pushed" }))
        .await
        .expect("note");
    let pad = goals::read_scratchpad(&goal.id).unwrap();
    assert!(goals::section_body(&pad, "Log").unwrap().contains("migration 0004"));
    assert_eq!(
        goals::progress_of(&pad),
        goals::Progress { done: 0, total: 2 },
        "a note must not disturb the plan"
    );

    // A note may not be aimed at the plan: appending to a plan is how a plan
    // grows a second copy of itself.
    assert!(
        note.call(serde_json::json!({ "section": "Plan", "text": "- [ ] sneak this in" }))
            .await
            .is_err()
    );

    // ── completing early is refused ──────────────────────────────────────────
    let complete = goal_tools::GoalCompleteTool::new(goal.id.clone());
    let err = complete
        .call(serde_json::json!({ "summary": "all done!" }))
        .await
        .expect_err("a goal with unchecked steps cannot be complete");
    assert!(
        format!("{err}").contains("unchecked"),
        "the refusal has to say what is still owed: {err}"
    );
    assert_eq!(
        goals::get(&goal.id).unwrap().status,
        GoalStatus::Active,
        "a refused completion leaves the goal running"
    );

    // ── blocking stops the heartbeat and records the question ────────────────
    let block = goal_tools::GoalBlockTool::new(goal.id.clone());
    block
        .call(serde_json::json!({
            "question": "Stripe test key or live key?",
            "reason": "spends money"
        }))
        .await
        .expect("block");

    let blocked = goals::get(&goal.id).unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked);
    assert!(blocked.blocked_reason.as_deref().unwrap().contains("test key or live key"));
    assert!(!goal_tick::is_due(&blocked, chrono::Utc::now()), "a blocked goal never ticks");
    assert!(goal_tick::due(chrono::Utc::now()).is_empty());
    assert!(
        goals::read_scratchpad(&goal.id)
            .unwrap()
            .contains("test key or live key"),
        "the question is in the document the next tick will read"
    );

    // ── unblocked, it finishes ───────────────────────────────────────────────
    let mut resumed = goals::get(&goal.id).unwrap();
    resumed.status = GoalStatus::Active;
    resumed.blocked_reason = None;
    goals::save(&resumed).unwrap();

    let pad = goals::read_scratchpad(&goal.id).unwrap();
    let done = goals::replace_section(
        &pad,
        "Plan",
        "- [x] 1. Schema + migration\n- [x] 2. Checkout endpoint",
    );
    write
        .call(serde_json::json!({ "markdown": done }))
        .await
        .unwrap();
    complete
        .call(serde_json::json!({ "summary": "Billing ships; reconciliation deliberately left out." }))
        .await
        .expect("now it may complete");

    let finished = goals::get(&goal.id).unwrap();
    assert_eq!(finished.status, GoalStatus::Done);
    assert!(!goal_tick::is_due(&finished, chrono::Utc::now()));
    assert_eq!(goals::active_count(), 0);
    assert_eq!(finished.progress(), goals::Progress { done: 2, total: 2 });

    // ── a rewrite that drops the goal statement gets it back ─────────────────
    // The one thing a scratchpad may not lose: without it every later tick works
    // towards nothing in particular, and the document still looks well-formed.
    write
        .call(serde_json::json!({ "markdown": "## Plan\n- [x] 1. Schema + migration\n- [x] 2. Checkout endpoint\n" }))
        .await
        .unwrap();
    assert!(
        goals::read_scratchpad(&goal.id)
            .unwrap()
            .contains("Ship Stripe billing"),
        "the goal statement is restored rather than lost"
    );

    // ── a long build is handed to the heartbeat, not waited on ───────────────
    let mut running = a_goal(&goals::new_id());
    running.title = "Awaiting".into();
    goals::save(&running).unwrap();
    goals::write_scratchpad(&running.id, &goals::seed_scratchpad(&running)).unwrap();

    let await_run = goal_tools::GoalAwaitRunTool::new(running.id.clone());
    await_run
        .call(serde_json::json!({
            "workspace_id": "ws_7",
            "run_id": "run_42",
            "what": "cargo test"
        }))
        .await
        .expect("handed over");

    let waiting = goals::get(&running.id).unwrap();
    let pending = waiting.pending_run.as_ref().expect("recorded");
    assert_eq!(pending.run_id, "run_42");
    assert_eq!(
        waiting.tick_interval_minutes(),
        goals::MIN_HEARTBEAT_MINUTES,
        "a goal waiting on a run looks again soon, not in half an hour"
    );
    assert!(
        goals::read_scratchpad(&running.id).unwrap().contains("run_42"),
        "the handover is in the document too, in case the record and the pad disagree"
    );

    // A second one is refused: a goal that started three builds and remembered
    // one would silently lose the other two.
    let second = await_run
        .call(serde_json::json!({ "workspace_id": "ws_7", "run_id": "run_43", "what": "cargo build" }))
        .await
        .expect_err("one at a time");
    assert!(format!("{second}").contains("run_42"), "{second}");
    goals::delete(&running.id).unwrap();

    // ── the journal is what a person reads ───────────────────────────────────
    goal_tick::append_journal(
        &goal.id,
        &goal_tick::JournalEntry {
            at: chrono::Utc::now().to_rfc3339(),
            tick: 1,
            kind: TickKind::Work,
            model: "gpt-5.4".into(),
            summary: "Wrote the migration.".into(),
            status: GoalStatus::Active,
            plan_done: 1,
            plan_total: 2,
            progressed: true,
            duration_secs: 42,
        },
    );
    let entries = goal_tick::read_journal(&goal.id, 50);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tick, 1);
    assert!(entries[0].progressed);

    // ── deleted ──────────────────────────────────────────────────────────────
    goals::delete(&goal.id).expect("delete");
    assert!(goals::get(&goal.id).is_none());
    assert!(goals::read_scratchpad(&goal.id).is_none(), "its document went with it");
    assert!(goal_tick::read_journal(&goal.id, 50).is_empty());
}
