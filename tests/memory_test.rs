//! End-to-end test of the memory store's public API and its on-disk format.
//!
//! Deliberately a **single** test function in its own binary. `crate::memory`
//! hangs off process-global `OnceLock`s (the store and the embedder), and
//! `paths::data_dir()` resolves once per process, so two tests sharing this
//! binary would share one store and race. One linear test also lets the
//! assertions build on each other the way real usage does.
//!
//! The embedder is [`NullEmbedder`] — a deterministic hashed bag-of-words
//! projection. It exercises the real vector path (persistence, dimensionality,
//! fusion, backfill) without a network call, which is the point: everything here
//! is about the machinery around embeddings, not about embedding quality.
use std::sync::Arc;

use metalcraft::{AgentMessage, AgentState};
use metalcraft_agent::memory::{
    self, RememberRequest,
    capture::{self, CaptureContext, CaptureKind},
    embed::{Availability, NullEmbedder},
    index::MemoryIndex,
    inject,
    recall::{Mode, RecallOptions},
    types::{LinkKind, MemoryKind, Source},
    vectors, wal,
};
use metalcraft_agent::persona::PromptExtras;

const DIMS: usize = 64;

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
        std::env::set_var("MEMORY_EMBED_DIMS", DIMS.to_string());
    }
    assert!(
        memory::set_embedder(Arc::new(NullEmbedder::new(DIMS))),
        "embedder installs once"
    );
    assert_eq!(memory::embedding_availability(), Availability::Ready);

    assert!(memory::enabled(), "memory is on by default");
    assert_eq!(
        memory::stats().await.total,
        0,
        "a fresh pod has an empty store"
    );

    // ── write ────────────────────────────────────────────────────────────────
    let saved = memory::remember(RememberRequest::new(
        MemoryKind::Preference,
        "Andrew prefers Rust over Go for pod services.",
        Source::User,
    ))
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
    let fact = memory::remember(req).await.expect("remember");
    let fact_id = fact.memory.id.clone();
    assert_eq!(fact.memory.entity.as_deref(), Some("metalcraft-inference"));
    assert_eq!(fact.memory.importance, 8.0);

    // ── secrets never reach the store ────────────────────────────────────────
    let leaky = memory::remember(RememberRequest::new(
        MemoryKind::Semantic,
        "the deploy key is sk-proj-abcdefghijklmnopqrstuvwxyz012345 for now",
        Source::Turn,
    ))
    .await
    .expect("remember");
    assert_eq!(leaky.redactions, 1, "the key must have been scrubbed");
    assert!(!leaky.memory.content.contains("abcdefghijkl"));
    assert!(leaky.memory.content.contains("[REDACTED:openai-key]"));

    // ── exact duplicates reinforce rather than pile up ───────────────────────
    let again = memory::remember(RememberRequest::new(
        MemoryKind::Preference,
        "Andrew   prefers rust over GO for pod services.", // reformatted + recased
        Source::User,
    ))
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
    assert_eq!(memory::stats().await.total, 3);

    // ── keyword recall ───────────────────────────────────────────────────────
    let hits = memory::recall("embeddings proxy", text_opts()).await;
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
    assert_eq!(memory::recall("rust", pref_only).await.len(), 1);
    let episodic_only = RecallOptions {
        kind: Some(MemoryKind::Episodic),
        ..text_opts()
    };
    assert_eq!(memory::recall("rust", episodic_only).await.len(), 0);
    assert!(
        memory::recall("nothing matches this at all", text_opts())
            .await
            .is_empty()
    );

    // Recall records the access — that is what feeds decay later.
    let (touched, _, _) = memory::get(&fact_id).await.expect("get");
    assert!(touched.access_count >= 1, "recall must record access");

    // ── embeddings ───────────────────────────────────────────────────────────
    let embedded = memory::backfill_embeddings(100).await.expect("backfill");
    assert!(
        embedded >= 3,
        "every live memory should get a vector, got {embedded}"
    );
    let s = memory::stats().await;
    assert_eq!(s.vectors, s.live, "full coverage after backfill");
    assert_eq!(
        memory::backfill_embeddings(100).await.expect("backfill"),
        0,
        "backfill is idempotent"
    );

    let vectors_path = metalcraft_agent::paths::memory_vectors_file();
    assert!(vectors_path.exists());
    let (on_disk, torn) = vectors::load(&vectors_path);
    assert_eq!(torn, 0, "the vector file we wrote must be fully parseable");
    assert!(on_disk.contains_key(&fact_id));
    assert_eq!(
        on_disk[&fact_id].len(),
        DIMS,
        "vectors are written at the configured size"
    );

    // Hybrid retrieves what keyword search alone finds, and reports which
    // retriever matched.
    let hybrid = memory::recall("embeddings proxy", RecallOptions::default()).await;
    assert!(!hybrid.is_empty());
    let fact_hit = hybrid
        .iter()
        .find(|h| h.memory.id == fact_id)
        .expect("fact recalled");
    assert!(
        fact_hit.signals.text_rank.is_some() || fact_hit.signals.vector_rank.is_some(),
        "a hit must record where it came from: {}",
        fact_hit.signals.describe()
    );

    // Vector-only mode runs with no keyword leg at all.
    //
    // NOTE: `NullEmbedder` projects a *bag of words*, so it only scores above
    // zero on shared vocabulary. This asserts the vector path is wired end to end
    // — query embedded, cosine ranked, signals recorded — and deliberately does
    // NOT claim anything about paraphrase matching, which only a real embedding
    // model provides. `recall.rs`'s unit tests cover paraphrase with hand-built
    // vectors, where the geometry is under the test's control.
    let vector_only = memory::recall(
        "proxies embeddings",
        RecallOptions {
            mode: Mode::Vector,
            ..Default::default()
        },
    )
    .await;
    assert!(
        !vector_only.is_empty(),
        "vector mode should return something"
    );
    assert!(
        vector_only.iter().all(|h| h.signals.text_rank.is_none()),
        "no keyword leg in vector mode"
    );
    assert!(
        vector_only
            .iter()
            .all(|h| h.signals.vector_similarity.is_some())
    );
    assert!(vector_only.iter().any(|h| h.memory.id == fact_id));

    // ── graph ────────────────────────────────────────────────────────────────
    memory::link(&pref_id, &fact_id, LinkKind::RelatesTo, "test")
        .await
        .expect("link");
    assert!(
        memory::link(&pref_id, &pref_id, LinkKind::RelatesTo, "test")
            .await
            .is_err(),
        "no self-links"
    );
    assert!(
        memory::link(&pref_id, "nonexistent", LinkKind::RelatesTo, "test")
            .await
            .is_err()
    );

    let (_, out_links, _) = memory::get(&pref_id).await.expect("get");
    assert_eq!(out_links.len(), 1);
    let (_, _, in_links) = memory::get(&fact_id).await.expect("get");
    assert_eq!(in_links.len(), 1);

    // ── forget: archive is soft, purge is not ────────────────────────────────
    memory::forget(&leaky.memory.id, false)
        .await
        .expect("archive");
    assert!(
        memory::get(&leaky.memory.id).await.is_some(),
        "archived records stay readable by id"
    );
    assert!(
        memory::recall("REDACTED", text_opts()).await.is_empty(),
        "archived memories drop out of recall"
    );
    let s = memory::stats().await;
    assert_eq!(s.total, 3);
    assert_eq!(s.live, 2);
    assert_eq!(s.archived, 1);

    memory::forget(&leaky.memory.id, true).await.expect("purge");
    assert!(
        memory::get(&leaky.memory.id).await.is_none(),
        "purge is permanent"
    );
    assert_eq!(memory::stats().await.total, 2);
    assert!(memory::forget("nonexistent", false).await.is_err());

    // ── the on-disk format is what the loader expects ────────────────────────
    let wal_path = metalcraft_agent::paths::memory_wal_file();
    let snapshot_path = metalcraft_agent::paths::memory_snapshot_file();
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

    // Vectors reattach from the sidecar, and one for a purged memory is dropped.
    let (from_disk, _) = vectors::load(&vectors_path);
    let attached = rebuilt.load_vectors(from_disk);
    assert_eq!(
        attached, 2,
        "only vectors whose memory still exists are kept"
    );
    assert_eq!(rebuilt.vector_count(), 2);

    // ── compaction: snapshot, rewritten vectors, truncated log ───────────────
    let folded = memory::compact().await.expect("compact");
    assert!(
        folded > 0,
        "compaction should have folded the events it reported"
    );
    assert!(snapshot_path.exists());
    assert_eq!(wal::count(&wal_path), 0, "the log is emptied after folding");
    assert!(
        !snapshot_path.with_extension("json.tmp").exists(),
        "no stray tmp file"
    );
    assert!(
        !vectors_path.with_extension("bin.tmp").exists(),
        "no stray tmp file"
    );

    // Compaction collapses the vector file to exactly the live set.
    let (compacted_vectors, torn) = vectors::load(&vectors_path);
    assert_eq!(torn, 0);
    assert_eq!(
        compacted_vectors.len(),
        2,
        "purged and superseded vectors are dropped"
    );

    // The snapshot alone reconstructs the store — the other half of boot — and
    // records what produced the vectors, so a model change is detectable.
    let snap = wal::read_snapshot(&snapshot_path).expect("snapshot parses");
    assert_eq!(
        snap.embed_dims,
        Some(DIMS),
        "the snapshot records the vector geometry"
    );
    assert!(snap.embed_model.is_some());
    let from_snap = MemoryIndex::from_snapshot(snap);
    assert_eq!(from_snap.len(), 2);
    assert_eq!(from_snap.search("rust", 10, None).len(), 1);
    assert_eq!(
        from_snap.links_from(&pref_id).len(),
        1,
        "links survive compaction"
    );

    // Compaction is safe to repeat and does not lose state.
    memory::compact().await.expect("compact again");
    assert_eq!(memory::stats().await.total, 2);
    assert_eq!(memory::stats().await.vectors, 2);

    // ── the system-prompt profile ────────────────────────────────────────────
    // The profile is the stable half: pinned memories and preferences, in a
    // fixed order, so it stays byte-identical across turns and inside the
    // provider's cached prompt prefix.
    let profile = memory::profile_block().await;
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
        memory::profile_block().await,
        "the profile must be stable between calls"
    );

    let mut pinned = RememberRequest::new(
        MemoryKind::Semantic,
        "The pod is deployed to k3s behind Caddy, never Railway.",
        Source::User,
    );
    pinned.pinned = true;
    let pinned_id = memory::remember(pinned).await.expect("remember").memory.id;
    let profile = memory::profile_block().await;
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

    // ── per-turn injection is ephemeral ──────────────────────────────────────
    let mut state = AgentState::new("what do you know about the inference gateway proxying?");
    let before = state.messages.len();
    let injected = inject::inject(&mut state, RecallOptions::default()).await;
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
    assert!(!inject::inject(&mut empty, RecallOptions::default()).await);
    assert_eq!(empty.messages.len(), 1);

    // Cleanup so the pinned memory does not skew the final counts.
    memory::forget(&pinned_id, true).await.expect("purge");

    // ── capture: the cheap half of remembering ───────────────────────────────
    // A turn appends one line and does nothing else. Nothing here calls an LLM,
    // embeds, or summarizes — that is the whole point, and it is why this can run
    // on every turn.
    assert!(capture::pending().is_empty(), "nothing captured yet");

    let ctx = CaptureContext {
        chat_id: Some("chat-42".into()),
        persona: Some("orchestrator-agent".into()),
        instance_id: None,
    };
    capture::record_turn(
        &ctx,
        "how do I deploy the pod?",
        "run ./start-agent.sh — it wraps Caddy and writes .env",
        vec!["read_file".into(), "bash".into()],
    );

    let pending = capture::pending();
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

    // Secrets are scrubbed on the way into the queue, not at distillation time —
    // the queue is a file on disk like any other.
    capture::record_turn(
        &ctx,
        "try sk-proj-abcdefghijklmnopqrstuvwxyz012345",
        "ok",
        vec![],
    );
    let leaked = capture::pending();
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
    let compactions: Vec<_> = capture::pending()
        .into_iter()
        .filter(|c| c.kind == CaptureKind::Compaction)
        .collect();
    assert_eq!(compactions.len(), 1);
    assert!(compactions[0].agent_text.contains("deployed to k3s"));
    assert!(
        compactions[0].user_text.is_empty(),
        "a summary has no user side"
    );

    // An end-of-conversation marker carries no content — it exists so the dream
    // knows an episode is closed without waiting for a time gap.
    capture::record_session_end("chat-42");
    let ends: Vec<_> = capture::pending()
        .into_iter()
        .filter(|c| c.kind == CaptureKind::SessionEnd)
        .collect();
    assert_eq!(ends.len(), 1);
    assert!(!ends[0].has_content());

    // Empty turns are not worth a line.
    let before = capture::pending_count();
    capture::record_turn(&ctx, "   ", "", vec![]);
    assert_eq!(
        capture::pending_count(),
        before,
        "an empty exchange is not captured"
    );

    // Captures are ordered oldest-first, which is the order the dream wants.
    let ordered = capture::pending();
    assert!(ordered.windows(2).all(|w| w[0].at <= w[1].at));

    // Draining keeps everything the dream has not claimed.
    let processed: Vec<String> = ordered.iter().take(2).map(|c| c.id.clone()).collect();
    let removed = capture::retain_pending(&processed).expect("retain");
    assert_eq!(removed, 2);
    assert_eq!(capture::pending_count(), before - 2);
    let remaining_ids: Vec<String> = capture::pending().into_iter().map(|c| c.id).collect();
    assert!(
        processed.iter().all(|p| !remaining_ids.contains(p)),
        "drained captures are gone"
    );

    // And the queue survives a reread from disk unscathed.
    let (all, skipped) = capture::read_all();
    assert_eq!(skipped, 0, "everything written must parse back");
    assert_eq!(all.len(), capture::pending_count());
}
