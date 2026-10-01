//! Service optimizer: start type and running state of background services.

use serde::{Deserialize, Serialize};

use super::catalog::ServiceAction;
use super::{ActionState, ActionStatus};
use crate::safety::{MutationOutcome, Safety};
use crate::win::scm::{Scm, StartType, READ_ACCESS};
use crate::{Error, Result};

/// ERROR_DEPENDENT_SERVICES_RUNNING: a stop request was refused because running services
/// depend on this one.
const ERROR_DEPENDENT_SERVICES_RUNNING: u32 = 1051;

/// Result of applying one service action. `notes` explains partial results, such as a
/// service whose start type changed but which could not be stopped yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceOutcome {
    pub outcome: MutationOutcome,
    pub notes: Vec<String>,
}

pub fn describe(action: &ServiceAction) -> String {
    format!("service {}", action.name)
}

/// Compares the installed service with the target. The start type decides the state; a
/// running service only counts against a target of Disabled, because Windows starts
/// Manual services on demand.
pub fn status(action: &ServiceAction) -> Result<ActionStatus> {
    let scm = Scm::connect()?;
    let Some(svc) = scm.open(action.name, READ_ACCESS)? else {
        return Ok(ActionStatus::new(
            ActionState::Unavailable,
            format!("{} is not installed", describe(action)),
        ));
    };
    let cfg = svc.config()?;
    let running = svc.status()?.state.is_active();
    let start_matches = cfg.start_type == action.start;
    let stop_pending = action.stop && action.start == StartType::Disabled && running;
    let state = if start_matches && !stop_pending {
        ActionState::Applied
    } else {
        ActionState::NotApplied
    };
    let detail = format!(
        "{} ({}): {}, {}; target {}",
        describe(action),
        cfg.display_name,
        cfg.start_type.label(),
        if running { "running" } else { "stopped" },
        action.start.label(),
    );
    Ok(ActionStatus::new(state, detail))
}

/// Journals the service's configuration and running state, sets the target start type,
/// then stops it when requested. A service that is not installed is skipped. A stop that
/// is refused because other running services depend on it leaves the new start type in
/// place and reports that the service stops at the next restart.
pub fn apply(safety: &Safety, action: &ServiceAction) -> Result<ServiceOutcome> {
    safety.ensure_elevated()?;
    let mut notes = Vec::new();

    let start = match safety.set_service_start_type(action.name, action.start, Some(false)) {
        Ok(outcome) => outcome,
        Err(Error::ServiceNotFound(_)) => {
            return Ok(ServiceOutcome {
                outcome: MutationOutcome::Skipped(format!("{} is not installed", describe(action))),
                notes,
            });
        }
        Err(e) => return Err(e),
    };

    let stop = if action.stop {
        match safety.stop_service(action.name) {
            Ok(outcome) => outcome,
            Err(e) if e.win32_code() == Some(ERROR_DEPENDENT_SERVICES_RUNNING) => {
                let reason = format!(
                    "{} was left running because other running services depend on it; it \
                     stays stopped after the next restart",
                    describe(action)
                );
                notes.push(reason.clone());
                MutationOutcome::Skipped(reason)
            }
            Err(e) => return Err(e),
        }
    } else {
        MutationOutcome::AlreadyInDesiredState
    };

    if let MutationOutcome::Skipped(reason) = &stop {
        if !notes.contains(reason) {
            notes.push(reason.clone());
        }
    }

    let outcome = match (&start, &stop) {
        (MutationOutcome::Applied, _) | (_, MutationOutcome::Applied) => MutationOutcome::Applied,
        (_, MutationOutcome::Skipped(reason)) => MutationOutcome::Skipped(reason.clone()),
        _ => MutationOutcome::AlreadyInDesiredState,
    };
    Ok(ServiceOutcome { outcome, notes })
}
