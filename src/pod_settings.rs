//! Pod-wide preferences: the answers that are the same for everything running
//! here, kept in one place instead of asked for once per thing.
//!
//! There is one so far, and it earns the file on its own. A cron schedule with
//! no timezone used to be evaluated on the pod's own clock — UTC in the cluster
//! — so "every day at 08:00" meant 4am in Detroit unless whoever armed it
//! happened to also type a zone into a free-text box. Both clients did type one,
//! from the browser or the phone, which hid the problem rather than fixing it:
//! anything else that armed a schedule (the agent's own `flow_set_schedules`, a
//! pack suggestion, a hand-written document) got UTC and no warning.
//!
//! A pod-level timezone gives "unset" a sane meaning — *the person's* zone, not
//! the datacentre's — and gives a schedule editor something to default to and to
//! say out loud.

use serde::{Deserialize, Serialize};

use crate::paths;

/// Everything a pod is configured to prefer.
///
/// Every field optional and every absence meaningful, because this file is
/// written by whichever client last saved and read by a pod that may be newer
/// than it: an unknown field must not become an opinion.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PodSettings {
    /// IANA timezone this pod's people live in — `"America/Detroit"`.
    ///
    /// Used for a cron schedule that names no zone of its own. `None` falls back
    /// to the host clock, which is what every pod did before this existed and is
    /// almost never what anybody meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Read the settings. A missing or unreadable file is the default, not an error:
/// a pod with no preferences is the normal state of a new one.
pub fn load() -> PodSettings {
    std::fs::read_to_string(paths::pod_settings_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write the settings, rejecting a timezone this pod cannot resolve.
///
/// Same rule as a schedule's own zone: a name that does not resolve would
/// silently mean UTC, and an hour that is quietly wrong is worse than a save
/// that visibly failed.
pub fn save(settings: &PodSettings) -> Result<(), String> {
    if let Some(zone) = &settings.timezone {
        zone.parse::<chrono_tz::Tz>().map_err(|_| {
            format!(
                "unknown timezone '{zone}': use an IANA name like 'America/Detroit' \
                 (case-sensitive), not an abbreviation or a UTC offset"
            )
        })?;
    }
    let path = paths::pod_settings_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// The zone a schedule that names none should be read in: the pod's, if it has
/// one and it resolves.
///
/// Returns the name rather than a parsed zone so the caller can log what it
/// used, and so the one place that turns a name into a zone stays
/// [`crate::schedule_timing`].
pub fn default_timezone() -> Option<String> {
    load()
        .timezone
        .filter(|z| z.parse::<chrono_tz::Tz>().is_ok())
}

/// Every timezone this pod can actually resolve, grouped by region.
///
/// Published so a client's picker offers exactly what the pod accepts. The
/// alternative — each client using its own platform's list — drifts: a zone the
/// phone knows about and this build of the tz database does not is a save that
/// fails after the person chose it, which is a worse experience than not being
/// offered it.
///
/// Fixed-offset entries are left out on purpose. `EST`, `PST8PDT` and the
/// `Etc/GMT+5` family are real zones that never observe daylight saving, so a
/// schedule on one drifts an hour away from the life it was supposed to match
/// every spring. Anyone who genuinely wants a fixed offset can still save one;
/// it is simply not something to be offered by a list whose whole job is to keep
/// people out of that trap. `UTC` stays, because meaning it is common and
/// unambiguous.
pub fn known_zones() -> Vec<TimezoneRegion> {
    let mut regions: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for tz in chrono_tz::TZ_VARIANTS {
        let name = tz.name();
        let Some((region, _)) = name.split_once('/') else {
            continue;
        };
        if region == "Etc" {
            continue;
        }
        regions.entry(region).or_default().push(name.to_string());
    }
    let mut out = vec![TimezoneRegion {
        region: "UTC".into(),
        zones: vec!["UTC".into()],
    }];
    out.extend(regions.into_iter().map(|(region, mut zones)| {
        zones.sort();
        zones.dedup();
        TimezoneRegion {
            region: region.to_string(),
            zones,
        }
    }));
    out
}

/// One region's worth of timezones, for a picker that groups them.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TimezoneRegion {
    /// The IANA area — `"America"`, `"Europe"` — or `"UTC"`.
    pub region: String,
    /// Full zone names in that area, sorted: `"America/Detroit"`.
    pub zones: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offered_zones_all_resolve_and_none_of_them_are_offset_traps() {
        let regions = known_zones();
        assert!(regions.len() > 5, "several regions");
        for region in &regions {
            assert!(!region.zones.is_empty(), "{} is empty", region.region);
            for zone in &region.zones {
                assert!(
                    zone.parse::<chrono_tz::Tz>().is_ok(),
                    "offered {zone}, which this pod cannot resolve"
                );
            }
        }
        let all: Vec<&String> = regions.iter().flat_map(|r| &r.zones).collect();
        assert!(all.iter().any(|z| *z == "America/Detroit"));
        assert!(all.iter().any(|z| *z == "Europe/London"));
        assert!(all.iter().any(|z| *z == "UTC"));
        // The traps: real zones that never move with the clocks, so a schedule
        // on one is an hour wrong for half the year.
        for trap in ["EST", "PST8PDT", "MST", "Etc/GMT+5"] {
            assert!(
                !all.iter().any(|z| z.as_str() == trap),
                "{trap} should not be offered"
            );
        }
    }

    #[test]
    fn a_timezone_that_does_not_resolve_is_refused() {
        let bad = PodSettings {
            timezone: Some("america/detroit".into()),
        };
        assert!(save(&bad).unwrap_err().contains("america/detroit"));
    }
}
