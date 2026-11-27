use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Component, Path},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{Notify, futures::Notified},
    time::{Instant, sleep, sleep_until},
};
use tokio_util::sync::CancellationToken;

pub(crate) const CONCURRENT_MEDIA_LIMIT: usize = 8;
pub(crate) const SEQUENTIAL_MEDIA_LIMIT: usize = 32;
const API_LIMIT: usize = 8;
const API_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub enum RecoveryNotice {
    Transferring,
    Retrying { reason: String },
    RateLimited { seconds: u64 },
    NetworkBytes { bytes: u64 },
}

type Observer = Arc<dyn Fn(RecoveryNotice) + Send + Sync>;

#[derive(Clone, Default)]
pub struct RetryBudget {
    used: Arc<Mutex<usize>>,
    observer: Option<Observer>,
    pub(crate) defer_no_response: bool,
    pub(crate) single_attempt: bool,
}

impl RetryBudget {
    pub(crate) fn validation() -> Self {
        Self { single_attempt: true, ..Self::default() }
    }

    pub async fn wait(&self, reason: &str) -> bool {
        let delay = {
            let mut used = self.used.lock().unwrap();
            let Some(delay) = [2, 5].get(*used) else {
                return false;
            };
            *used += 1;
            Duration::from_secs(*delay) + Duration::from_millis(rand::random::<u64>() % 501)
        };
        if let Some(observer) = &self.observer {
            observer(RecoveryNotice::Retrying {
                reason: reason.to_owned(),
            });
        }
        sleep(delay).await;
        true
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryDto {
    pub cooldown_seconds: Option<u64>,
    pub retry_connection: bool,
    pub worker_cap: Option<usize>,
}

#[derive(Default)]
pub struct Recovery {
    state: Mutex<State>,
    changed: Notify,
    next_id: AtomicU64,
}

#[derive(Default)]
struct State {
    next_api_start: Option<Instant>,
    api_count: usize,
    file_count: usize,
    sequential_media: bool,
    media_limits: HashMap<u64, usize>,
    media_pause: CancellationToken,
    accounts: HashMap<u64, ApiGate>,
    hosts: HashMap<String, Cooldown>,
    drives: HashSet<String>,
    files_by_job: HashMap<u64, usize>,
    file_waiters: VecDeque<FileWaiter>,
    last_grant: HashMap<u64, u64>,
    grant_number: u64,
    step: WorkerStep,
}

#[derive(Default)]
struct WorkerStep {
    cap: Option<usize>,
    epoch: u64,
    successes: u8,
}

#[derive(Default)]
struct ApiGate {
    phase: ApiPhase,
    fallback: u64,
}

#[derive(Default)]
enum ApiPhase {
    #[default]
    Open,
    Cooldown(Instant),
    Probe(Option<u64>),
    Blocked,
}

#[derive(Default)]
struct Cooldown {
    until: Option<Instant>,
    fallback: u64,
}

struct FileWaiter {
    id: u64,
    job: u64,
    limit: usize,
    host: String,
}

pub(crate) fn retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
}

impl Recovery {
    pub(crate) fn set_media_paused(&self, paused: bool) {
        let mut state = self.state.lock().unwrap();
        if paused {
            state.media_pause.cancel();
        } else if state.media_pause.is_cancelled() {
            state.media_pause = CancellationToken::new();
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn admit_media(&self, job: u64, sequential: bool, limit: usize) {
        let mut state = self.state.lock().unwrap();
        debug_assert!(state.media_limits.is_empty() || state.sequential_media == sequential);
        state.sequential_media = sequential;
        state.media_limits.insert(job, limit);
    }

    pub(crate) fn finish_media(&self, job: u64) {
        let mut state = self.state.lock().unwrap();
        state.media_limits.remove(&job);
        state.last_grant.remove(&job);
        self.changed.notify_waiters();
    }

    pub(crate) fn update_media_limits(&self, limit: usize) {
        let mut state = self.state.lock().unwrap();
        state
            .media_limits
            .values_mut()
            .for_each(|current| *current = limit);
        self.changed.notify_waiters();
    }

    pub fn status(&self, account: Option<u64>) -> RecoveryDto {
        let state = self.state.lock().unwrap();
        let api = state
            .accounts
            .iter()
            .filter(|(key, _)| account.is_none_or(|account| **key == account))
            .map(|(_, gate)| gate);
        let api_until = api.clone().filter_map(|gate| match gate.phase {
            ApiPhase::Cooldown(until) => Some(until),
            _ => None,
        });
        let cooldown_seconds = state
            .hosts
            .values()
            .filter_map(|host| host.until)
            .chain(api_until)
            .filter_map(seconds_left)
            .max();
        RecoveryDto {
            cooldown_seconds,
            retry_connection: api
                .into_iter()
                .any(|gate| matches!(gate.phase, ApiPhase::Blocked)),
            worker_cap: state.step.cap,
        }
    }

    pub fn retry_connection(&self) {
        let mut state = self.state.lock().unwrap();
        for gate in state.accounts.values_mut() {
            if matches!(gate.phase, ApiPhase::Blocked) {
                gate.phase = ApiPhase::Probe(None);
            }
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn retry_account_connection(&self, account: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(gate) = state.accounts.get_mut(&account)
            && matches!(gate.phase, ApiPhase::Blocked)
        {
            gate.phase = ApiPhase::Probe(None);
            self.changed.notify_waiters();
        }
    }

    pub(crate) fn api_waiting_reason(&self, account: u64) -> Option<String> {
        let state = self.state.lock().unwrap();
        match state.accounts.get(&account)?.phase {
            ApiPhase::Cooldown(until) => seconds_left(until)
                .map(|seconds| format!("Qobuz is limiting API requests. Retrying in {seconds}s.")),
            ApiPhase::Probe(_) => Some("Checking the Qobuz connection…".into()),
            ApiPhase::Blocked => {
                Some("The Qobuz connection needs attention. Choose Retry connection.".into())
            }
            ApiPhase::Open => None,
        }
    }

    pub(crate) fn rate_limited_host(&self, host: &str, retry_after: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        let gate = state.hosts.entry(host.to_ascii_lowercase()).or_default();
        let until = cooldown_deadline(retry_after, &mut gate.fallback);
        gate.until = Some(gate.until.map_or(until, |current| current.max(until)));
        self.changed.notify_waiters();
    }

    pub(crate) fn rate_limited_from_headers(&self, host: &str, headers: &reqwest::header::HeaderMap) {
        self.rate_limited_host(host, retry_after_header(headers));
    }

    pub(crate) fn host_is_throttled(&self, host: &str) -> bool {
        self.state.lock().unwrap().hosts.get(host)
            .and_then(|gate| gate.until)
            .is_some_and(|until| until > Instant::now())
    }

    pub(crate) fn has_active_timer(&self) -> bool {
        let state = self.state.lock().unwrap();
        let now = Instant::now();
        state
            .accounts
            .values()
            .any(|gate| !matches!(gate.phase, ApiPhase::Open))
            || state
                .hosts
                .values()
                .any(|gate| gate.until.is_some_and(|until| until > now))
    }

    pub fn drive_blocked(&self, path: &Path) -> Option<String> {
        let key = volume(path)?;
        self.state.lock().unwrap().drives.contains(&key).then_some(key)
    }

    pub fn release_drive(&self, path: &Path) {
        if let Some(key) = volume(path) {
            self.state.lock().unwrap().drives.remove(&key);
            self.changed.notify_waiters();
        }
    }

    pub(crate) fn request(self: &Arc<Self>, account: u64) -> ApiRequest {
        ApiRequest {
            recovery: self.clone(),
            account,
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub(crate) fn transfer(
        self: &Arc<Self>,
        job: u64,
        generation: u64,
        limit: usize,
        observer: impl Fn(RecoveryNotice) + Send + Sync + 'static,
    ) -> TransferContext {
        let observer: Observer = Arc::new(observer);
        TransferContext {
            recovery: self.clone(),
            job,
            generation,
            limit,
            observer,
        }
    }
}

impl State {
    fn capacity(&self) -> bool {
        self.step
            .cap
            .is_none_or(|cap| self.api_count + self.file_count < cap)
    }

    fn next_file(&self) -> Option<u64> {
        let ceiling = if self.sequential_media {
            SEQUENTIAL_MEDIA_LIMIT
        } else {
            CONCURRENT_MEDIA_LIMIT
        };
        if self.media_pause.is_cancelled() || self.file_count >= ceiling || !self.capacity() {
            return None;
        }
        let now = Instant::now();
        self.file_waiters
            .iter()
            .filter(|waiter| {
                let limit = self
                    .media_limits
                    .get(&waiter.job)
                    .copied()
                    .unwrap_or(waiter.limit);
                self.files_by_job.get(&waiter.job).copied().unwrap_or(0) < limit
                    && self
                        .hosts
                        .get(&waiter.host)
                        .and_then(|host| host.until)
                        .is_none_or(|until| until <= now)
            })
            .min_by_key(|waiter| self.last_grant.get(&waiter.job).copied().unwrap_or(0))
            .map(|waiter| waiter.id)
    }
}

impl WorkerStep {
    fn finish(&mut self, epoch: u64, outcome: ApiOutcome<'_>) {
        if epoch != self.epoch {
            return;
        }
        match outcome {
            ApiOutcome::TimedOut => {
                self.successes = 0;
                let reduced = Some(if matches!(self.cap, Some(1 | 2)) {
                    1
                } else {
                    2
                });
                if reduced != self.cap {
                    self.cap = reduced;
                    self.epoch += 1;
                }
            }
            ApiOutcome::Success if self.cap.is_some() => {
                self.successes += 1;
                if self.successes == 3 {
                    self.cap = match self.cap {
                        Some(1) => Some(2),
                        Some(2) => Some(4),
                        _ => None,
                    };
                    self.successes = 0;
                    self.epoch += 1;
                }
            }
            ApiOutcome::Success => (),
            _ => self.successes = 0,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ApiOutcome<'a> {
    Success,
    TimedOut,
    Transient,
    Rejected,
    RateLimited(Option<&'a str>),
}

pub(crate) struct ApiRequest {
    recovery: Arc<Recovery>,
    account: u64,
    id: u64,
}

impl ApiRequest {
    pub async fn acquire(&self) -> ApiPermit<'_> {
        loop {
            let notice = self.recovery.changed.notified();
            tokio::pin!(notice);
            notice.as_mut().enable();
            let deadline = {
                let mut state = self.recovery.state.lock().unwrap();
                let now = Instant::now();
                let gate = state.accounts.entry(self.account).or_default();
                if matches!(gate.phase, ApiPhase::Cooldown(until) if until <= now) {
                    gate.phase = ApiPhase::Probe(None);
                }
                let (eligible, probe, cooldown) = match gate.phase {
                    ApiPhase::Open => (true, false, None),
                    ApiPhase::Probe(owner) if owner.is_none_or(|id| id == self.id) => {
                        (true, true, None)
                    }
                    ApiPhase::Cooldown(until) => (false, false, Some(until)),
                    _ => (false, false, None),
                };
                let next_start = state.next_api_start.filter(|next| *next > now);
                if eligible
                    && next_start.is_none()
                    && state.api_count < API_LIMIT
                    && state.capacity()
                {
                    if probe {
                        state.accounts.get_mut(&self.account).unwrap().phase =
                            ApiPhase::Probe(Some(self.id));
                    }
                    state.api_count += 1;
                    state.next_api_start = Some(now + API_INTERVAL);
                    return ApiPermit {
                        request: self,
                        probe,
                        epoch: state.step.epoch,
                        active: true,
                    };
                }
                cooldown.or(next_start.filter(|_| eligible))
            };
            let _ = await_notice(notice, None, deadline).await;
        }
    }

    pub fn block_probe(&self) {
        let mut state = self.recovery.state.lock().unwrap();
        let gate = state.accounts.entry(self.account).or_default();
        if matches!(gate.phase, ApiPhase::Probe(Some(id)) if id == self.id) {
            gate.phase = ApiPhase::Blocked;
            self.recovery.changed.notify_waiters();
        }
    }
}

impl Drop for ApiRequest {
    fn drop(&mut self) {
        let mut state = self.recovery.state.lock().unwrap();
        let gate = state.accounts.entry(self.account).or_default();
        if matches!(gate.phase, ApiPhase::Probe(Some(id)) if id == self.id) {
            gate.phase = ApiPhase::Probe(None);
            self.recovery.changed.notify_waiters();
        }
    }
}

pub(crate) struct ApiPermit<'a> {
    request: &'a ApiRequest,
    probe: bool,
    epoch: u64,
    active: bool,
}

impl ApiPermit<'_> {
    pub fn probing(&self) -> bool {
        self.probe
    }

    pub fn finish(mut self, outcome: ApiOutcome<'_>) {
        let mut state = self.request.recovery.state.lock().unwrap();
        state.step.finish(self.epoch, outcome);
        let gate = state.accounts.entry(self.request.account).or_default();
        let owns_probe = matches!(gate.phase, ApiPhase::Probe(Some(id)) if id == self.request.id);
        match outcome {
            ApiOutcome::Success => {
                gate.fallback = 0;
                if owns_probe {
                    gate.phase = ApiPhase::Open;
                }
            }
            ApiOutcome::RateLimited(header) => {
                let until = cooldown_deadline(header, &mut gate.fallback);
                let until = match gate.phase {
                    ApiPhase::Cooldown(current) => current.max(until),
                    _ => until,
                };
                gate.phase = ApiPhase::Cooldown(until);
            }
            ApiOutcome::Rejected if owns_probe => gate.phase = ApiPhase::Probe(None),
            _ => (),
        }
        state.api_count -= 1;
        self.active = false;
        self.request.recovery.changed.notify_waiters();
    }
}

impl Drop for ApiPermit<'_> {
    fn drop(&mut self) {
        if self.active {
            let mut state = self.request.recovery.state.lock().unwrap();
            state.api_count -= 1;
            self.request.recovery.changed.notify_waiters();
        }
    }
}

#[derive(Clone)]
pub struct TransferContext {
    recovery: Arc<Recovery>,
    job: u64,
    generation: u64,
    limit: usize,
    observer: Observer,
}

impl TransferContext {
    async fn wait_condition(
        &self,
        cancel: &CancellationToken,
        mut ready: impl FnMut() -> bool,
    ) -> bool {
        loop {
            let notice = self.recovery.changed.notified();
            tokio::pin!(notice);
            notice.as_mut().enable();
            if cancel.is_cancelled() {
                return false;
            }
            if ready() {
                return true;
            }
            if !await_notice(notice, Some(cancel), None).await {
                return false;
            }
        }
    }

    pub(crate) async fn wait_for_media(&self, cancel: &CancellationToken) -> bool {
        self.wait_condition(cancel, || {
            !self.recovery.state.lock().unwrap().media_pause.is_cancelled()
        })
        .await
    }

    pub(crate) fn is_sequential(&self) -> bool {
        let state = self.recovery.state.lock().unwrap();
        state.sequential_media && state.media_limits.contains_key(&self.job)
    }

    pub fn received(&self, bytes: u64) {
        (self.observer)(RecoveryNotice::NetworkBytes { bytes });
    }

    pub fn attempt_key(&self) -> (u64, u64) {
        (self.job, self.generation)
    }

    pub fn budget(&self) -> RetryBudget {
        RetryBudget {
            observer: Some(self.observer.clone()),
            ..RetryBudget::default()
        }
    }

    pub async fn acquire(&self, host: &str, cancel: &CancellationToken) -> Option<TransferPermit> {
        let host = host.to_ascii_lowercase();
        let id = self.recovery.next_id.fetch_add(1, Ordering::Relaxed);
        let mut waiter = TransferWaiter {
            recovery: &self.recovery,
            id,
            active: true,
        };
        self.recovery
            .state
            .lock()
            .unwrap()
            .file_waiters
            .push_back(FileWaiter {
                id,
                job: self.job,
                limit: self.limit,
                host: host.clone(),
            });
        let mut last_report = None;
        loop {
            let notice = self.recovery.changed.notified();
            tokio::pin!(notice);
            notice.as_mut().enable();
            if cancel.is_cancelled() {
                return None;
            }
            let (permit, cooldown) = {
                let mut state = self.recovery.state.lock().unwrap();
                let cooldown = state
                    .hosts
                    .get(&host)
                    .and_then(|gate| gate.until)
                    .filter(|until| *until > Instant::now());
                if state.next_file() == Some(id) {
                    state.file_waiters.retain(|waiter| waiter.id != id);
                    state.file_count += 1;
                    *state.files_by_job.entry(self.job).or_default() += 1;
                    state.grant_number += 1;
                    let grant = state.grant_number;
                    state.last_grant.insert(self.job, grant);
                    (
                        Some(TransferPermit {
                            recovery: self.recovery.clone(),
                            job: self.job,
                            pause: state.media_pause.clone(),
                        }),
                        None,
                    )
                } else {
                    (None, cooldown)
                }
            };
            if let Some(permit) = permit {
                waiter.active = false;
                drop(waiter);
                (self.observer)(RecoveryNotice::Transferring);
                return Some(permit);
            }
            let deadline = cooldown.map(|until| {
                if let Some(seconds) =
                    seconds_left(until).filter(|seconds| Some(*seconds) != last_report)
                {
                    last_report = Some(seconds);
                    (self.observer)(RecoveryNotice::RateLimited { seconds });
                }
                until.min(Instant::now() + Duration::from_secs(1))
            });
            if !await_notice(notice, Some(cancel), deadline).await {
                return None;
            }
        }
    }

    pub fn disk_full(&self, path: &Path) {
        if let Some(key) = volume(path) {
            self.recovery.state.lock().unwrap().drives.insert(key);
        }
    }

    pub fn wait_for_drive(&self, path: &Path, cancel: &CancellationToken) -> bool {
        !cancel.is_cancelled() && self.recovery.drive_blocked(path).is_none()
    }

    pub(crate) async fn wait_for_drive_release(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> bool {
        self.wait_condition(cancel, || self.recovery.drive_blocked(path).is_none())
            .await
    }
}

async fn await_notice(
    notice: Pin<&mut Notified<'_>>,
    cancel: Option<&CancellationToken>,
    deadline: Option<Instant>,
) -> bool {
    match (cancel, deadline) {
        (Some(cancel), Some(deadline)) => {
            tokio::select! { _ = cancel.cancelled() => return false, _ = notice => (), _ = sleep_until(deadline) => () }
        }
        (Some(cancel), None) => {
            tokio::select! { _ = cancel.cancelled() => return false, _ = notice => () }
        }
        (None, Some(deadline)) => {
            tokio::select! { _ = notice => (), _ = sleep_until(deadline) => () }
        }
        (None, None) => notice.await,
    }
    true
}

struct TransferWaiter<'a> {
    recovery: &'a Recovery,
    id: u64,
    active: bool,
}

impl Drop for TransferWaiter<'_> {
    fn drop(&mut self) {
        if self.active {
            self.recovery
                .state
                .lock()
                .unwrap()
                .file_waiters
                .retain(|waiter| waiter.id != self.id);
            self.recovery.changed.notify_waiters();
        }
    }
}

pub struct TransferPermit {
    recovery: Arc<Recovery>,
    job: u64,
    pub(crate) pause: CancellationToken,
}

impl TransferPermit {
    #[cfg(test)]
    pub fn rate_limited_host(&self, host: &str, retry_after: Option<&str>) {
        self.recovery.rate_limited_host(host, retry_after);
    }

    pub fn rate_limited_from_headers(&self, host: &str, headers: &reqwest::header::HeaderMap) {
        self.recovery.rate_limited_from_headers(host, headers);
    }

    pub fn success_host(&self, host: &str) {
        if let Some(gate) = self
            .recovery
            .state
            .lock()
            .unwrap()
            .hosts
            .get_mut(&host.to_ascii_lowercase())
        {
            gate.fallback = 0;
        }
    }
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        let mut state = self.recovery.state.lock().unwrap();
        state.file_count -= 1;
        if let Some(count) = state.files_by_job.get_mut(&self.job) {
            *count -= 1;
            if *count == 0 {
                state.files_by_job.remove(&self.job);
            }
        }
        self.recovery.changed.notify_waiters();
    }
}

fn seconds_left(until: Instant) -> Option<u64> {
    let left = until.checked_duration_since(Instant::now())?;
    Some(left.as_secs() + u64::from(left.subsec_nanos() != 0))
}

fn cooldown_deadline(header: Option<&str>, fallback: &mut u64) -> Instant {
    let now = Instant::now();
    if let Some(delay) = header
        .map(str::trim)
        .and_then(|header| {
            header
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    let date = httpdate::parse_http_date(header).ok()?;
                    Some(date.duration_since(SystemTime::now()).unwrap_or_default())
                })
        })
        .and_then(|delay| now.checked_add(delay))
    {
        return delay;
    }
    *fallback = if *fallback == 0 {
        30
    } else {
        (*fallback * 2).min(300)
    };
    now + Duration::from_secs(*fallback)
}

fn volume(path: &Path) -> Option<String> {
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => {
                Some(format!("{}:", char::from(letter).to_ascii_uppercase()))
            }
            std::path::Prefix::UNC(server, share)
            | std::path::Prefix::VerbatimUNC(server, share) => Some(format!(
                "\\\\{}\\{}",
                server.to_string_lossy().to_lowercase(),
                share.to_string_lossy().to_lowercase()
            )),
            _ => Some(prefix.as_os_str().to_string_lossy().to_lowercase()),
        },
        Component::RootDir => Some("/".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
