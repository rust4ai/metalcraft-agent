//! One real tick, against a real model.
//!
//! Everything else about projects is unit-tested: the store, the task graph, the
//! ledger, the guards. None of that can tell you whether a **conductor writes a
//! briefing worth reading**, or whether a worker handed one does anything with
//! it. This does — it boots a project the way the API does, runs one tick end to
//! end, and reads back what the two agents actually left behind.
//!
//! It is the only test here that spends money, so it is gated on
//! `OPENAI_API_KEY` and skips loudly without one.
//!
//! What it asserts is deliberately about *shape*, not content. A model writing a
//! brief will not write the same brief twice, so asserting on its words would
//! make this a test of one sampling. What must hold every time is that the
//! machinery closed its loop: a brief was composed and stored, a plan exists,
//! the ledger has been written, the conversation has a turn in it, and the
//! journal knows whether the briefing came from the conductor or the fallback.

use std::fs;
use std::sync::Once;

use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::projects::{self, Project, ProjectKind, ProjectStatus};
use metalcraft_agent::runtime::AgentRuntimeContext;
use metalcraft_agent::{project_conductor, project_tasks, project_tick, seed};

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-project-live-{}", std::process::id()));
        let _ = fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();
        seed::ensure_defaults();
    });
}

/// A project with no repository. Deliberate: without a buildr.space credential
/// the workspace half is unreachable anyway, and this test is about whether the
/// *conductor and worker loop* closes — a project that can only read and think
/// exercises every part of that and none of the parts that need somebody's
/// cloud account.
fn a_project(id: &str) -> Project {
    Project {
        id: id.into(),
        title: "Docs audit".into(),
        goal: "Work out what a newcomer to this repository would misunderstand first, \
               and write it down. Reading and note-taking only — do not change any files."
            .into(),
        kind: ProjectKind::Build,
        instance_id: String::new(),
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
async fn a_project_boots_and_ticks_for_real() {
    init();

    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!(
            "SKIP a_project_boots_and_ticks_for_real: set OPENAI_API_KEY (e.g. in a crate-root \
             .env) to run the one test that proves any of this works."
        );
        return;
    }

    let Ok(context) = AgentRuntimeContext::from_environment() else {
        eprintln!("SKIP a_project_boots_and_ticks_for_real: no runtime context.");
        return;
    };

    // ── boot, the way the API does it ────────────────────────────────────────
    let id = projects::new_id();
    let mut project = a_project(&id);
    let worker = metalcraft_agent::agent_instance::for_project(
        &id,
        &project.title,
        &project.agent_preset,
        metalcraft_agent::agent_instance::ProjectRole::Worker,
    )
    .expect("worker instance");
    let conductor = metalcraft_agent::agent_instance::for_project(
        &id,
        &project.title,
        &project.agent_preset,
        metalcraft_agent::agent_instance::ProjectRole::Conductor,
    )
    .expect("conductor instance");
    // The two agents must not be the same one: they accumulate different memory,
    // and sharing an instance would give each the other's recall exactly when it
    // misleads.
    assert_ne!(worker.id, conductor.id, "a project runs two agents, not one");

    project.instance_id = worker.id.clone();
    project.conductor_instance_id = conductor.id.clone();
    projects::save(&project).expect("save");
    projects::write_scratchpad(&id, &projects::seed_scratchpad(&project)).expect("seed");

    // ── one tick, for real ───────────────────────────────────────────────────
    let cwd = env!("CARGO_MANIFEST_DIR").to_string();
    let outcome = project_tick::run_tick(
        &context,
        &projects::get(&id).unwrap(),
        &cwd,
        &ApprovalMode::AutoApprove,
    )
    .await;

    assert!(!outcome.waited, "nothing was pending, so the tick had to spend");

    let after = projects::get(&id).expect("the project survived its own tick");
    eprintln!(
        "\n── tick 1 ──────────────────────────────────────\nstatus: {:?}  progressed: {}\n\
         summary: {}\n",
        after.status, outcome.progressed, outcome.summary
    );

    // ── the conductor's first act: instructions for its worker ───────────────
    assert!(
        !after.worker_brief.trim().is_empty(),
        "the conductor did not write the worker's brief"
    );
    eprintln!("── worker brief ────────────────────────────────\n{}\n", after.worker_brief);

    // ── it planned, rather than doing the work itself ────────────────────────
    let tasks = project_tasks::list(&id);
    assert!(
        !tasks.is_empty(),
        "the conductor produced no plan; a project with no tasks has nothing for a worker to take"
    );
    eprintln!("── plan ────────────────────────────────────────\n{}\n", project_tasks::render(&tasks));

    // Every task has to be workable by a stranger, which is the one property a
    // plan can have that a list of titles cannot.
    for t in &tasks {
        assert!(!t.title.trim().is_empty(), "task {} has no title", t.id);
    }

    // ── it kept its own memory ───────────────────────────────────────────────
    let ledger = project_conductor::read(&id).expect("the conductor wrote no ledger at all");
    for section in project_conductor::SECTIONS {
        assert!(
            projects::section_body(&ledger, section).is_some(),
            "the ledger lost `## {section}`, which the tick frame refers to by name"
        );
    }
    // The runner's half: what the worker did, written from the task deltas
    // rather than from anyone's recollection.
    let tried = projects::section_body(&ledger, "Tried").unwrap_or_default();
    assert!(
        tried.contains("t1") || tried.contains("nothing moved"),
        "the worker's return was not recorded in the ledger: {tried}"
    );
    eprintln!("── conductor ledger ────────────────────────────\n{ledger}\n");

    // ── the journal knows what the tick was told, and by whom ────────────────
    let journal = project_tick::read_journal(&id, 10);
    assert_eq!(journal.len(), 1, "one tick, one journal line");
    let entry = &journal[0];
    assert!(
        entry.briefing.as_deref().is_some_and(|b| !b.trim().is_empty()),
        "the briefing was not recorded — the first question about a bad tick is what it was told"
    );
    assert!(
        entry.conducted,
        "the tick fell back to a templated briefing, so the conductor never ran: {:?}",
        entry.briefing
    );
    eprintln!("── briefing ────────────────────────────────────\n{}\n", entry.briefing.clone().unwrap_or_default());

    // ── and there is one thread a person can read it in ──────────────────────
    assert!(
        !after.session_id.trim().is_empty(),
        "the project opened no conversation to be read in"
    );

    // ── the second tick reads the first ──────────────────────────────────────
    // The point of all of it: a tick that remembers nothing still arrives
    // holding what the last one left. If the ledger and the plan did not carry,
    // this is where it shows.
    let outcome2 = project_tick::run_tick(
        &context,
        &projects::get(&id).unwrap(),
        &cwd,
        &ApprovalMode::AutoApprove,
    )
    .await;
    eprintln!("\n── tick 2 ──────────────────────────────────────\n{}\n", outcome2.summary);

    let final_project = projects::get(&id).unwrap();
    assert_eq!(final_project.counters.ticks, 2, "both ticks counted");
    assert_eq!(
        final_project.worker_brief, after.worker_brief,
        "the brief is written once — a worker whose standing instructions are \
         rewritten under it every tick has no standing instructions"
    );
    assert_eq!(
        project_tick::read_journal(&id, 10).len(),
        2,
        "two ticks, two journal lines"
    );
}
