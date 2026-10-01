//! Read-only Windows Update Agent status, history and search.
//!
//! Status reads `IAutomaticUpdates2::Results`, `ISystemInformation::RebootRequired` and the
//! update history of `IUpdateSearcher`; the search runs `BeginSearch` for software updates
//! that are not installed. Nothing here downloads or installs anything, and the online search
//! only refreshes Windows Update's own metadata.
//!
//! Creating any of these objects can start the demand-start Windows Update service, so they
//! are created only by the security checkup's live readers. A search refuses to start while
//! [`search_forbidden`] is true: in test builds, and whenever `OPTIMIZER_FORBID_UPDATE_SEARCH`
//! is "1" (cargo and pytest runs set it). The calling thread must be in a COM apartment (see
//! `win::com`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use windows::core::{implement, Interface, Ref, BSTR};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::UpdateAgent::{
    orcAborted, orcSucceeded, orcSucceededWithErrors, uoInstallation, AutomaticUpdates,
    IAutomaticUpdates2, ICategoryCollection, ISearchCompletedCallback,
    ISearchCompletedCallbackArgs, ISearchCompletedCallback_Impl, ISearchJob, IStringCollection,
    ISystemInformation, IUpdate, IUpdateHistoryEntry2, IUpdateSearcher, IUpdateSession,
    OperationResultCode, SystemInformation, UpdateSession,
};
use windows::Win32::System::Variant::{VARIANT, VT_DATE};

use crate::{Error, Result};

/// Variable that, when "1", makes every Windows Update search refuse to start.
pub const FORBID_ENV: &str = "OPTIMIZER_FORBID_UPDATE_SEARCH";

/// True when Windows Update searches must not run: in test builds and while
/// `OPTIMIZER_FORBID_UPDATE_SEARCH` is "1".
pub fn search_forbidden() -> bool {
    cfg!(test) || std::env::var(FORBID_ENV).is_ok_and(|v| v.trim() == "1")
}

/// Criteria of the search: software updates that are neither installed nor hidden.
const SEARCH_CRITERIA: &str = "IsInstalled=0 and IsHidden=0 and Type='Software'";
/// Poll interval of a running search.
const POLL: Duration = Duration::from_millis(200);
/// How long an aborted search may take to end.
const ABORT_WAIT: Duration = Duration::from_secs(10);

/// What Windows Update reports about itself.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct UpdateStatus {
    /// Last successful search of the Automatic Updates client.
    pub(crate) last_search: Option<DateTime<Utc>>,
    /// Last successful installation of the Automatic Updates client (any update).
    pub(crate) last_install: Option<DateTime<Utc>>,
    pub(crate) reboot_required: Option<bool>,
    /// Newest entries first, as the agent returns them.
    pub(crate) history: Vec<HistoryEntry>,
}

/// One entry of the update history.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct HistoryEntry {
    pub(crate) title: String,
    pub(crate) date: Option<DateTime<Utc>>,
    /// An installation (not an uninstallation).
    pub(crate) installation: bool,
    /// Succeeded, with or without errors.
    pub(crate) succeeded: bool,
    pub(crate) hresult: i32,
    /// Category GUIDs, as the agent formats them. Windows 11 records its own updates with no
    /// categories or with categories whose id is empty.
    pub(crate) category_ids: Vec<String>,
    /// Id of the update service the update came from, as the agent formats it; empty when
    /// none is recorded.
    pub(crate) service_id: String,
    /// The update's support page; empty when none is recorded.
    pub(crate) support_url: String,
}

/// An update a search found.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct FoundUpdate {
    pub(crate) title: String,
    /// Knowledge-base article numbers without the "KB" prefix.
    pub(crate) kb: Vec<String>,
    pub(crate) msrc_severity: Option<String>,
    pub(crate) category_ids: Vec<String>,
    pub(crate) downloaded: bool,
    /// When the update was last published or changed.
    pub(crate) released_at: Option<DateTime<Utc>>,
}

