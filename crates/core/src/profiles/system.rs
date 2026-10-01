//! Everything profiles read from or change on the system, behind one trait so the plan and
//! apply algorithms run against a fake in tests.

use std::sync::Arc;

use super::step::{MaintenanceChoice, SettingStep, StepResult, WindowsUpdateChoice};
use crate::debloat::{ApplyReport, Engine, ScanReport};
use crate::network::{self, DnsReport, DnsRequest, NetworkReport};
use crate::safety::rollback::RollbackFilter;
use crate::safety::state_log::Journal;
use crate::safety::{self, MutationOutcome, Safety};
use crate::startup::{self, StartupEntry};
use crate::updates::wu::WuSettingId;
use crate::{maintenance, updates, Result};

/// Reads and changes of a profile. Production: [`LiveProfiles`]; tests: `fake::FakeProfiles`.
/// Every mutation runs under the caller's session.
pub(crate) trait ProfileSystem {
    /// Whether this process is elevated.
    fn elevated(&self) -> bool;
    /// Ok when per-user changes reach the signed-in user.
    fn per_user_allowed(&self) -> Result<()>;
    fn journal(&self) -> &Journal;
    /// Catalog tweaks and Store apps with their state.
    fn scan(&self) -> Result<ScanReport>;
    fn startup(&self) -> Result<Vec<StartupEntry>>;
    fn network(&self) -> Result<NetworkReport>;
    /// What a DNS change would do. Read-only.
    fn plan_dns(&self, adapter_id: &str, request: &DnsRequest) -> Result<DnsReport>;
    /// The Windows Update settings Cairn set that still differ from their baseline.
    fn wu_current(&self) -> Result<WindowsUpdateChoice>;
    /// Row keys (`windows_update:<field>`) of the Windows Update settings with an active
    /// journal record, whatever their current value.
    fn wu_recorded(&self) -> Result<Vec<String>>;
    fn wu_plan(&self, want: &WindowsUpdateChoice) -> Result<Vec<SettingStep>>;
    /// The recorded, enabled maintenance schedule.
    fn maintenance_current(&self) -> Result<Option<MaintenanceChoice>>;
    fn maintenance_plan(&self, want: &MaintenanceChoice) -> Result<Vec<SettingStep>>;
    fn apply_items(&self, safety: &Safety, ids: &[String]) -> Result<ApplyReport>;
    /// Journal targets of catalog item ids; reads only the catalog and the journal.
    fn revert_filter(&self, ids: &[String]) -> Result<RollbackFilter>;
    fn set_startup(&self, safety: &Safety, id: &str, enabled: bool) -> Result<MutationOutcome>;
    fn set_dns(&self, safety: &Safety, adapter_id: &str, request: &DnsRequest)
        -> Result<DnsReport>;
    fn wu_apply(
        &self,
        safety: &Safety,
        want: &WindowsUpdateChoice,
        keys: &[String],
    ) -> Result<(Vec<StepResult>, RollbackFilter)>;
    fn maintenance_apply(
        &self,
        safety: &Safety,
        want: &MaintenanceChoice,
    ) -> Result<(Vec<StepResult>, RollbackFilter)>;
}

/// The real system, through the engine, startup, network, Windows Update and maintenance
/// modules.
#[derive(Debug)]
pub(crate) struct LiveProfiles {
    engine: Engine,
}

impl LiveProfiles {
    pub(crate) fn new(journal: Arc<Journal>) -> LiveProfiles {
        LiveProfiles {
            engine: Engine::new(journal),
        }
    }
}

impl ProfileSystem for LiveProfiles {
    fn elevated(&self) -> bool {
        crate::is_elevated()
    }

    fn per_user_allowed(&self) -> Result<()> {
        safety::check_interactive_user()
    }

    fn journal(&self) -> &Journal {
        self.engine.journal()
    }

    fn scan(&self) -> Result<ScanReport> {
        self.engine.scan()
    }

    fn startup(&self) -> Result<Vec<StartupEntry>> {
        startup::list()
    }

    fn network(&self) -> Result<NetworkReport> {
        network::list(Some(self.engine.journal()))
    }

    fn plan_dns(&self, adapter_id: &str, request: &DnsRequest) -> Result<DnsReport> {
        network::plan_dns(adapter_id, request)
    }

    fn wu_current(&self) -> Result<WindowsUpdateChoice> {
        updates::wu::profile_current(self.engine.journal())
    }

    fn wu_recorded(&self) -> Result<Vec<String>> {
        let state = updates::wu::wu_state(Some(self.engine.journal()))?;
        Ok(state
            .settings
            .iter()
            .filter(|s| s.by_cairn && s.id != WuSettingId::Pause)
            .map(|s| format!("windows_update:{}", s.id.as_str()))
            .collect())
    }

    fn wu_plan(&self, want: &WindowsUpdateChoice) -> Result<Vec<SettingStep>> {
        updates::wu::profile_plan(self.engine.journal(), want)
    }

    fn maintenance_current(&self) -> Result<Option<MaintenanceChoice>> {
        maintenance::profile_current(self.engine.journal())
    }

    fn maintenance_plan(&self, want: &MaintenanceChoice) -> Result<Vec<SettingStep>> {
        maintenance::profile_plan(self.engine.journal(), want)
    }

    fn apply_items(&self, safety: &Safety, ids: &[String]) -> Result<ApplyReport> {
        self.engine.apply_in(safety, ids)
    }

    fn revert_filter(&self, ids: &[String]) -> Result<RollbackFilter> {
        self.engine.revert_filter(ids)
    }

    fn set_startup(&self, safety: &Safety, id: &str, enabled: bool) -> Result<MutationOutcome> {
        startup::set_enabled(safety, id, enabled)
    }

    fn set_dns(
        &self,
        safety: &Safety,
        adapter_id: &str,
        request: &DnsRequest,
    ) -> Result<DnsReport> {
        network::set_dns(safety, adapter_id, request)
    }

    fn wu_apply(
        &self,
        safety: &Safety,
        want: &WindowsUpdateChoice,
        keys: &[String],
    ) -> Result<(Vec<StepResult>, RollbackFilter)> {
        updates::wu::profile_apply_in(safety, want, keys)
    }

    fn maintenance_apply(
        &self,
        safety: &Safety,
        want: &MaintenanceChoice,
    ) -> Result<(Vec<StepResult>, RollbackFilter)> {
        maintenance::profile_apply_in(safety, want)
    }
}
