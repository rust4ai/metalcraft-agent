//! What this agent is going to do, and when.
//!
//! A [`ScheduledFlow`] binds one trigger to one flow and names the agent it runs
//! as. Creating one is the deliberate "yes, run this in the background" act —
//! **arming** — and deleting one is disarming. Nothing else on the pod fires a
//! flow on a timer, so this directory is the complete answer to "what will this
//! pod do on its own?".
//!
//! ## Why arming lives here and not on the flow
//!
//! It used to take three places to answer that question: `flow.enabled`, then
//! `flow.schedules[].enabled`, then `flow_bindings.json` for the agent. Two of
//! those could disagree, and every install path had to remember to force both
//! false so that installing an identity didn't quietly start doing things at 3am.
//! Now the fact that nothing scheduled it *is* the off switch, and the agent it
//! runs as travels with the schedule that needs it.
//!
//! [`crate::flow_bindings`] keeps the other half: which **preset** a flow belongs
//! to, which is a property of the flow rather than of any one schedule.

use std::path::Path;

use chrono::Utc;
use metalcraft_flows::{SavedFlow, ScheduleSpec, ScheduledFlow};

use crate::agent_instance::AgentInstance;
use crate::agent_preset::AgentPreset;
use crate::paths;

/// Mint an opaque id.
///
/// Deliberately not derived from the flow or the trigger: a readable id like
/// `morning-brief-0800` becomes a lie the first time someone moves the cron, and
/// it is carried in log lines and URLs long after. [`ScheduleSpec::name`] is the
/// readable handle; this is a pointer.
pub fn new_id() -> String {
    format!("sf_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

fn dir() -> std::path::PathBuf {
    paths::scheduled_flows_dir()
}

/// Every scheduled flow on this pod.
pub fn list() -> Vec<ScheduledFlow> {
    metalcraft_flows::list_scheduled_flows(&dir())
}

/// One scheduled flow by id.
pub fn get(id: &str) -> Option<ScheduledFlow> {
    metalcraft_flows::load_scheduled_flow(&dir(), id)
}

/// Every schedule of one flow.
pub fn for_flow(flow_id: &str) -> Vec<ScheduledFlow> {
    metalcraft_flows::scheduled_for_flow(&dir(), flow_id)
}

/// Every schedule armed to one agent — "what is this thing scheduled to do",
/// which the delete-an-agent path needs before it strands a timer.
pub fn for_instance(instance_id: &str) -> Vec<ScheduledFlow> {
    list()
        .into_iter()
        .filter(|sf| sf.instance_id.as_deref() == Some(instance_id))
        .collect()
}

/// Persist a scheduled flow, validating it first.
pub fn save(sf: &ScheduledFlow) -> Result<(), String> {
    let errors = metalcraft_flows::validate_scheduled(sf);
    if !errors.is_empty() {
        return Err(errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; "));
    }
    // A cron this pod cannot parse is a schedule that silently never fires, so it
    // is rejected at the door rather than at 3am. The crate checks shape only —
    // it has no cron dependency — which leaves the host to check the dialect it
    // actually evaluates.
    if let metalcraft_flows::ScheduleTrigger::Cron { cron } = &sf.schedule.trigger {
        use std::str::FromStr;
        cron::Schedule::from_str(cron)
            .map_err(|e| format!("invalid cron expression '{cron}': {e}"))?;
        // And the zone it is read in. An unparseable name used to mean "the
        // pod's clock", which in the cluster is UTC — so `America/detroit`, or
        // `PST`, or a typo, quietly moved a morning brief to the middle of the
        // night and nothing anywhere said so. A wrong hour is harder to notice
        // than a rejected save, so this is rejected.
        if let Some(zone) = &sf.schedule.timezone {
            zone.parse::<chrono_tz::Tz>().map_err(|_| {
                format!(
                    "unknown timezone '{zone}': use an IANA name like 'America/Detroit' \
                     (case-sensitive), not an abbreviation or a UTC offset"
                )
            })?;
        }
    }
    metalcraft_flows::save_scheduled_flow(&dir(), sf).map_err(|e| e.to_string())
}

/// What a caller must supply to arm a flow. Everything else — the id, the
/// timestamps, the agent — this module resolves.
pub struct NewSchedule<'a> {
    /// The flow to run.
    pub flow: &'a SavedFlow,
    /// When to run it.
    pub schedule: ScheduleSpec,
    /// Start enabled? Defaults to `true` at the API layer; a caller that wants to
    /// stage a schedule without arming its timer passes `false`.
    pub enabled: bool,
    /// Attach to an existing agent instead of minting one.
    pub instance: Option<&'a str>,
    /// The author's suggestion key, when this came from a pack or the registry.
    pub from_suggestion: Option<String>,
    /// Use this id instead of a generated one (a hand-authored slug). Rejected on
    /// collision — a create must never overwrite somebody's existing schedule.
    pub id: Option<String>,
}

/// Arm a flow: create its schedule and bind it to an agent.
///
/// **This is what creates the agent.** Installing a flow ships it unscheduled, so
/// this is the second consent point and the natural moment for the instance to
/// come into existence.
///
/// Schedules of one flow share an agent by default — the 08:00 and 18:00 briefs
/// are the same agent, so the evening run remembers the morning one. Pass
/// `instance` to attach to an existing agent instead; running a briefer as the
/// agent you chat with is a reasonable thing to want.
pub fn arm(new: NewSchedule<'_>) -> Result<ScheduledFlow, String> {
    let preset_slug = crate::flow_bindings::preset_for(&new.flow.id);
    let preset = AgentPreset::load(&preset_slug, &paths::agent_presets_dir())?;
    // The containment rule: a flow may only reach personas its preset rosters,
    // and a schedule may override the persona, so both are checked here.
    crate::flow_bindings::check_personas(new.flow, &preset)?;
    if let Some(p) = new.schedule.persona.as_deref()
        && !preset.allows_persona(p)
    {
        return Err(format!(
            "schedule names persona '{p}', which is not in agent '{}' (roster: {})",
            preset.slug,
            preset.callable_personas().join(", ")
        ));
    }

    let id = match new.id {
        Some(id) => {
            if get(&id).is_some() {
                return Err(format!("a schedule with id '{id}' already exists"));
            }
            id
        }
        None => new_id(),
    };

    let instance = resolve_instance(new.flow, &preset, new.instance, &new.schedule)?;

    let now = Utc::now().to_rfc3339();
    let sf = ScheduledFlow {
        id,
        flow_id: new.flow.id.clone(),
        enabled: new.enabled,
        schedule: new.schedule,
        instance_id: Some(instance.id),
        from_suggestion: new.from_suggestion,
        created_at: now.clone(),
        updated_at: now,
    };
    save(&sf)?;
    Ok(sf)
}

/// The agent a new schedule runs as: an explicitly named one, else the one
/// another schedule of this flow already uses, else the flow's own agent
/// ([`crate::agent_instance::for_flow`]) — minted here only if no run has
/// already minted it.
fn resolve_instance(
    flow: &SavedFlow,
    preset: &AgentPreset,
    explicit: Option<&str>,
    schedule: &ScheduleSpec,
) -> Result<AgentInstance, String> {
    if let Some(id) = explicit {
        return crate::agent_instance::load(id);
    }

    if let Some(existing) = for_flow(&flow.id)
        .into_iter()
        .filter_map(|sf| sf.instance_id)
        .find_map(|id| crate::agent_instance::load(&id).ok())
    {
        return Ok(existing);
    }

    // Not a fresh mint: the flow may already have an agent, because running it
    // by hand once creates one now. Arming a flow somebody has already tried
    // must continue that agent — a second one beside it would split the memory
    // of a single automation across two rows of the fleet.
    let label = schedule
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(&flow.name);
    crate::agent_instance::for_flow(&flow.id, label, &preset.slug)
}

/// Disarm: delete the schedule. **The agent and everything it remembers are
/// kept** — disarming is "stop running this on a timer", not "destroy the thing
/// that was running it".
pub fn disarm(id: &str) -> Result<(), String> {
    if !metalcraft_flows::delete_scheduled_flow(&dir(), id) {
        return Err(format!("no scheduled flow '{id}'"));
    }
    Ok(())
}

/// Delete every schedule of a flow (on flow delete). Returns how many went.
///
/// A schedule pointing at a flow that no longer exists is not merely useless: the
/// daemon would log an error for it on every poll, forever.
pub fn forget_flow(flow_id: &str) -> usize {
    let mut n = 0;
    for sf in for_flow(flow_id) {
        if metalcraft_flows::delete_scheduled_flow(&dir(), &sf.id) {
            n += 1;
        }
    }
    n
}

/// Projected firing times for a trigger — `description` plus the next few runs.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct SchedulePreview {
    /// Human-readable trigger, e.g. ``"Cron `0 0 8 * * *` (America/Detroit)"``.
    pub description: String,
    /// Next few firing times as RFC-3339 strings. Empty for a manual trigger, and
    /// for a cron this pod cannot parse — which is the visible symptom of a
    /// schedule that will never fire.
    pub next_runs: Vec<String>,
}

/// Project the next `N` firings of a schedule.
pub fn preview(spec: &ScheduleSpec) -> SchedulePreview {
    preview_in(spec, crate::pod_settings::default_timezone().as_deref())
}

/// [`preview`], with the pod default supplied — so the projection a client sees
/// is computed in the same zone the daemon will fire in, rather than in
/// whichever zone the pod's own clock happens to be.
fn preview_in(spec: &ScheduleSpec, pod_zone: Option<&str>) -> SchedulePreview {
    use metalcraft_flows::ScheduleTrigger;
    use std::str::FromStr;
    const N: usize = 3;

    let mut description = spec.describe();
    // A zone name this pod cannot resolve is the same class of failure as a cron
    // it cannot parse, and reads worse: the schedule fires, on the pod's clock,
    // at an hour nobody asked for. Say so here, where the arming screen shows it.
    if let Some(zone) = spec.timezone.as_deref()
        && matches!(spec.trigger, ScheduleTrigger::Cron { .. })
        && zone.parse::<chrono_tz::Tz>().is_err()
    {
        return SchedulePreview {
            description: format!(
                "Unknown timezone `{zone}` — use an IANA name like `America/Detroit`"
            ),
            next_runs: vec![],
        };
    }
    // Say which zone a schedule that names none will actually be read in. It is
    // the pod's now, not the host clock, and a description that stays silent
    // about it leaves "08:00" meaning whatever the reader assumes.
    if let Some(zone) = pod_zone
        && spec.timezone.is_none()
        && matches!(spec.trigger, ScheduleTrigger::Cron { .. })
    {
        description = format!("{description} ({zone} — this pod's timezone)");
    }
    let next_runs = match &spec.trigger {
        ScheduleTrigger::Manual => vec![],
        ScheduleTrigger::Minutes { interval } => project_every(*interval * 60, N),
        ScheduleTrigger::Hours { interval } => project_every(*interval * 3600, N),
        ScheduleTrigger::Cron { cron } => match cron::Schedule::from_str(cron) {
            Ok(schedule) => match spec
                .timezone
                .as_deref()
                .or(pod_zone)
                .and_then(|name| name.parse::<chrono_tz::Tz>().ok())
            {
                Some(zone) => schedule
                    .upcoming(zone)
                    .take(N)
                    .map(|t| t.to_rfc3339())
                    .collect(),
                None => schedule
                    .upcoming(chrono::Local)
                    .take(N)
                    .map(|t| t.to_rfc3339())
                    .collect(),
            },
            Err(e) => {
                // Say so, rather than going quiet. `save` rejects an unparseable
                // cron, but migration carries legacy ones in on purpose — and a
                // schedule that will never fire should read as broken, not as
                // merely having nothing coming up. (The five-field POSIX form
                // lands here: this parser wants seconds.)
                description = format!("Invalid cron `{cron}`: {e}");
                vec![]
            }
        },
    };
    SchedulePreview {
        description,
        next_runs,
    }
}

/// Interval triggers fire relative to the last run, which for an unarmed schedule
/// is "now" — so the projection is now + n×interval.
fn project_every(seconds: u64, n: usize) -> Vec<String> {
    if seconds == 0 {
        return vec![];
    }
    let now = Utc::now();
    (1..=n as i64)
        .map(|i| (now + chrono::Duration::seconds(seconds as i64 * i)).to_rfc3339())
        .collect()
}

// ---- Migration -------------------------------------------------------------

/// One-time migration of pre-v3 flows: lift each flow's scheduling into its own
/// [`ScheduledFlow`], carrying the agent over from `flow_bindings.json`.
///
/// Idempotent, and safe to run on partially-migrated data. Called at startup from
/// [`crate::seed::ensure_defaults`], so both the daemon and the CLI migrate before
/// anything can read — or, worse, *re-save* — a flow. A v3 `SavedFlow` has no
/// `schedules` field, so saving an un-migrated flow through the new types would
/// silently drop its scheduling.
///
/// The safety property, enforced by [`metalcraft_flows::extract`]: a schedule is
/// migrated enabled only if the flow's master switch **and** its own toggle were
/// both on. Migration never starts something that was not already running.
pub fn migrate_from_flows() -> MigrationReport {
    let mut report = MigrationReport::default();
    let flows_dir = paths::flows_dir();
    let Ok(entries) = std::fs::read_dir(&flows_dir) else {
        return report;
    };

    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    paths.sort();

    for path in paths {
        match migrate_one(&path) {
            Ok(n) => report.created += n,
            Err(e) => {
                log::error!(
                    "flow migration: leaving {} untouched: {e}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                report.failed += 1;
            }
        }
    }

    if report.created > 0 {
        log::info!(
            "flow migration: created {} scheduled flow(s) from legacy documents",
            report.created
        );
    }
    report
}

/// What a migration pass did. Mostly for the log and the tests — nothing branches
/// on it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MigrationReport {
    /// Scheduled flows written.
    pub created: usize,
    /// Flow documents left untouched because extraction failed.
    pub failed: usize,
}

/// Migrate one flow file. Returns how many scheduled flows it produced.
///
/// Either the whole file migrates or none of it does: the flow is rewritten last,
/// after its schedules are safely on disk, so a crash in the middle leaves a
/// legacy document that will simply be migrated again.
fn migrate_one(path: &Path) -> Result<usize, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("reading: {e}"))?;
    let doc: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("not JSON: {e}"))?;
    let out = metalcraft_flows::extract(&doc)?;
    if !out.changed {
        return Ok(0);
    }

    let binding = crate::flow_bindings::get(&out.flow.id);
    let now = Utc::now().to_rfc3339();
    let mut created = 0;

    for extracted in &out.schedules {
        let instance_id = binding.instances.get(&extracted.key).cloned();

        // A manual schedule earns a document only if it was armed. Armed, it says
        // "when I run this by hand, be this agent" — real state, and the only way
        // to express it. Unarmed, it said nothing at all: every flow had one
        // whether or not anyone asked for it.
        if !extracted.schedule.trigger.is_timed() && instance_id.is_none() {
            continue;
        }

        // Already migrated (a previous pass got this far and then failed): don't
        // mint a second document for the same schedule. Ids are generated, so
        // this pair is what identifies a migrated schedule, not the filename.
        if for_flow(&out.flow.id)
            .iter()
            .any(|sf| sf.from_suggestion.as_deref() == Some(extracted.key.as_str()))
        {
            continue;
        }

        let sf = ScheduledFlow {
            id: new_id(),
            flow_id: out.flow.id.clone(),
            enabled: extracted.enabled,
            schedule: extracted.schedule.clone(),
            instance_id,
            from_suggestion: Some(extracted.key.clone()),
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        // Written directly rather than through `save`: a legacy schedule with a
        // cron this pod can't parse still has to migrate. It fired nothing before
        // and fires nothing now, but discarding it would lose the only record
        // that somebody meant it to run.
        metalcraft_flows::save_scheduled_flow(&paths::scheduled_flows_dir(), &sf)
            .map_err(|e| format!("saving schedule '{}': {e}", extracted.key))?;
        log::info!(
            "flow migration: {} '{}' → {} ({})",
            out.flow.id,
            extracted.key,
            sf.id,
            if sf.enabled { "enabled" } else { "disabled" }
        );
        created += 1;
    }

    metalcraft_flows::save_flow(&paths::flows_dir(), &out.flow)
        .map_err(|e| format!("rewriting flow: {e}"))?;

    // The bindings file keeps `preset`; the per-schedule agent map has moved into
    // the documents just written.
    crate::flow_bindings::clear_instances(&out.flow.id)?;

    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use metalcraft_flows::ScheduleTrigger;

    fn spec(trigger: ScheduleTrigger, tz: Option<&str>) -> ScheduleSpec {
        ScheduleSpec {
            trigger,
            name: None,
            timezone: tz.map(str::to_string),
            inputs: None,
            persona: None,
        }
    }

    #[test]
    fn generated_ids_are_opaque_and_unique() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(a.starts_with("sf_"), "{a}");
        assert_eq!(a.len(), 11);
    }

    #[test]
    fn a_cron_with_no_zone_of_its_own_is_read_in_the_pods() {
        // "08:00" from something that never thought about timezones — the
        // agent's own scheduling tool, a pack suggestion — used to mean 08:00
        // wherever the pod happened to run, which in the cluster is UTC.
        let p = preview_in(
            &spec(
                ScheduleTrigger::Cron {
                    cron: "0 0 8 * * *".into(),
                },
                None,
            ),
            Some("Asia/Tokyo"),
        );
        assert_eq!(p.next_runs.len(), 3);
        for run in &p.next_runs {
            assert!(
                run.contains("T08:00:00+09:00"),
                "projected {run}, which is not 08:00 in Tokyo"
            );
        }
    }

    #[test]
    fn a_borrowed_zone_says_whose_it_is() {
        let p = preview_in(
            &spec(
                ScheduleTrigger::Cron {
                    cron: "0 0 8 * * *".into(),
                },
                None,
            ),
            Some("Asia/Tokyo"),
        );
        assert_eq!(
            p.description,
            "Cron `0 0 8 * * *` (Asia/Tokyo — this pod's timezone)"
        );
    }

    #[test]
    fn a_schedules_own_zone_beats_the_pods() {
        let p = preview_in(
            &spec(
                ScheduleTrigger::Cron {
                    cron: "0 0 8 * * *".into(),
                },
                Some("Asia/Tokyo"),
            ),
            Some("America/Detroit"),
        );
        for run in &p.next_runs {
            assert!(run.contains("T08:00:00+09:00"), "{run}");
        }
    }

    #[test]
    fn a_timezone_this_pod_cannot_resolve_reads_as_broken() {
        // The failure it replaces: an unknown name fell back to the pod's clock,
        // so `America/detroit` (lowercase d), `PST`, and every typo fired at an
        // hour nobody chose, with nothing anywhere saying so.
        for name in ["america/detroit", "PST", "GMT-5", "America/Detroitt"] {
            let p = preview(&spec(
                ScheduleTrigger::Cron {
                    cron: "0 0 8 * * *".into(),
                },
                Some(name),
            ));
            assert!(
                p.description.starts_with("Unknown timezone"),
                "{name}: {}",
                p.description
            );
            assert!(p.next_runs.is_empty(), "{name} must not project firings");
        }
    }

    #[test]
    fn preview_projects_a_cron_in_its_timezone() {
        let p = preview(&spec(
            ScheduleTrigger::Cron {
                cron: "0 0 8 * * *".into(),
            },
            Some("America/Detroit"),
        ));
        assert_eq!(p.description, "Cron `0 0 8 * * *` (America/Detroit)");
        assert_eq!(p.next_runs.len(), 3);
    }

    #[test]
    fn preview_of_an_unparseable_cron_shows_no_runs() {
        // The visible symptom of a schedule that will never fire: it describes
        // itself but projects nothing.
        let p = preview(&spec(
            ScheduleTrigger::Cron {
                cron: "not a cron".into(),
            },
            None,
        ));
        assert!(p.next_runs.is_empty());
        assert!(
            p.description.starts_with("Invalid cron"),
            "{}",
            p.description
        );
    }

    #[test]
    fn preview_of_manual_projects_nothing() {
        let p = preview(&spec(ScheduleTrigger::Manual, None));
        assert!(p.next_runs.is_empty());
        assert_eq!(p.description, "Manual (runs only when triggered)");
    }

    #[test]
    fn preview_of_an_interval_projects_forward() {
        let p = preview(&spec(ScheduleTrigger::Minutes { interval: 15 }, None));
        assert_eq!(p.next_runs.len(), 3);
        assert_eq!(p.description, "Every 15 minute(s)");
        // A zero interval is invalid and projects nothing rather than looping.
        let p = preview(&spec(ScheduleTrigger::Minutes { interval: 0 }, None));
        assert!(p.next_runs.is_empty());
    }
}
