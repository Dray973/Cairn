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
        description: "Game Mode on, background recording off, raw mouse input, no \
                      accessibility-shortcut prompts, and less background work from Edge and \
                      update sharing.",
        tweaks: &[
            "gaming.background_recording",
            "gaming.game_mode",
            "gaming.mouse_acceleration",
            "gaming.sticky_keys",
            "performance.edge_background",
            "performance.delivery_optimization",
            "performance.menu_delay",
        ],
        apps: &[],
    },
    Starter {
        id: "privacy",
        name: "Privacy",
        description: "Limits diagnostic data and ads across Windows and Edge, turns off \
                      app-launch tracking, error reports, the feedback survey tasks, Start \
                      suggestions, setup reminders and lock-screen notifications, and removes \
                      Cortana, Feedback Hub and the Bing app.",
        tweaks: &[
            "privacy.diagnostic_data",
            "privacy.diagtrack_service",
            "privacy.advertising_id",
            "privacy.tailored_experiences",
            "privacy.suggested_content",
            "privacy.feedback_tasks",
            "privacy.app_launch_tracking",
            "privacy.error_reporting",
            "privacy.error_report_task",
            "privacy.ceip_tasks",
            "privacy.lock_screen_ads",
            "privacy.start_suggestions",
            "privacy.setup_reminders",
            "privacy.lock_screen_notifications",
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
                      hides the Task View button, suggested apps, lock screen ads and sync app \
                      notices in File Explorer. The Store, Xbox, media and Office apps stay.",
        tweaks: &[
            "interface.task_view_button",
            "interface.search_icon",
            "interface.explorer_sync_ads",
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
        assert_eq!(privacy.tweaks.len(), 15);
        let low_default_privacy: Vec<&str> = catalog::TWEAKS
            .iter()
            .filter(|t| {
                t.category == catalog::Category::Privacy
                    && t.risk == Risk::Low
                    && t.default_on
                    && t.requires.is_none()
                    && t.retired.is_none()
            })
            .map(|t| t.id)
            .collect();
        assert_eq!(low_default_privacy.len(), 14);
        for id in low_default_privacy {
            assert!(privacy.tweaks.iter().any(|t| t == id), "{id}");
        }
        // Tweaks whose effect no available PC could show are not recommended, so the starter
        // leaves them out too.
        for id in [
            "privacy.feedback_prompts",
            "privacy.typing_data",
            "privacy.online_speech",
        ] {
            assert!(!privacy.tweaks.iter().any(|t| t == id), "{id}");
        }
        // Office policies are offered for business and enterprise Office only and are not
        // recommended, so the starter leaves them out; Edge is on every PC.
        assert!(!privacy
            .tweaks
            .iter()
            .any(|t| t == "privacy.office_telemetry"));
        assert!(privacy.tweaks.iter().any(|t| t == "privacy.edge_telemetry"));
    }

    #[test]
    fn starters_hold_only_offered_recommended_tweaks() {
        for summary in starters() {
            for id in starter(summary.id).unwrap().tweaks {
                let tweak = catalog::tweak(&id).unwrap();
                assert!(tweak.retired.is_none(), "{}: {id} is retired", summary.id);
                assert!(
                    tweak.undo_limit.is_none(),
                    "{}: {id} has an undo limit",
                    summary.id
                );
            }
        }
        // Every tweak the Privacy starter applies is recommended; the Gaming and Clean
        // starters also pick Low-risk tweaks that are not.
        for id in starter("privacy").unwrap().tweaks {
            assert!(catalog::tweak(&id).unwrap().default_on, "{id}");
        }
    }

    #[test]
    fn starter_descriptions_name_what_they_change() {
        let privacy = starter("privacy").unwrap();
        for group in [
            "Start suggestions",
            "setup reminders",
            "lock-screen notifications",
        ] {
            assert!(privacy.description.contains(group), "{group}");
        }
        for id in [
            "privacy.start_suggestions",
            "privacy.setup_reminders",
            "privacy.lock_screen_notifications",
        ] {
            assert!(privacy.tweaks.iter().any(|t| t == id), "{id}");
        }
        for retired in [
            "privacy.cortana",
            "privacy.activity_history",
            "privacy.web_search",
            "privacy.ceip",
        ] {
            assert!(!privacy.tweaks.iter().any(|t| t == retired), "{retired}");
        }
        assert!(!privacy.tweaks.iter().any(|t| t == "privacy.recall"));
        for gone in ["search highlights", "Office"] {
            assert!(!privacy.description.contains(gone), "{gone}");
        }

        let clean = starter("clean").unwrap();
        assert!(clean
            .tweaks
            .iter()
            .any(|t| t == "interface.explorer_sync_ads"));
        assert!(clean
            .description
            .contains("sync app notices in File Explorer"));
        assert!(!clean.description.contains("Widgets"));
        assert_eq!(clean.tweaks.len(), 5);

        let gaming = starter("gaming").unwrap();
        assert!(!gaming.description.contains("multimedia scheduling"));
        for retired in [
            "gaming.multimedia_scheduling",
            "gaming.network_throttling",
            "performance.startup_delay",
        ] {
            assert!(!gaming.tweaks.iter().any(|t| t == retired), "{retired}");
        }
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
        assert_eq!(gaming.counts.tweaks, 7);
        assert_eq!(gaming.counts.apps, 0);
    }
}
