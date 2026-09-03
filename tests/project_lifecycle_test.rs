//! One project, from creation to completion, through the pieces that actually run
//! it: the store, the scratchpad, and the `project_*` tools.
//!
//! What this is really guarding is the loop's memory. A project is a hundred fresh
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
use metalcraft_agent::project_tick::{self, TickKind};
use metalcraft_agent::projects::{self, Project, ProjectKind, ProjectStatus};
use metalcraft_agent::tools::project as project_tools;

fn a_project(id: &str) -> Project {
    Project {
        id: id.into(),
        title: "Billing".into(),
        goal: "Ship Stripe billing in rust4ai/foo".into(),
        kind: ProjectKind::Build,
        instance_id: "inst_test".into(),
        conductor_instance_id: String::new(),
        worker_brief: String::new(),
        tick_requested: false,
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
async fn a_project_lives_through_its_scratchpad() {
    let data_dir = std::env::temp_dir().join(format!("mc-agent-project-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    fs::create_dir_all(&data_dir).unwrap();

    // ── created ──────────────────────────────────────────────────────────────
    let project = a_project(&projects::new_id());
    projects::save(&project).expect("save");
    projects::write_scratchpad(&project.id, &projects::seed_scratchpad(&project)).expect("seed");

    let loaded = projects::get(&project.id).expect("round-trips");
    assert_eq!(loaded.goal, project.goal);
    assert_eq!(projects::active_count(), 1);

    // A project with no plan yet plans, and is due immediately — nobody who just
    // asked for something wants to wait half an hour for it to start.
    let pad = projects::read_scratchpad(&project.id).unwrap();
    assert!(pad.contains("Ship Stripe billing"), "the project is in its own scratchpad");
    assert_eq!(project_tick::tick_kind(&loaded, &pad, &[]), TickKind::Plan);
    assert!(project_tick::is_due(&loaded, chrono::Utc::now()));
    assert_eq!(project_tick::due(chrono::Utc::now()).len(), 1);

    // ── the planning tick writes a plan ──────────────────────────────────────
    let write = project_tools::ProjectScratchpadWriteTool::new(project.id.clone());
    let planned = projects::replace_section(
        &pad,
        "Plan",
        "- [ ] 1. Schema + migration\n- [ ] 2. Checkout endpoint",
    );
    write
        .call(serde_json::json!({ "markdown": planned }))
        .await
        .expect("plan written");

    let pad = projects::read_scratchpad(&project.id).unwrap();
    assert_eq!(projects::progress_of(&pad), projects::Progress { done: 0, total: 2 });
    assert_eq!(project_tick::tick_kind(&loaded, &pad, &[]), TickKind::Work);
    // the previous version is kept — grooming is the one op that can destroy a project
    assert!(!projects::snapshots(&project.id).is_empty(), "the write snapshotted");

    // ── a work tick logs what it did ─────────────────────────────────────────
    let note = project_tools::ProjectNoteTool::new(project.id.clone());
    note.call(serde_json::json!({ "section": "Log", "text": "t1: migration 0004, pushed" }))
        .await
        .expect("note");
    let pad = projects::read_scratchpad(&project.id).unwrap();
    assert!(projects::section_body(&pad, "Log").unwrap().contains("migration 0004"));
    assert_eq!(
        projects::progress_of(&pad),
        projects::Progress { done: 0, total: 2 },
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
    let complete = project_tools::ProjectCompleteTool::new(project.id.clone());
    let err = complete
        .call(serde_json::json!({ "summary": "all done!" }))
        .await
        .expect_err("a project with unchecked steps cannot be complete");
    assert!(
        format!("{err}").contains("still open"),
        "the refusal has to say what is still owed: {err}"
    );
    assert_eq!(
        projects::get(&project.id).unwrap().status,
        ProjectStatus::Active,
        "a refused completion leaves the project running"
    );

    // ── blocking stops the heartbeat and records the question ────────────────
    let block = project_tools::ProjectBlockTool::new(project.id.clone());
    block
        .call(serde_json::json!({
            "question": "Stripe test key or live key?",
            "reason": "spends money"
        }))
        .await
        .expect("block");

    let blocked = projects::get(&project.id).unwrap();
    assert_eq!(blocked.status, ProjectStatus::Blocked);
    assert!(blocked.blocked_reason.as_deref().unwrap().contains("test key or live key"));
    assert!(!project_tick::is_due(&blocked, chrono::Utc::now()), "a blocked project never ticks");
    assert!(project_tick::due(chrono::Utc::now()).is_empty());
    assert!(
        projects::read_scratchpad(&project.id)
            .unwrap()
            .contains("test key or live key"),
        "the question is in the document the next tick will read"
    );

    // ── unblocked, it finishes ───────────────────────────────────────────────
    let mut resumed = projects::get(&project.id).unwrap();
    resumed.status = ProjectStatus::Active;
    resumed.blocked_reason = None;
    projects::save(&resumed).unwrap();

    let pad = projects::read_scratchpad(&project.id).unwrap();
    let done = projects::replace_section(
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

    let finished = projects::get(&project.id).unwrap();
    assert_eq!(finished.status, ProjectStatus::Done);
    assert!(!project_tick::is_due(&finished, chrono::Utc::now()));
    assert_eq!(projects::active_count(), 0);
    assert_eq!(finished.progress(), projects::Progress { done: 2, total: 2 });

    // ── a rewrite that drops the project statement gets it back ─────────────────
    // The one thing a scratchpad may not lose: without it every later tick works
    // towards nothing in particular, and the document still looks well-formed.
    write
        .call(serde_json::json!({ "markdown": "## Plan\n- [x] 1. Schema + migration\n- [x] 2. Checkout endpoint\n" }))
        .await
        .unwrap();
    assert!(
        projects::read_scratchpad(&project.id)
            .unwrap()
            .contains("Ship Stripe billing"),
        "the project statement is restored rather than lost"
    );

    // ── a long build is handed to the heartbeat, not waited on ───────────────
    let mut running = a_project(&projects::new_id());
    running.title = "Awaiting".into();
    projects::save(&running).unwrap();
    projects::write_scratchpad(&running.id, &projects::seed_scratchpad(&running)).unwrap();

    let await_run = project_tools::ProjectAwaitRunTool::new(running.id.clone());
    await_run
        .call(serde_json::json!({
            "workspace_id": "ws_7",
            "run_id": "run_42",
            "what": "cargo test"
        }))
        .await
        .expect("handed over");

    let waiting = projects::get(&running.id).unwrap();
    let pending = waiting.pending_run.as_ref().expect("recorded");
    assert_eq!(pending.run_id, "run_42");
    assert_eq!(
        waiting.tick_interval_minutes(),
        projects::MIN_HEARTBEAT_MINUTES,
        "a project waiting on a run looks again soon, not in half an hour"
    );
    assert!(
        projects::read_scratchpad(&running.id).unwrap().contains("run_42"),
        "the handover is in the document too, in case the record and the pad disagree"
    );

    // A second one is refused: a project that started three builds and remembered
    // one would silently lose the other two.
    let second = await_run
        .call(serde_json::json!({ "workspace_id": "ws_7", "run_id": "run_43", "what": "cargo build" }))
        .await
        .expect_err("one at a time");
    assert!(format!("{second}").contains("run_42"), "{second}");
    projects::delete(&running.id).unwrap();

    // ── the journal is what a person reads ───────────────────────────────────
    project_tick::append_journal(
        &project.id,
        &project_tick::JournalEntry {
            at: chrono::Utc::now().to_rfc3339(),
            tick: 1,
            kind: TickKind::Work,
            model: "gpt-5.4".into(),
            summary: "Wrote the migration.".into(),
            status: ProjectStatus::Active,
            plan_done: 1,
            plan_total: 2,
            progressed: true,
            duration_secs: 42,
            briefing: Some("Take the migration step; the schema decision is already made.".into()),
            conducted: true,
        },
    );
    let entries = project_tick::read_journal(&project.id, 50);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tick, 1);
    assert!(entries[0].progressed);
    // The briefing is on the record: the first question about a tick that went
    // wrong is what it was actually told to do.
    assert!(entries[0].briefing.as_deref().unwrap_or_default().contains("migration"));
    assert!(entries[0].conducted);

    // ── an audit project keeps a ledger, and the cap is a rail not a hope ───────
    let mut audit = a_project(&projects::new_id());
    audit.kind = ProjectKind::Audit;
    audit.title = "Audit".into();
    audit.rails.max_open_prs = 2;
    projects::save(&audit).unwrap();

    let finding = project_tools::ProjectFindingTool::new(audit.id.clone());
    let update = project_tools::ProjectFindingUpdateTool::new(audit.id.clone());

    let first = finding
        .call(serde_json::json!({
            "title": "Unwrap on a None",
            "file": "src/lib.rs:42",
            "severity": "high",
            "detail": "…"
        }))
        .await
        .unwrap();
    assert_eq!(first["already_known"], false);
    let id = first["id"].as_str().unwrap().to_string();

    // The same thing, worded differently on a later sweep. This is the whole
    // reason the ledger exists: two PRs for one bug is how a repo learns to
    // ignore the sender.
    let again = finding
        .call(serde_json::json!({
            "title": "  unwrap on a none.  ",
            "file": "src/lib.rs:42",
            "severity": "high"
        }))
        .await
        .unwrap();
    assert_eq!(again["already_known"], true);
    assert_eq!(again["id"], serde_json::json!(id));
    assert_eq!(metalcraft_agent::project_findings::list(&audit.id).len(), 1);

    // Fill the PR slots, then be refused the third — in code, not in a prompt.
    for (title, file) in [("Missing bound", "src/a.rs:1"), ("Dead branch", "src/b.rs:2")] {
        finding
            .call(serde_json::json!({ "title": title, "file": file, "severity": "low" }))
            .await
            .unwrap();
    }
    let ids: Vec<String> = metalcraft_agent::project_findings::list(&audit.id)
        .iter()
        .map(|f| f.id.clone())
        .collect();
    for id in ids.iter().take(2) {
        update
            .call(serde_json::json!({ "id": id, "state": "pr_open", "link": "http://pr" }))
            .await
            .expect("within the cap");
    }
    let refused = update
        .call(serde_json::json!({ "id": ids[2], "state": "pr_open" }))
        .await
        .expect_err("the cap holds");
    assert!(format!("{refused}").contains("limit"), "{refused}");

    // A merged PR gives its slot back; an issue never took one.
    update
        .call(serde_json::json!({ "id": &ids[0], "state": "merged" }))
        .await
        .unwrap();
    update
        .call(serde_json::json!({ "id": &ids[2], "state": "pr_open" }))
        .await
        .expect("a merged PR freed a slot");
    assert_eq!(metalcraft_agent::project_findings::open_prs(&audit.id), 2);

    // And the agent can see all of it next tick.
    let rendered = metalcraft_agent::project_findings::render(&audit.id);
    assert!(rendered.contains("Unwrap on a None"), "{rendered}");
    assert!(rendered.contains("Merged"), "{rendered}");
    projects::delete(&audit.id).unwrap();

    // ── forcing a tick is a request, not a preemption ────────────────────────
    let mut forced = projects::get(&project.id).unwrap();
    forced.status = ProjectStatus::Active;
    forced.counters.last_tick_at = Some(chrono::Utc::now().to_rfc3339());
    forced.tick_requested = false;
    assert!(
        !project_tick::is_due(&forced, chrono::Utc::now()),
        "a project that just ticked is not due"
    );

    forced.tick_requested = true;
    assert!(
        project_tick::is_due(&forced, chrono::Utc::now()),
        "but a forced one is, whatever its bookmark says"
    );

    // A paused project that is forced stays paused: "run now" is about WHEN, not
    // about overriding a decision somebody already took.
    forced.status = ProjectStatus::Paused;
    assert!(!project_tick::is_due(&forced, chrono::Utc::now()));

    // ── deleted ──────────────────────────────────────────────────────────────
    projects::delete(&project.id).expect("delete");
    assert!(projects::get(&project.id).is_none());
    assert!(projects::read_scratchpad(&project.id).is_none(), "its document went with it");
    assert!(project_tick::read_journal(&project.id, 50).is_empty());
}
