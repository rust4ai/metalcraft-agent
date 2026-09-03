//! A project's plan, from an empty document to a finished one, through the tools a
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
use metalcraft_agent::project_tasks::{self, TaskStatus};
use metalcraft_agent::projects::{self, Project, ProjectKind, ProjectStatus};
use metalcraft_agent::tools::project as goal_tools;
use metalcraft_agent::tools::task as task_tools;

fn a_goal(id: &str) -> Project {
    Project {
        id: id.into(),
        title: "Limiter".into(),
        goal: "Ship the token-bucket limiter in rust4ai/foo".into(),
        kind: ProjectKind::Build,
        instance_id: "inst_test".into(),
        agent_preset: "general-agent".into(),
        workspace: projects::Workspace::default(),
        status: ProjectStatus::Active,
        blocked_reason: None,
        heartbeat: projects::Heartbeat::default(),
        io: metalcraft_agent::scheduled_tasks::IoBinding::Unbound,
        journal_chat_id: None,
        rails: projects::Rails::default(),
        counters: projects::Counters::default(),
        pending_run: None,
        models: projects::ModelTiers::default(),
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

    let project = a_goal(&projects::new_id());
    projects::save(&project).expect("save");
    projects::write_scratchpad(&project.id, &projects::seed_scratchpad(&project)).expect("seed");

    // ── a project with no tasks still reads as unplanned ────────────────────────
    assert!(!project_tasks::exists(&project.id));
    assert_eq!(project.progress(), projects::Progress { done: 0, total: 0 });

    // ── the planning tick writes the whole plan in one call ──────────────────
    let add = task_tools::TaskAddTool::new(project.id.clone());
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

    let tasks = project_tasks::list(&project.id);
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[2].deps, vec!["t1".to_string(), "t2".to_string()]);
    assert_eq!(tasks[2].assignee.as_deref(), Some("coding-agent"));
    // The safe default: a task nobody said anything about is assumed to write.
    assert!(tasks[2].mutates_workspace);
    assert!(!tasks[0].mutates_workspace);

    // ── the two independent rows are ready AT THE SAME TIME ──────────────────
    // This is the parallelism the whole design is for: one tick, two delegates.
    let ready = project_tasks::ready(&tasks);
    assert_eq!(
        ready.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        vec!["t1", "t2"],
        "independent tasks must be startable together"
    );

    // ── the plan the tick is shown is rendered from the records ──────────────
    let frame = metalcraft_agent::project_tick::tick_frame(
        &projects::get(&project.id).unwrap(),
        metalcraft_agent::project_tick::TickKind::Work,
        2,
    );
    assert!(frame.contains("**t1** [ready]"), "{frame}");
    assert!(frame.contains("**t3** [todo] Write the token bucket (after t1, t2)"), "{frame}");
    assert!(
        !frame.contains("No plan yet"),
        "the rendered list must replace the placeholder, not sit beside it"
    );

    // ── done needs proof ─────────────────────────────────────────────────────
    let done = task_tools::TaskDoneTool::new(project.id.clone());
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
    let after = project_tasks::list(&project.id);
    assert_eq!(after.len(), 3);
    assert_eq!(after[0].status, TaskStatus::Done);
    assert_eq!(after[1].status, TaskStatus::Todo);
    assert_eq!(after[2].status, TaskStatus::Todo);
    assert_eq!(after[2].deps, vec!["t1".to_string(), "t2".to_string()]);
    assert_eq!(projects::get(&project.id).unwrap().progress(), projects::Progress { done: 1, total: 3 });

    // ── a task parks without stopping the project ───────────────────────────────
    let block = task_tools::TaskBlockTool::new(project.id.clone());
    block
        .call(serde_json::json!({ "id": "t2", "reason": "needs the staging API key" }))
        .await
        .expect("one task can be parked");
    assert_eq!(
        projects::get(&project.id).unwrap().status,
        ProjectStatus::Active,
        "parking a task must not stop the project — that is what project_block is for"
    );
    assert!(
        projects::read_scratchpad(&project.id)
            .unwrap()
            .contains("staging API key"),
        "a blocked task is visible to someone reading the project"
    );

    // ── a run is owed by one task, and only that task waits ──────────────────
    let await_run = goal_tools::ProjectAwaitRunTool::new(project.id.clone());
    await_run
        .call(serde_json::json!({
            "workspace_id": "ws_1", "run_id": "r_88", "what": "cargo test --all", "task_id": "t3"
        }))
        .await
        .expect("a task can own its run");
    let waiting = project_tasks::list(&project.id);
    let t3 = project_tasks::get(&waiting, "t3").unwrap();
    assert_eq!(t3.status, TaskStatus::Waiting);
    assert_eq!(t3.pending_run.unwrap().run_id, "r_88");
    assert!(
        projects::get(&project.id).unwrap().pending_run.is_none(),
        "a task's run is the task's, not the project's"
    );
    // The project looks again soon because something of its is running.
    assert_eq!(projects::get(&project.id).unwrap().tick_interval_minutes(), 5);

    // ── a gate refuses a task whose run has not gone green ───────────────────
    assert!(
        done.call(serde_json::json!({
            "id": "t3", "evidence_kind": "commit", "evidence": "deadbee"
        }))
        .await
        .is_err(),
        "a gated task cannot be closed before its gate passes"
    );

    // ── the project cannot say it is done while its plan is open ────────────────
    let complete = goal_tools::ProjectCompleteTool::new(project.id.clone());
    let e = complete
        .call(serde_json::json!({ "summary": "shipped it" }))
        .await
        .expect_err("open tasks refuse a completion");
    assert!(format!("{e}").contains("still open"), "{e}");

    // ── dropping what reality made pointless frees what waited on it ─────────
    let drop = task_tools::TaskDropTool::new(project.id.clone());
    drop.call(serde_json::json!({
        "id": "t2", "why": "the middleware was deleted upstream"
    }))
    .await
    .expect("a review tick prunes");
    let pruned = project_tasks::list(&project.id);
    assert_eq!(project_tasks::progress(&pruned), projects::Progress { done: 1, total: 2 });

    // ── a cycle is refused rather than wedging the project forever ──────────────
    let update = task_tools::TaskUpdateTool::new(project.id.clone());
    assert!(
        update
            .call(serde_json::json!({ "id": "t1", "deps": ["t3"] }))
            .await
            .is_err(),
        "t1 → t3 → t1 would mean nothing could ever start"
    );
    // ...and the refusal changed nothing.
    assert!(project_tasks::get(&project_tasks::list(&project.id), "t1").unwrap().deps.is_empty());

    // ── dispatch guards, before a single token is spent ──────────────────────
    // The delegation itself needs a live API, but everything that protects the
    // workspace and the plan happens first — so that part is testable, and it is
    // the part that fails silently if it is wrong.
    let cfg = metalcraft_agent::tools::ToolConfig {
        api_key: "test-key".into(),
        model_name: "test-model".into(),
        system_prompt: String::new(),
        skills_dir: data_dir.join("skills"),
        available_skills: Vec::new(),
        reply_sink: None,
        session_binding: None,
        reschedule_depth: 0,
        preset_personas: None,
        sub_agent_depth: 0,
        instance_id: None,
        interrupt: None,
        turn_plan: None,
        project_id: Some(project.id.clone()),
    };
    let dispatch = task_tools::TaskDispatchTool::new(project.id.clone(), &cfg);

    let e = dispatch
        .call(serde_json::json!({ "ids": ["t3"] }))
        .await
        .expect_err("a task still waiting on a run is not ready");
    assert!(format!("{e}").contains("not ready"), "{e}");

    let e = dispatch
        .call(serde_json::json!({ "ids": ["t_nope"] }))
        .await
        .expect_err("an id that is not a task is refused");
    assert!(format!("{e}").contains("no task"), "{e}");

    // Two rows that both write: refused, because there is one workspace and two
    // agents editing it at once overwrite each other without either noticing.
    // (One writer alongside readers is allowed — a reader seeing a file mid-edit
    // is imprecise, not corrupting.)
    add.call(serde_json::json!({
        "tasks": [
            { "title": "Read the changelog", "mutates_workspace": false },
            { "title": "Rewrite the changelog", "mutates_workspace": true },
            { "title": "Regenerate the fixtures", "mutates_workspace": true },
        ]
    }))
    .await
    .expect("three more tasks");
    let e = dispatch
        .call(serde_json::json!({ "ids": ["t5", "t6"] }))
        .await
        .expect_err("two writers cannot run at the same time");
    let msg = format!("{e}");
    assert!(msg.contains("t5") && msg.contains("t6"), "name them both: {msg}");
    assert!(msg.contains("only one"), "{msg}");

    let _ = fs::remove_dir_all(&data_dir);
}