/// How a search ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SearchOutcome {
    Found(Vec<FoundUpdate>),
    Cancelled,
    TimedOut,
}

/// Status, the reboot flag and the newest history entries, read page by page (see
/// [`read_history`]) until a page holds an entry `enough` accepts, the history ends or
/// `history_max` entries were read. Only the history is required; the Automatic Updates
/// results and the reboot flag are `None` when they cannot be read.
pub(crate) fn status(
    history_max: i32,
    enough: &dyn Fn(&HistoryEntry) -> bool,
) -> Result<UpdateStatus> {
    let (last_search, last_install) = automatic_updates_results().unwrap_or((None, None));
    let reboot_required = reboot_required().ok();
    let searcher = searcher()?;
    // SAFETY: `searcher` is a live interface; the call only returns a value.
    let total = unsafe { searcher.GetTotalHistoryCount() }?;
    let history = read_history(total, history_max, enough, |start, count| {
        history_page(&searcher, start, count)
    })?;
    Ok(UpdateStatus {
        last_search,
        last_install,
        reboot_required,
        history,
    })
}

/// History entries read per `QueryHistory` call.
const HISTORY_PAGE: i32 = 100;

/// The newest history entries, [`HISTORY_PAGE`] at a time through `page(start, count)` (index
/// 0 is the newest entry): until a page holds an entry `enough` accepts, a page comes back
/// short because the history ended, or `max` of the `total` entries were read.
fn read_history(
    total: i32,
    max: i32,
    enough: &dyn Fn(&HistoryEntry) -> bool,
    mut page: impl FnMut(i32, i32) -> Result<Vec<HistoryEntry>>,
) -> Result<Vec<HistoryEntry>> {
    let limit = total.min(max).max(0);
    let mut history = Vec::new();
    let mut start = 0;
    while start < limit {
        let count = HISTORY_PAGE.min(limit - start);
        let entries = page(start, count)?;
        let ended = entries.len() < count as usize;
        let found = entries.iter().any(enough);
        history.extend(entries);
        if ended || found {
            break;
        }
        start += count;
    }
    Ok(history)
}

/// `count` history entries from index `start`.
fn history_page(searcher: &IUpdateSearcher, start: i32, count: i32) -> Result<Vec<HistoryEntry>> {
    // SAFETY: the range lies within the history the agent reported.
    let entries = unsafe { searcher.QueryHistory(start, count) }?;
    // SAFETY: `entries` is a live collection; the call only returns a value.
    let listed = unsafe { entries.Count() }?;
    let mut page = Vec::with_capacity(listed.max(0) as usize);
    for index in 0..listed {
        // SAFETY: `index` is below the collection's count.
        let entry = unsafe { entries.get_Item(index) }?;
        // SAFETY: `entry` is a live interface; each getter only returns a value, and the BSTRs
        // are owned and freed on drop.
        let (operation, result, hresult, date, title, service_id, support_url) = unsafe {
            (
                entry.Operation()?,
                entry.ResultCode()?,
                entry.HResult().unwrap_or(0),
                entry.Date().ok(),
                entry.Title().map(|t| t.to_string()).unwrap_or_default(),
                entry.ServiceID().map(|t| t.to_string()).unwrap_or_default(),
                entry
                    .SupportUrl()
                    .map(|t| t.to_string())
                    .unwrap_or_default(),
            )
        };
        let category_ids = entry
            .cast::<IUpdateHistoryEntry2>()
            .ok()
            // SAFETY: the cast interface is live; the call only returns a value.
            .and_then(|e2| unsafe { e2.Categories() }.ok())
            .map(|c| category_ids(&c))
            .unwrap_or_default();
        page.push(HistoryEntry {
            title,
            date: date.and_then(ole_date_to_utc),
            installation: operation == uoInstallation,
            succeeded: result == orcSucceeded || result == orcSucceededWithErrors,
            hresult,
            category_ids,
            service_id,
            support_url,
        });
    }
    Ok(page)
}

