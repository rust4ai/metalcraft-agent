//! The conductor's ledger: the memory that makes a project more than a sequence
//! of strangers.
//!
//! **One test function on purpose.** `paths::data_dir()` caches
//! `METALCRAFT_DATA_DIR` in a `OnceLock`, so two `#[test]`s in one binary would
//! silently share whichever dir was set first.

use std::fs;

use metalcraft::Tool;
use metalcraft_agent::projects::{self, Project, ProjectKind, ProjectStatus};

fn a_project(id: &str) -> Project {
    Project {
        id: id.into(),
        title: "Limiter".into(),
        goal: "Ship the token-bucket limiter in rust4ai/foo".into(),
        kind: ProjectKind::Build,
        instance_id: "inst_test".into(),
        conductor_instance_id: String::new(),
        worker_brief: String::new(),
        tick_requested: false,
        session_id: String::new(),
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
async fn the_conductor_keeps_its_own_memory() {
    use metalcraft_agent::project_conductor as ledger;
    use metalcraft_agent::project_tasks::{Task, TaskStatus};
    use metalcraft_agent::tools::conductor as conductor_tools;

    let data_dir = std::env::temp_dir().join(format!("mc-conductor-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    fs::create_dir_all(&data_dir).unwrap();

    let project = a_project(&projects::new_id());
    projects::save(&project).expect("save");

    // A project with no ledger yet reads as the seed rather than as nothing —
    // the frame refers to these sections by name, so they have to exist.
    let seeded = ledger::for_injection(&project.id);
    for section in ledger::SECTIONS {
        assert!(
            projects::section_body(&seeded, section).is_some(),
            "the injected ledger is missing `{section}`"
        );
    }

    // A rewrite that drops a section is refused: the tick prompt speaks in these
    // headings, and a document without them is one nothing can act on.
    let write = conductor_tools::ConductorWriteTool::new(project.id.clone());
    let e = write
        .call(serde_json::json!({ "markdown": "## Bearing\nShip it.\n" }))
        .await
        .expect_err("a ledger missing three sections is not a ledger");
    assert!(format!("{e}").contains("Learned"), "{e}");

    write
        .call(serde_json::json!({
            "markdown": "## Bearing\nGet the limiter landed first.\n\n## Learned\n- The suite needs LC_ALL=C.\n\n## Tried\n(nothing yet)\n\n## Watching\n(none)\n"
        }))
        .await
        .expect("a complete ledger is accepted");
    assert!(ledger::read(&project.id).unwrap().contains("LC_ALL=C"));

    let note = conductor_tools::ConductorNoteTool::new(project.id.clone());
    note.call(serde_json::json!({ "section": "Watching", "text": "CI is flaky on macOS runners" }))
        .await
        .expect("one line is cheaper than a rewrite");
    assert!(
        projects::section_body(&ledger::read(&project.id).unwrap(), "Watching")
            .unwrap()
            .contains("flaky")
    );

    // The other half of the memory: what the worker did is written by the
    // RUNNER, from the task deltas, so it cannot be forgotten by a model.
    let mut before = vec![
        Task {
            id: "t1".into(),
            title: "Write the bucket".into(),
            detail: String::new(),
            status: TaskStatus::Todo,
            deps: vec![],
            assignee: None,
            mutates_workspace: true,
            pending_run: None,
            gate: None,
            attempts: 0,
            evidence: vec![],
            blocked_reason: None,
            created_at: String::new(),
            updated_at: String::new(),
        },
        Task {
            id: "t2".into(),
            title: "Wire it in".into(),
            detail: String::new(),
            status: TaskStatus::Todo,
            deps: vec![],
            assignee: None,
            mutates_workspace: true,
            pending_run: None,
            gate: None,
            attempts: 0,
            evidence: vec![],
            blocked_reason: None,
            created_at: String::new(),
            updated_at: String::new(),
        },
    ];
    let mut after = before.clone();
    after[0].status = TaskStatus::Done;
    after[1].attempts = 1;
    ledger::record_worker_return(&project.id, 7, "Landed the bucket; the wiring needs a decision.", &before, &after);

    let tried = projects::section_body(&ledger::read(&project.id).unwrap(), "Tried")
        .unwrap()
        .to_string();
    // "tick 7", not "t7": this section is full of task ids, and a tick rendered
    // the same way is one more thing to disambiguate while reading it.
    assert!(tried.contains("tick 7"), "the tick number: {tried}");
    assert!(!tried.contains("**t7**"), "and never in the shape of a task id: {tried}");
    assert!(tried.contains("closed t1"), "{tried}");
    assert!(tried.contains("t2 (attempt 1)"), "unfinished work is on the record: {tried}");

    // A tick where nothing moved says so, rather than saying nothing — three of
    // these in a row is the pattern the no-progress rail exists to catch, and
    // the conductor should be able to see it coming.
    before = after.clone();
    ledger::record_worker_return(&project.id, 8, "Read the middleware.", &before, &after);
    let tried = projects::section_body(&ledger::read(&project.id).unwrap(), "Tried")
        .unwrap()
        .to_string();
    assert!(tried.contains("nothing moved"), "{tried}");

    // ── the worker's brief is state, not something re-derived every tick ─────
    // Which is the difference between a brief and a briefing: this says what the
    // project IS, so it must not change under the worker between ticks.
    let mut with_brief = projects::get(&project.id).unwrap();
    assert!(
        with_brief.worker_brief.is_empty(),
        "a new project has no brief until its first tick writes one"
    );
    with_brief.worker_brief = "A Rust service. Verified means `cargo test --all` is green.".into();
    projects::save(&with_brief).expect("save");

    // It reaches the worker through the system prompt rather than the tick
    // message, so it survives a fresh context and does not cost a re-send.
    let persona = metalcraft_agent::persona::Persona::load(
        "project-builder",
        &metalcraft_agent::paths::personas_dir(),
    );
    if let Ok(persona) = persona {
        let extras = metalcraft_agent::persona::PromptExtras::default()
            .with_project_brief(&with_brief.worker_brief);
        let prompt = persona.build_system_prompt_with(
            &metalcraft_agent::paths::skills_dir(),
            ".",
            &extras,
        );
        assert!(prompt.contains("cargo test --all"), "the brief must reach the system prompt");
        assert!(prompt.contains("# This Project"), "under its own heading");
    }

    let _ = fs::remove_dir_all(&data_dir);
}
