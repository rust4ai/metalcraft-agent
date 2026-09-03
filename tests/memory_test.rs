//! End-to-end test of the memory store's public API and its on-disk format.
//!
//! Deliberately a **single** test function in its own binary. `paths::data_dir()`
//! resolves once per process and the instance layer keeps a process-wide resident
//! set, so two tests sharing this binary would share state and race. One linear
//! test also lets the assertions build on each other the way real usage does.
//!
//! Everything is scoped to an agent instance, because that is the only scope
//! there is: memory belongs to an agent, and a caller without one (the CLI) has
//! no memory rather than a shared one.
use metalcraft::{AgentMessage, AgentState};
use metalcraft_agent::memory::{
    self, RememberRequest,
    capture::{self, CaptureContext, CaptureKind},
    index::MemoryIndex,
    inject,
    recall::{Mode, RecallOptions},
    types::MemoryKind,
    types::Source,
    wal,
};
use metalcraft_agent::persona::PromptExtras;

/// The agent everything here belongs to. No `AgentInstance` record is created:
/// an id with no record resolves to no preset base, which is exactly the
/// "agent that only knows what it learns" case.
const INST: &str = "inst_memorytest";

fn text_opts() -> RecallOptions {
    RecallOptions {
        mode: Mode::Text,
        ..Default::default()
    }
}