/// The Automatic Updates client's last successful search and installation.
type AutomaticUpdatesDates = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

fn automatic_updates_results() -> Result<AutomaticUpdatesDates> {
    // SAFETY: the caller's thread is in a COM apartment; the object is released on return.
    let au: IAutomaticUpdates2 = unsafe { CoCreateInstance(&AutomaticUpdates, None, CLSCTX_ALL) }?;
    // SAFETY: `au` is a live interface; each getter only returns a value.
    let results = unsafe { au.Results() }?;
    // SAFETY: as above.
    let search = unsafe { results.LastSearchSuccessDate() }.ok();
    // SAFETY: as above.
    let install = unsafe { results.LastInstallationSuccessDate() }.ok();
    Ok((
        search.as_ref().and_then(variant_date),
        install.as_ref().and_then(variant_date),
    ))
}

fn reboot_required() -> Result<bool> {
    // SAFETY: the caller's thread is in a COM apartment; the object is released on return.
    let info: ISystemInformation =
        unsafe { CoCreateInstance(&SystemInformation, None, CLSCTX_ALL) }?;
    // SAFETY: `info` is a live interface; the getter only returns a value.
    Ok(unsafe { info.RebootRequired() }?.as_bool())
}

/// A searcher of a session that names Cairn as its client.
fn searcher() -> Result<IUpdateSearcher> {
    // SAFETY: the caller's thread is in a COM apartment; the session is released on return
    // while the searcher keeps what it needs.
    let session: IUpdateSession = unsafe { CoCreateInstance(&UpdateSession, None, CLSCTX_ALL) }?;
    // SAFETY: the BSTR outlives the call.
    unsafe { session.SetClientApplicationID(&BSTR::from(crate::APP_NAME)) }?;
    // SAFETY: `session` is a live interface.
    Ok(unsafe { session.CreateUpdateSearcher() }?)
}

/// The date of a `VT_DATE` VARIANT; anything else is `None`.
fn variant_date(v: &VARIANT) -> Option<DateTime<Utc>> {
    if v.vt() != VT_DATE {
        return None;
    }
    // SAFETY: `vt` says the `date` field is the valid one.
    ole_date_to_utc(unsafe { v.Anonymous.Anonymous.Anonymous.date })
}

fn category_ids(categories: &ICategoryCollection) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: `categories` is a live collection; the calls only return values.
    let count = unsafe { categories.Count() }.unwrap_or(0);
    for index in 0..count {
        // SAFETY: `index` is below the collection's count.
        if let Ok(category) = unsafe { categories.get_Item(index) } {
            // SAFETY: `category` is a live interface.
            if let Ok(id) = unsafe { category.CategoryID() } {
                out.push(id.to_string());
            }
        }
    }
    out
}

fn strings(collection: &IStringCollection) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: `collection` is a live collection; the calls only return values.
    let count = unsafe { collection.Count() }.unwrap_or(0);
    for index in 0..count {
        // SAFETY: `index` is below the collection's count.
        if let Ok(value) = unsafe { collection.get_Item(index) } {
            out.push(value.to_string());
        }
    }
    out
}

fn found_update(update: &IUpdate) -> FoundUpdate {
    // SAFETY: `update` is a live interface; each getter only returns a value.
    unsafe {
        FoundUpdate {
            title: update.Title().map(|t| t.to_string()).unwrap_or_default(),
            kb: update
                .KBArticleIDs()
                .map(|c| strings(&c))
                .unwrap_or_default(),
            msrc_severity: update
                .MsrcSeverity()
                .ok()
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty()),
            category_ids: update
                .Categories()
                .map(|c| category_ids(&c))
                .unwrap_or_default(),
            downloaded: update.IsDownloaded().map(|b| b.as_bool()).unwrap_or(false),
            released_at: update
                .LastDeploymentChangeTime()
                .ok()
                .and_then(ole_date_to_utc),
        }
    }
}

/// Completion callback the agent requires; the search is polled instead.
#[implement(ISearchCompletedCallback)]
struct SearchDone;

