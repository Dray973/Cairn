//! Starter profiles built into Cairn: only catalog tweak ids and catalog package patterns of
//! Low risk, with no startup, DNS, Windows Update or maintenance section, so they fit every
//! PC.

use serde::Serialize;

use super::format::{to_text, Profile, SectionCounts, FORMAT, SCHEMA};

/// A starter profile as the Profiles section lists it.
#[derive(Debug, Clone, Serialize)]
pub struct StarterSummary {
    /// Stable id: `gaming`, `privacy` or `clean`.
    pub id: &'static str,
    pub name: String,
    pub description: String,
    pub counts: SectionCounts,
    /// The profile as a file would hold it.
    pub text: String,
}

struct Starter {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    tweaks: &'static [&'static str],
    apps: &'static [&'static str],
}

const STARTERS: &[Starter] = &[
    Starter {
        id: "gaming",
        name: "Gaming",
        description: "Game Mode on, background recording off, games first in multimedia \
                      scheduling, raw mouse input, no accessibility-shortcut prompts, and less \
                      background work from Edge and update sharing.",
        tweaks: &[
            "gaming.background_recording",
            "gaming.game_mode",
            "gaming.multimedia_scheduling",
            "gaming.mouse_acceleration",
            "gaming.sticky_keys",
            "gaming.network_throttling",
            "performance.startup_delay",
            "performance.edge_background",
            "performance.delivery_optimization",
            "performance.menu_delay",
        ],
        apps: &[],
    },
    Starter {
        id: "privacy",
        name: "Privacy",
        description: "Limits diagnostic data and ads across Windows, Office and Edge, turns off \
                      activity, typing and app-launch tracking, error reports and feedback \
                      prompts, and removes Cortana, Feedback Hub and the Bing app.",
        tweaks: &[
            "privacy.diagnostic_data",
            "privacy.diagtrack_service",
            "privacy.activity_history",
            "privacy.cortana",
            "privacy.advertising_id",
            "privacy.tailored_experiences",
            "privacy.suggested_content",
            "privacy.web_search",
            "privacy.feedback_prompts",
            "privacy.feedback_tasks",
            "privacy.recall",
            "privacy.app_launch_tracking",
            "privacy.typing_data",
            "privacy.online_speech",
            "privacy.error_reporting",
            "privacy.error_report_task",
            "privacy.ceip",
            "privacy.ceip_tasks",
            "privacy.lock_screen_ads",
            "privacy.office_telemetry",
            "privacy.edge_telemetry",
        ],
        apps: &[
            "Microsoft.549981C3F5F10",
            "Microsoft.WindowsFeedbackHub",
            "Microsoft.BingSearch",
        ],
    },
    Starter {
        id: "clean",
        name: "Clean",
        description: "Removes promoted games, streaming and rarely used preinstalled apps and \
                      hides Widgets, the Task View button, suggested apps and lock screen ads. \
                      The Store, Xbox, media and Office apps stay.",
        tweaks: &[
            "interface.widgets",
            "interface.task_view_button",
            "interface.search_icon",
            "privacy.suggested_content",
            "privacy.lock_screen_ads",
        ],
        apps: &[
            "king.com.*",
            "Microsoft.MicrosoftSolitaireCollection",
            "Microsoft.ZuneVideo",
            "Disney.*",
            "AmazonVideo.PrimeVideo",
            "BytedancePte.Ltd.TikTok",
            "Facebook.*",
            "Microsoft.BingNews",
            "Microsoft.BingWeather",
            "Microsoft.GetHelp",
            "Microsoft.Getstarted",
            "Microsoft.MicrosoftOfficeHub",
            "Microsoft.People",
            "Microsoft.WindowsMaps",
            "Microsoft.Windows.DevHome",
            "MicrosoftTeams",
            "Microsoft.SkypeApp",
            "Microsoft.MixedReality.Portal",
            "Microsoft.Microsoft3DViewer",
            "Clipchamp.Clipchamp",
            "Microsoft.XboxApp",
        ],
    },
];

fn profile_of(starter: &Starter) -> Profile {
    Profile {
        format: FORMAT.to_string(),
        schema: SCHEMA,
        name: starter.name.to_string(),
        description: starter.description.to_string(),
        created: None,
        created_with: None,
        tweaks: starter.tweaks.iter().map(|s| s.to_string()).collect(),
        apps: starter.apps.iter().map(|s| s.to_string()).collect(),
        startup: Vec::new(),
        dns: Default::default(),
        windows_update: None,
        maintenance: None,
    }
}