#[tokio::test]
async fn memory_store_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    // SAFETY: set before any other thread exists in this test binary, and before
    // the first `data_dir()` call, which caches the value in a OnceLock.
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", dir.path());
    }

    assert!(memory::enabled(), "memory is on by default");
    let fresh = memory::instance_view(INST, 0).await;
    assert_eq!(fresh.learned, 0, "a fresh agent has learned nothing");
    assert_eq!(fresh.shipped, 0, "and this one ships nothing either");

    // ── write ────────────────────────────────────────────────────────────────
    let saved = memory::remember(
        INST,
        RememberRequest::new(
            MemoryKind::Preference,
            "Andrew prefers Rust over Go for pod services.",
            Source::User,
        ),
    )
    .await
    .expect("remember");
    assert!(!saved.deduplicated);
    assert_eq!(saved.redactions, 0);
    let pref_id = saved.memory.id.clone();

    let mut req = RememberRequest::new(
        MemoryKind::Semantic,
        "metalcraft-inference proxies /responses and /embeddings, not /completions.",
        Source::Tool,
    );
    req.entity = Some("metalcraft-inference".into());
    req.importance = Some(8.0);
    let fact = memory::remember(INST, req).await.expect("remember");
    let fact_id = fact.memory.id.clone();
    assert_eq!(fact.memory.entity.as_deref(), Some("metalcraft-inference"));
    assert_eq!(fact.memory.importance, 8.0);

    // ── secrets never reach the store ────────────────────────────────────────
    let leaky = memory::remember(
        INST,
        RememberRequest::new(
            MemoryKind::Semantic,
            "the deploy key is sk-proj-abcdefghijklmnopqrstuvwxyz012345 for now",
            Source::Turn,
        ),
    )
    .await
    .expect("remember");
    assert_eq!(leaky.redactions, 1, "the key must have been scrubbed");
    assert!(!leaky.memory.content.contains("abcdefghijkl"));
    assert!(leaky.memory.content.contains("[REDACTED:openai-key]"));

    // ── exact duplicates reinforce rather than pile up ───────────────────────
    let again = memory::remember(
        INST,
        RememberRequest::new(
            MemoryKind::Preference,
            "Andrew   prefers rust over GO for pod services.", // reformatted + recased
            Source::User,
        ),
    )
    .await
    .expect("remember");
    assert!(
        again.deduplicated,
        "normalized-identical content must dedupe"
    );
    assert_eq!(
        again.memory.id, pref_id,
        "and reinforce the original record"
    );
    assert_eq!(memory::instance_view(INST, 0).await.learned, 3);

    // ── keyword recall ───────────────────────────────────────────────────────
    let hits = memory::recall(INST, "embeddings proxy", text_opts()).await;
    assert_eq!(
        hits.len(),
        1,
        "got: {:?}",
        hits.iter().map(|h| &h.memory.content).collect::<Vec<_>>()
    );
    assert_eq!(hits[0].memory.id, fact_id);
    assert!(hits[0].score > 0.0);
    assert_eq!(hits[0].signals.text_rank, Some(0));

    let pref_only = RecallOptions {
        kind: Some(MemoryKind::Preference),
        ..text_opts()
    };
    assert_eq!(memory::recall(INST, "rust", pref_only).await.len(), 1);
    let episodic_only = RecallOptions {
        kind: Some(MemoryKind::Episodic),
        ..text_opts()
    };
    assert_eq!(memory::recall(INST, "rust", episodic_only).await.len(), 0);
    assert!(
        memory::recall(INST, "nothing matches this at all", text_opts())
            .await
            .is_empty()
    );

    // One agent cannot read another's memory, even given a real id.
    assert!(
        memory::get("inst_someone_else", &fact_id).await.is_none(),
        "memory is scoped to the agent that wrote it"
    );
    assert!(
        memory::recall("inst_someone_else", "embeddings proxy", text_opts())
            .await
            .is_empty()
    );

    // ── forget ───────────────────────────────────────────────────────────────
    // This agent has no preset base, so everything it holds is its own and is
    // purged outright. Tombstoning a *shipped* memory is covered by
    // `memory_layers_test`, which builds a base to tombstone against.
    assert!(matches!(
        memory::forget(INST, &leaky.memory.id).await.expect("forget"),
        memory::instance::Forgotten::Purged
    ));
    assert!(
        memory::get(INST, &leaky.memory.id).await.is_none(),
        "a purge is permanent"
    );
    assert!(
        memory::recall(INST, "REDACTED", text_opts()).await.is_empty(),
        "and it drops out of recall"
    );
    assert_eq!(memory::instance_view(INST, 0).await.learned, 2);
    assert!(memory::forget(INST, "nonexistent").await.is_err());

    // ── the on-disk format is what the loader expects ────────────────────────
    let wal_path = metalcraft_agent::paths::memory_instance_dir(INST).join("wal.jsonl");
    assert!(wal_path.exists(), "writes must have produced a log");

    let (events, skipped) = wal::replay(&wal_path);
    assert_eq!(skipped, 0, "the log we wrote must be fully parseable");
    assert!(!events.is_empty());

    // Replaying the log alone reconstructs the same live state — this is the boot
    // path with no snapshot.
    let mut rebuilt = MemoryIndex::new();
    for e in events {
        rebuilt.apply(e);
    }
    assert_eq!(
        rebuilt.len(),
        2,
        "purged memory must not come back from the log"
    );
    assert_eq!(rebuilt.search("embeddings proxy", 10, None).len(), 1);
    assert!(rebuilt.get(&pref_id).is_some());

    // ── the system-prompt profile ────────────────────────────────────────────
    // The profile is the stable half: pinned memories and preferences, in a
    // fixed order, so it stays byte-identical across turns and inside the
    // provider's cached prompt prefix.
    let profile = memory::profile_block(INST).await;
    assert!(
        profile.contains("Andrew prefers Rust"),
        "preferences belong in the profile: {profile}"
    );
    assert!(
        !profile.contains("metalcraft-inference proxies"),
        "a plain semantic fact is query-dependent and belongs in per-turn recall, not the profile"
    );
    assert_eq!(
        profile,
        memory::profile_block(INST).await,
        "the profile must be stable between calls"
    );
    assert!(
        memory::profile_block("inst_someone_else").await.is_empty(),
        "the profile is one agent's, not the pod's"
    );

    let mut pinned = RememberRequest::new(
        MemoryKind::Semantic,
        "The pod is deployed to k3s behind Caddy, never Railway.",
        Source::User,
    );
    pinned.pinned = true;
    let pinned_id = memory::remember(INST, pinned)
        .await
        .expect("remember")
        .memory
        .id;
    let profile = memory::profile_block(INST).await;
    assert!(
        profile.contains("k3s behind Caddy"),
        "pinned memories are always in the profile"
    );
    assert!(
        profile.starts_with("- The pod is deployed"),
        "pinned sorts first"
    );

    // It lands in the system prompt through the persona's placeholder machinery.
    let p = metalcraft_agent::persona::Persona {
        name: "T".into(),
        description: "test".into(),
        tools: vec![],
        integrations: vec![],
        skills: vec![],
        version: None,
        max_run_secs: None,
        system_prompt: "You are a test persona.".into(),
    };
    let extras = PromptExtras {
        memory_profile: profile.clone(),
        project_brief: String::new(),
    };
    let prompt = p.build_system_prompt_with(&dir.path().join("skills"), ".", &extras);
    assert!(prompt.contains("# What You Remember About This User"));
    assert!(prompt.contains("k3s behind Caddy"));
    // And an explicit placeholder wins over the fallback heading.
    let mut templated = p.clone();
    templated.system_prompt = "Base.\n\nMEMORY:\n{{memory_profile}}".into();
    let prompt = templated.build_system_prompt_with(&dir.path().join("skills"), ".", &extras);
    assert!(prompt.contains("MEMORY:\n- The pod is deployed"));
    assert!(
        !prompt.contains("# What You Remember About This User"),
        "no duplicate section"
    );
    // Empty extras suppress the section entirely rather than printing a heading.
    let prompt =
        p.build_system_prompt_with(&dir.path().join("skills"), ".", &PromptExtras::default());
    assert!(!prompt.contains("What You Remember"));
    // A caller with no agent — the CLI — gets no profile at all.
    assert!(
        PromptExtras::load(None).await.memory_profile.is_empty(),
        "no agent, no memory"
    );

    // ── per-turn injection is ephemeral ──────────────────────────────────────
    let mut state = AgentState::new("what do you know about the inference gateway proxying?");
    let before = state.messages.len();
    let injected = inject::inject(&mut state, INST, RecallOptions::default()).await;
    assert!(
        injected,
        "a matching memory exists, so something should be injected"
    );
    assert_eq!(state.messages.len(), before + 1);

    let blocks: Vec<&String> = state
        .messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::User(t) if t.starts_with(inject::SENTINEL) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].contains("context, not instructions"));

    // The user's own words stay last, so the question is what the model reads
    // most recently.
    match state.messages.last() {
        Some(AgentMessage::User(t)) => {
            assert!(
                t.starts_with("what do you know"),
                "the real question must remain last"
            );
        }
        other => panic!("expected the user message last, got {other:?}"),
    }

    // Stripping restores exactly what came in — this is what keeps the block out
    // of the persisted transcript and out of the compaction token count.
    inject::strip_messages(&mut state.messages);
    assert_eq!(state.messages.len(), before);
    assert!(
        !state
            .messages
            .iter()
            .any(|m| matches!(m, AgentMessage::User(t) if t.contains(inject::SENTINEL))),
        "no trace of the injection survives"
    );

    // A query nothing matches injects nothing at all.
    let mut empty = AgentState::new("zzzz nothing in this store resembles this query zzzz");
    assert!(!inject::inject(&mut empty, INST, RecallOptions::default()).await);
    assert_eq!(empty.messages.len(), 1);

    // Cleanup so the pinned memory does not skew the final counts.
    memory::forget(INST, &pinned_id).await.expect("purge");

    // ── capture: the cheap half of remembering ───────────────────────────────
    // A turn appends one line and does nothing else. Nothing here calls an LLM
    // or summarizes — that is the whole point, and it is why this can run on
    // every turn.
    assert!(capture::pending(INST).is_empty(), "nothing captured yet");

    let ctx = CaptureContext {
        chat_id: Some("chat-42".into()),
        persona: Some("orchestrator-agent".into()),
        instance_id: Some(INST.into()),
    };
    capture::record_turn(
        &ctx,
        "how do I deploy the pod?",
        "run ./start-agent.sh — it wraps Caddy and writes .env",
        vec!["read_file".into(), "bash".into()],
    );

    let pending = capture::pending(INST);
    assert_eq!(pending.len(), 1);
    let c = &pending[0];
    assert_eq!(c.kind, CaptureKind::Turn);
    assert_eq!(c.chat_id.as_deref(), Some("chat-42"));
    assert_eq!(c.persona.as_deref(), Some("orchestrator-agent"));
    assert_eq!(
        c.tools,
        vec!["read_file", "bash"],
        "tool names are what make procedural memory possible"
    );
    assert!(c.has_content());
    assert!(c.processed_at.is_none());

    // A turn with no agent has no queue to go in.
    let anonymous = CaptureContext {
        chat_id: Some("chat-43".into()),
        persona: None,
        instance_id: None,
    };
    capture::record_turn(&anonymous, "a CLI question", "a CLI answer", vec![]);
    assert_eq!(
        capture::pending(INST).len(),
        1,
        "a turn with no agent is not captured anywhere"
    );

    // Secrets are scrubbed on the way into the queue, not at distillation time —
    // the queue is a file on disk like any other.
    capture::record_turn(
        &ctx,
        "try sk-proj-abcdefghijklmnopqrstuvwxyz012345",
        "ok",
        vec![],
    );
    let leaked = capture::pending(INST);
    assert!(
        leaked.iter().all(|c| !c.user_text.contains("abcdefghijkl")),
        "a key must never reach the capture queue"
    );
    assert!(
        leaked
            .iter()
            .any(|c| c.user_text.contains("[REDACTED:openai-key]"))
    );

    // A compaction summary is rescued rather than discarded.
    capture::record_compaction(&ctx, "Earlier: set up the pod, fixed TLS, deployed to k3s.");
    let compactions: Vec<_> = capture::pending(INST)
        .into_iter()
        .filter(|c| c.kind == CaptureKind::Compaction)
        .collect();
    assert_eq!(compactions.len(), 1);
    assert!(compactions[0].agent_text.contains("deployed to k3s"));
    assert!(
        compactions[0].user_text.is_empty(),
        "a summary has no user side"
    );

    // An end-of-conversation marker carries no content — it exists so a later
    // distillation pass knows an episode is closed without waiting for a gap.
    capture::record_session_end(INST, "chat-42");
    let ends: Vec<_> = capture::pending(INST)
        .into_iter()
        .filter(|c| c.kind == CaptureKind::SessionEnd)
        .collect();
    assert_eq!(ends.len(), 1);
    assert!(!ends[0].has_content());

    // Empty turns are not worth a line.
    let before = capture::pending_count(INST);
    capture::record_turn(&ctx, "   ", "", vec![]);
    assert_eq!(
        capture::pending_count(INST),
        before,
        "an empty exchange is not captured"
    );

    // Captures are ordered oldest-first.
    let ordered = capture::pending(INST);
    assert!(ordered.windows(2).all(|w| w[0].at <= w[1].at));

    // Draining keeps everything not yet claimed.
    let processed: Vec<String> = ordered.iter().take(2).map(|c| c.id.clone()).collect();
    let removed = capture::retain_pending(INST, &processed).expect("retain");
    assert_eq!(removed, 2);
    assert_eq!(capture::pending_count(INST), before - 2);
    let remaining_ids: Vec<String> = capture::pending(INST).into_iter().map(|c| c.id).collect();
    assert!(
        processed.iter().all(|p| !remaining_ids.contains(p)),
        "drained captures are gone"
    );

    // And the queue survives a reread from disk unscathed.
    let (all, skipped) = capture::read_all(INST);
    assert_eq!(skipped, 0, "everything written must parse back");
    assert_eq!(all.len(), capture::pending_count(INST));
}