impl ISearchCompletedCallback_Impl for SearchDone_Impl {
    fn Invoke(
        &self,
        _searchjob: Ref<'_, ISearchJob>,
        _callbackargs: Ref<'_, ISearchCompletedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// The calls the wait for a search makes on its job.
trait SearchJobCalls {
    fn is_completed(&self) -> Result<bool>;
    fn request_abort(&self) -> Result<()>;
    /// Waits for the operation to complete, then releases the completion callback.
    fn clean_up(&self);
}

impl SearchJobCalls for ISearchJob {
    fn is_completed(&self) -> Result<bool> {
        // SAFETY: `self` is a live interface; the call only returns a value.
        Ok(unsafe { self.IsCompleted() }?.as_bool())
    }

    fn request_abort(&self) -> Result<()> {
        // SAFETY: `self` is a live interface.
        unsafe { self.RequestAbort() }?;
        Ok(())
    }

    fn clean_up(&self) {
        // SAFETY: `self` is a live interface. A failure leaves nothing to undo: the job is
        // released when its last reference is dropped.
        let _ = unsafe { self.CleanUp() };
    }
}

/// How the wait for a search job ended.
#[derive(Debug, Clone, PartialEq)]
enum JobEnd {
    /// The job completed without being stopped; `EndSearch` reads its result.
    Completed,
    /// The job was stopped and ended within the abort wait; it has been cleaned up.
    Stopped(SearchOutcome),
    /// The job was stopped and had not ended when the abort wait ran out.
    Abandoned(SearchOutcome),
}

/// Polls `job` every `poll` until it completes. When `cancel` is set or `deadline` passes,
/// the job is asked to abort and gets at most `abort_wait` more to end. `CleanUp` waits for
/// the operation to complete, so it is called only on a completed job: a job that ignores the
/// abort is released without it and ends on its own.
fn wait_for_job(
    job: &dyn SearchJobCalls,
    cancel: &AtomicBool,
    deadline: Duration,
    poll: Duration,
    abort_wait: Duration,
) -> Result<JobEnd> {
    let started = Instant::now();
    let mut stopped: Option<(Instant, SearchOutcome)> = None;
    loop {
        if job.is_completed()? {
            return Ok(match stopped {
                None => JobEnd::Completed,
                Some((_, outcome)) => {
                    job.clean_up();
                    JobEnd::Stopped(outcome)
                }
            });
        }
        match &stopped {
            None => {
                let outcome = if cancel.load(Ordering::SeqCst) {
                    Some(SearchOutcome::Cancelled)
                } else if started.elapsed() >= deadline {
                    Some(SearchOutcome::TimedOut)
                } else {
                    None
                };
                if let Some(outcome) = outcome {
                    job.request_abort()?;
                    stopped = Some((Instant::now(), outcome));
                }
            }
            Some((at, outcome)) if at.elapsed() >= abort_wait => {
                return Ok(JobEnd::Abandoned(outcome.clone()));
            }
            Some(_) => {}
        }
        thread::sleep(poll);
    }
}

/// Searches for software updates that are not installed, offline (Windows Update's cached
/// metadata) or online. The search is aborted when `cancel` is set or `deadline` passes; an
/// aborted search gets at most 10 s more to end, and one that ignores the abort is released
/// without waiting for it. A search that failed, or ended with errors and found nothing, is an
/// error (see [`search_outcome`]). Refuses to start while [`search_forbidden`].
pub(crate) fn search(
    online: bool,
    cancel: &AtomicBool,
    deadline: Duration,
) -> Result<SearchOutcome> {
    if search_forbidden() {
        return Err(Error::Other(format!(
            "Windows Update searches are disabled in this environment ({FORBID_ENV})"
        )));
    }
    let searcher = searcher()?;
    // SAFETY: `searcher` is a live interface; the flag is passed by value.
    unsafe { searcher.SetOnline(online.into()) }?;
    let callback: ISearchCompletedCallback = SearchDone.into();
    let state = VARIANT::default();
    // SAFETY: the criteria, callback and state outlive the call; the job is polled below.
    let job = unsafe { searcher.BeginSearch(&BSTR::from(SEARCH_CRITERIA), &callback, &state) }?;
    match wait_for_job(&job, cancel, deadline, POLL, ABORT_WAIT)? {
        JobEnd::Completed => {}
        JobEnd::Stopped(outcome) | JobEnd::Abandoned(outcome) => return Ok(outcome),
    }
    // SAFETY: `job` completed; EndSearch reads its result.
    let result = unsafe { searcher.EndSearch(&job) };
    job.clean_up();
    let result = result?;
    // SAFETY: `result` is a live interface; the getter only returns a value.
    let code = unsafe { result.ResultCode() }?;
    search_outcome(code, || {
        // SAFETY: `result` is a live interface; the getter only returns a value.
        let updates = unsafe { result.Updates() }?;
        // SAFETY: `updates` is a live collection.
        let count = unsafe { updates.Count() }?;
        let mut found = Vec::with_capacity(count.max(0) as usize);
        for index in 0..count {
            // SAFETY: `index` is below the collection's count.
            let update = unsafe { updates.get_Item(index) }?;
            found.push(found_update(&update));
        }
        Ok(found)
    })
}

/// The outcome of a search that ended with `code`; `read` lists the updates it found and is
/// called only when they count. A search that ended with errors may have missed updates, so
/// it counts only when it still found some; any code but success, success with errors and
/// abort is an error.
fn search_outcome(
    code: OperationResultCode,
    read: impl FnOnce() -> Result<Vec<FoundUpdate>>,
) -> Result<SearchOutcome> {
    if code == orcSucceeded {
        Ok(SearchOutcome::Found(read()?))
    } else if code == orcAborted {
        Ok(SearchOutcome::Cancelled)
    } else if code == orcSucceededWithErrors {
        let found = read()?;
        if found.is_empty() {
            Err(Error::Other(
                "Windows Update reported errors while checking, so nothing can be said about \
                 waiting updates."
                    .into(),
            ))
        } else {
            Ok(SearchOutcome::Found(found))
        }
    } else {
        Err(Error::Other(format!(
            "Windows Update could not finish checking (result {}).",
            code.0
        )))
    }
}

/// Days since 1899-12-30T00:00Z (an OLE Automation date) as UTC; `None` for non-finite,
/// zero, negative or out-of-range values.
pub(crate) fn ole_date_to_utc(d: f64) -> Option<DateTime<Utc>> {
    // 9999-12-31 is day 2958465 of the OLE calendar.
    if !d.is_finite() || d <= 0.0 || d > 2_958_466.0 {
        return None;
    }
    let base = Utc.with_ymd_and_hms(1899, 12, 30, 0, 0, 0).single()?;
    let millis = (d * 86_400_000.0).round() as i64;
    base.checked_add_signed(ChronoDuration::milliseconds(millis))
}

/// User text for an HRESULT a Windows Update call failed with.
pub(crate) fn wu_error_text(hresult: i32) -> String {
    match hresult as u32 {
        0x8007_0422 => "The Windows Update service is disabled.".into(),
        0x8024_402C | 0x8007_2EE7 | 0x8007_2EFD | 0x8024_4022 | 0x8007_2EE2 | 0x8024_0438 => {
            "Windows Update could not be reached; check the internet connection.".into()
        }
        0x8024_001E => "Windows Update is shutting down; try again after a restart.".into(),
        0x8024_000B => "The check was cancelled.".into(),
        other => format!("Windows Update reported error 0x{other:08X}."),
    }
}

/// User text for an error of this module: [`wu_error_text`] for COM failures, the error's
/// own text otherwise.
pub(crate) fn error_text(e: &Error) -> String {
    match e {
        Error::Win32(inner) => wu_error_text(inner.code().0),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn ole_dates_convert_to_utc() {
        let expected = "2025-01-01T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(ole_date_to_utc(45658.5), Some(expected));
        assert_eq!(
            ole_date_to_utc(1.0),
            "1899-12-31T00:00:00Z".parse::<DateTime<Utc>>().ok()
        );
        assert_eq!(ole_date_to_utc(0.0), None);
        assert_eq!(ole_date_to_utc(-3.0), None);
        assert_eq!(ole_date_to_utc(f64::NAN), None);
        assert_eq!(ole_date_to_utc(f64::INFINITY), None);
        assert_eq!(ole_date_to_utc(1e12), None);
    }

    #[test]
    fn error_texts_name_the_cause() {
        assert_eq!(
            wu_error_text(0x8007_0422u32 as i32),
            "The Windows Update service is disabled."
        );
        for code in [
            0x8024_402Cu32,
            0x8007_2EE7,
            0x8007_2EFD,
            0x8024_4022,
            0x8007_2EE2,
            0x8024_0438,
        ] {
            assert_eq!(
                wu_error_text(code as i32),
                "Windows Update could not be reached; check the internet connection."
            );
        }
        assert_eq!(
            wu_error_text(0x8024_001Eu32 as i32),
            "Windows Update is shutting down; try again after a restart."
        );
        assert_eq!(
            wu_error_text(0x8024_000Bu32 as i32),
            "The check was cancelled."
        );
        assert_eq!(
            wu_error_text(0x8024_0FFFu32 as i32),
            "Windows Update reported error 0x80240FFF."
        );
        let com = Error::Win32(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x8007_0422u32 as i32,
        )));
        assert_eq!(error_text(&com), "The Windows Update service is disabled.");
        assert_eq!(error_text(&Error::Other("plain".into())), "plain");
    }

    /// A search job that completes on its own after `completes_after` polls, or
    /// `ends_after_abort` polls after it was asked to abort (never when `None`), and records
    /// the calls made on it.
    #[derive(Debug, Default)]
    struct FakeJob {
        completes_after: Option<u32>,
        ends_after_abort: Option<u32>,
        polls: Cell<u32>,
        polls_since_abort: Cell<Option<u32>>,
        completed: Cell<bool>,
        aborts: Cell<u32>,
        clean_ups: Cell<u32>,
    }

    impl SearchJobCalls for FakeJob {
        fn is_completed(&self) -> Result<bool> {
            let polls = self.polls.get() + 1;
            self.polls.set(polls);
            if self.completes_after.is_some_and(|n| polls > n) {
                self.completed.set(true);
            }
            if let Some(since) = self.polls_since_abort.get() {
                if self.ends_after_abort.is_some_and(|n| since >= n) {
                    self.completed.set(true);
                }
                self.polls_since_abort.set(Some(since + 1));
            }
            Ok(self.completed.get())
        }

        fn request_abort(&self) -> Result<()> {
            self.aborts.set(self.aborts.get() + 1);
            self.polls_since_abort.set(Some(0));
            Ok(())
        }

        fn clean_up(&self) {
            // The real call waits for a running job to complete.
            assert!(
                self.completed.get(),
                "CleanUp would wait for a running search"
            );
            self.clean_ups.set(self.clean_ups.get() + 1);
        }
    }

    const TICK: Duration = Duration::from_millis(1);
    const LONG: Duration = Duration::from_secs(600);

    #[test]
    fn a_completed_search_is_left_for_end_search() {
        let job = FakeJob {
            completes_after: Some(3),
            ..FakeJob::default()
        };
        let cancel = AtomicBool::new(false);
        let end = wait_for_job(&job, &cancel, LONG, TICK, LONG).unwrap();
        assert_eq!(end, JobEnd::Completed);
        assert_eq!(
            (job.polls.get(), job.aborts.get(), job.clean_ups.get()),
            (4, 0, 0)
        );
    }

    #[test]
    fn a_stopped_search_that_ends_is_cleaned_up() {
        let job = FakeJob {
            ends_after_abort: Some(2),
            ..FakeJob::default()
        };
        let cancel = AtomicBool::new(true);
        let end = wait_for_job(&job, &cancel, LONG, TICK, LONG).unwrap();
        assert_eq!(end, JobEnd::Stopped(SearchOutcome::Cancelled));
        assert_eq!((job.aborts.get(), job.clean_ups.get()), (1, 1));
        let job = FakeJob {
            ends_after_abort: Some(0),
            ..FakeJob::default()
        };
        let cancel = AtomicBool::new(false);
        let end = wait_for_job(&job, &cancel, Duration::ZERO, TICK, LONG).unwrap();
        assert_eq!(end, JobEnd::Stopped(SearchOutcome::TimedOut));
        assert_eq!((job.aborts.get(), job.clean_ups.get()), (1, 1));
    }

    #[test]
    fn a_search_that_ignores_the_abort_is_released_without_waiting_for_it() {
        let abort_wait = Duration::from_millis(40);
        for (cancelled, deadline, outcome) in [
            (true, LONG, SearchOutcome::Cancelled),
            (false, Duration::ZERO, SearchOutcome::TimedOut),
        ] {
            let job = FakeJob::default();
            let cancel = AtomicBool::new(cancelled);
            let started = Instant::now();
            let end = wait_for_job(&job, &cancel, deadline, TICK, abort_wait).unwrap();
            let took = started.elapsed();
            assert_eq!(end, JobEnd::Abandoned(outcome));
            assert_eq!((job.aborts.get(), job.clean_ups.get()), (1, 0));
            assert!(
                took >= abort_wait && took < Duration::from_secs(10),
                "{took:?}"
            );
        }
    }

    /// Reads a history of `total` entries titled "entry {index}" through `read_history`, where
    /// the entry at index `wanted` is enough and the page from index `short_at` comes back with
    /// 30 entries only. Returns the entries read and the pages asked for.
    fn paged(
        total: i32,
        max: i32,
        wanted: Option<i32>,
        short_at: Option<i32>,
    ) -> (Vec<HistoryEntry>, Vec<(i32, i32)>) {
        let entries: Vec<HistoryEntry> = (0..total.max(0))
            .map(|i| HistoryEntry {
                title: format!("entry {i}"),
                ..HistoryEntry::default()
            })
            .collect();
        let wanted = wanted.map(|i| format!("entry {i}"));
        let enough = |e: &HistoryEntry| Some(&e.title) == wanted.as_ref();
        let mut calls = Vec::new();
        let read = read_history(total, max, &enough, |start, count| {
            calls.push((start, count));
            let end = if short_at == Some(start) {
                start + 30
            } else {
                start + count
            };
            Ok(entries[start as usize..end as usize].to_vec())
        })
        .unwrap();
        (read, calls)
    }

    #[test]
    fn the_history_is_read_a_page_at_a_time_until_an_entry_is_enough() {
        // The entry wanted is on the second page.
        let (read, calls) = paged(450, 1000, Some(150), None);
        assert_eq!((read.len(), calls), (200, vec![(0, 100), (100, 100)]));
        // None is: up to the end of the history, or up to the most entries asked for.
        let (read, calls) = paged(450, 1000, None, None);
        assert_eq!(read.len(), 450);
        assert_eq!(
            calls,
            vec![(0, 100), (100, 100), (200, 100), (300, 100), (400, 50)]
        );
        let (read, calls) = paged(450, 150, None, None);
        assert_eq!((read.len(), calls), (150, vec![(0, 100), (100, 50)]));
        // A short page means the history ended.
        let (read, calls) = paged(450, 1000, None, Some(100));
        assert_eq!((read.len(), calls), (130, vec![(0, 100), (100, 100)]));
        // An empty history is not read.
        assert!(paged(0, 1000, None, None).1.is_empty());
        // A page that cannot be read fails the reading.
        let enough = |_: &HistoryEntry| false;
        let err = read_history(450, 1000, &enough, |start, count| {
            if start == 0 {
                Ok(vec![HistoryEntry::default(); count as usize])
            } else {
                Err(Error::Other("broken".into()))
            }
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "broken");
    }

    #[test]
    fn search_result_codes_map_to_outcomes() {
        use windows::Win32::System::UpdateAgent::{orcFailed, orcInProgress, orcNotStarted};
        let one = || {
            Ok(vec![FoundUpdate {
                title: "2026-09 Security Update".into(),
                ..FoundUpdate::default()
            }])
        };
        let none = || Ok(Vec::new());
        let unread = || -> Result<Vec<FoundUpdate>> { panic!("the updates are not read") };
        assert_eq!(
            search_outcome(orcSucceeded, none).unwrap(),
            SearchOutcome::Found(Vec::new())
        );
        assert!(matches!(
            search_outcome(orcSucceeded, one),
            Ok(SearchOutcome::Found(found)) if found.len() == 1
        ));
        // Errors with updates found: those updates are waiting, others may be missing.
        assert!(matches!(
            search_outcome(orcSucceededWithErrors, one),
            Ok(SearchOutcome::Found(found)) if found.len() == 1
        ));
        // Errors and nothing found: nothing is known.
        assert_eq!(
            search_outcome(orcSucceededWithErrors, none)
                .unwrap_err()
                .to_string(),
            "Windows Update reported errors while checking, so nothing can be said about \
             waiting updates."
        );
        assert_eq!(
            search_outcome(orcAborted, unread).unwrap(),
            SearchOutcome::Cancelled
        );
        for (code, number) in [(orcFailed, 4), (orcNotStarted, 0), (orcInProgress, 1)] {
            assert_eq!(
                search_outcome(code, unread).unwrap_err().to_string(),
                format!("Windows Update could not finish checking (result {number}).")
            );
        }
        // A failure to read the updates is the search's error.
        let broken = || -> Result<Vec<FoundUpdate>> { Err(Error::Other("broken".into())) };
        assert_eq!(
            search_outcome(orcSucceeded, broken)
                .unwrap_err()
                .to_string(),
            "broken"
        );
    }

    #[test]
    fn searches_are_refused_in_tests_before_any_com_call() {
        assert!(search_forbidden());
        let cancel = AtomicBool::new(false);
        let err = search(true, &cancel, Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains(FORBID_ENV), "{err}");
    }

    #[test]
    #[ignore = "creates Windows Update Agent objects, which can start the Windows Update service"]
    fn live_windows_update_status_reads() {
        let _com = crate::win::com::enter_mta();
        let status = status(100, &|_| false).unwrap();
        println!("Automatic Updates last search: {:?}", status.last_search);
        println!("Automatic Updates last install: {:?}", status.last_install);
        println!("reboot required: {:?}", status.reboot_required);
        println!("history entries: {}", status.history.len());
        // "counts" marks the entries the security checkup takes as security installs (see
        // health::updates::is_security_install).
        for entry in &status.history {
            println!(
                "{:?}  installation {}  succeeded {}  {}  service {:?}  support {:?}  \
                 categories {:?}  counts {}",
                entry.date,
                entry.installation,
                entry.succeeded,
                entry.title,
                entry.service_id,
                entry.support_url,
                entry.category_ids,
                crate::health::updates::is_security_install(entry)
            );
        }
        match crate::health::updates::last_installed(&status) {
            Some(entry) => println!(
                "the checkup's last installed: {} ({:?})",
                entry.title, entry.date
            ),
            None => println!("the checkup's last installed: none among these entries"),
        }
        let event = crate::win::event_log::query(
            "Microsoft-Windows-WindowsUpdateClient/Operational",
            "*[System[EventID=26]]",
            true,
        )
        .ok()
        .and_then(|mut q| q.next_batch(1).ok())
        .and_then(|events| {
            let system = crate::win::event_log::RenderContext::system().ok()?;
            let event = events.first()?;
            system.render(event).ok()?[crate::win::event_log::SYSTEM_TIME_CREATED].as_time()
        });
        println!("newest event 26 (search found updates): {event:?}");
    }
}