/// The starter profiles: gaming, privacy, clean (in this order).
pub fn starters() -> Vec<StarterSummary> {
    STARTERS
        .iter()
        .map(|s| {
            let profile = profile_of(s);
            StarterSummary {
                id: s.id,
                name: profile.name.clone(),
                description: profile.description.clone(),
                counts: SectionCounts::of(&profile),
                text: to_text(&profile),
            }
        })
        .collect()
}

/// The starter profile `id` (ASCII case ignored).
pub fn starter(id: &str) -> Option<Profile> {
    STARTERS
        .iter()
        .find(|s| s.id.eq_ignore_ascii_case(id.trim()))
        .map(profile_of)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debloat::catalog::{self, Risk, BLOAT_PACKAGES};
    use crate::profiles::format::parse;

    #[test]
    fn starters_parse_and_are_low_risk_catalog_items() {
        for summary in starters() {
            let parsed = parse(&summary.text).unwrap();
            assert_eq!(parsed, starter(summary.id).unwrap(), "{}", summary.id);
            assert_eq!(SectionCounts::of(&parsed), summary.counts);
            for id in &parsed.tweaks {
                let tweak =
                    catalog::tweak(id).unwrap_or_else(|| panic!("{id} is not in the catalog"));
                assert_eq!(tweak.risk, Risk::Low, "{id}");
            }
            for app in &parsed.apps {
                let entry = BLOAT_PACKAGES
                    .iter()
                    .find(|b| b.name.eq_ignore_ascii_case(app))
                    .unwrap_or_else(|| panic!("{app} is not a catalog package"));
                assert_eq!(entry.risk, Risk::Low, "{app}");
                assert!(
                    !catalog::is_protected_package(app.trim_end_matches('*')),
                    "{app}"
                );
            }
        }
        let privacy = starter("privacy").unwrap();
        assert_eq!(privacy.tweaks.len(), 21);
        let low_default_privacy: Vec<&str> = catalog::TWEAKS
            .iter()
            .filter(|t| {
                t.category == catalog::Category::Privacy
                    && t.risk == Risk::Low
                    && t.default_on
                    && t.requires.is_none()
            })
            .map(|t| t.id)
            .collect();
        assert_eq!(low_default_privacy.len(), 19);
        for id in low_default_privacy {
            assert!(privacy.tweaks.iter().any(|t| t == id), "{id}");
        }
        assert!(privacy
            .tweaks
            .iter()
            .any(|t| t == "privacy.office_telemetry"));
        assert!(privacy.tweaks.iter().any(|t| t == "privacy.edge_telemetry"));
    }

    #[test]
    fn starter_apps_are_catalog_patterns() {
        for summary in starters() {
            for app in starter(summary.id).unwrap().apps {
                assert!(
                    BLOAT_PACKAGES.iter().any(|b| b.name == app),
                    "{app} is not spelled like its catalog entry"
                );
            }
        }
        let clean = starter("clean").unwrap();
        assert_eq!(clean.apps.len(), 21);
        for kept in [
            "Microsoft.Copilot",
            "Microsoft.PowerAutomateDesktop",
            "Microsoft.XboxGamingOverlay",
            "Microsoft.GamingApp",
            "Microsoft.ZuneMusic",
            "MSTeams",
            "Microsoft.OutlookForWindows",
            "Microsoft.YourPhone",
            "Microsoft.MicrosoftStickyNotes",
        ] {
            assert!(!clean.apps.iter().any(|a| a == kept), "{kept}");
        }
    }

    #[test]
    fn starters_have_no_machine_specific_sections() {
        for summary in starters() {
            let p = starter(summary.id).unwrap();
            assert!(p.startup.is_empty() && p.dns.is_empty(), "{}", summary.id);
            assert!(
                p.windows_update.is_none() && p.maintenance.is_none(),
                "{}",
                summary.id
            );
            assert!(
                p.created.is_none() && p.created_with.is_none(),
                "{}",
                summary.id
            );
            assert_eq!(summary.counts.startup + summary.counts.dns, 0);
            assert_eq!(
                summary.counts.windows_update + summary.counts.maintenance,
                0
            );
            assert!(!summary.text.contains("created"), "{}", summary.text);
        }
    }

    #[test]
    fn starter_ids_are_stable() {
        let ids: Vec<&str> = starters().iter().map(|s| s.id).collect();
        assert_eq!(ids, ["gaming", "privacy", "clean"]);
        let names: Vec<String> = starters().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["Gaming", "Privacy", "Clean"]);
        assert_eq!(starter("GAMING").unwrap().name, "Gaming");
        assert_eq!(starter(" clean ").unwrap().name, "Clean");
        assert!(starter("work").is_none());
        let gaming = &starters()[0];
        assert_eq!(gaming.counts.tweaks, 10);
        assert_eq!(gaming.counts.apps, 0);
    }
}
