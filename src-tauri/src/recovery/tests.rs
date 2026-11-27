use super::*;

#[test]
fn recovery_uses_only_requests_from_the_current_step() {
    let mut step = WorkerStep::default();
    step.finish(0, ApiOutcome::TimedOut);
    assert_eq!(step.cap, Some(2));
    step.finish(0, ApiOutcome::TimedOut);
    assert_eq!(step.cap, Some(2));
    step.finish(1, ApiOutcome::TimedOut);
    assert_eq!(step.cap, Some(1));
    for expected in [Some(2), Some(4), None] {
        let epoch = step.epoch;
        for _ in 0..3 {
            step.finish(epoch, ApiOutcome::Success);
        }
        assert_eq!(step.cap, expected);
    }
}

#[test]
fn a_valid_retry_after_is_not_capped_at_five_minutes() {
    let mut fallback = 0;
    assert!(seconds_left(cooldown_deadline(Some("1800"), &mut fallback)).unwrap() >= 1799);
    assert!(seconds_left(cooldown_deadline(Some("garbage"), &mut fallback)).unwrap() >= 29);
    assert!(seconds_left(cooldown_deadline(None, &mut fallback)).unwrap() >= 59);
}

#[test]
fn round_robin_skips_jobs_at_their_captured_cap() {
    let mut state = State::default();
    for (id, job) in [(1, 10), (2, 10), (3, 20), (4, 30)] {
        state.file_waiters.push_back(FileWaiter {
            id,
            job,
            limit: 1,
            host: "example.test".into(),
        });
    }
    assert_eq!(state.next_file(), Some(1));
    state.files_by_job.insert(10, 1);
    state.file_count = 1;
    assert_eq!(state.next_file(), Some(3));
    state.last_grant.insert(20, 2);
    assert_eq!(state.next_file(), Some(4));
    state.api_count = 1;
    state.step.cap = Some(2);
    assert_eq!(state.next_file(), None);
}

#[tokio::test]
async fn cooldown_admits_one_api_check_and_manual_retry_reopens_a_blocked_connection() {
    let recovery = Arc::new(Recovery::default());
    let first = recovery.request(10);
    first
        .acquire()
        .await
        .finish(ApiOutcome::RateLimited(Some("0")));
    let check = first.acquire().await;
    assert!(check.probing());
    let waiting = recovery.request(10);
    assert!(
        tokio::time::timeout(Duration::from_millis(15), waiting.acquire())
            .await
            .is_err()
    );
    check.finish(ApiOutcome::TimedOut);
    first.block_probe();
    assert!(recovery.status(Some(10)).retry_connection);
    assert!(
        tokio::time::timeout(Duration::from_millis(15), waiting.acquire())
            .await
            .is_err()
    );
    recovery.retry_connection();
    let check = waiting.acquire().await;
    assert!(check.probing());
    check.finish(ApiOutcome::Success);
    assert!(!recovery.status(Some(10)).retry_connection);
    assert!(!first.acquire().await.probing());
}

#[tokio::test]
async fn cancelled_transfer_waiters_release_their_place_without_using_a_slot() {
    let recovery = Arc::new(Recovery::default());
    let context = recovery.transfer(1, 0, 1, |_| {});
    let cancel = CancellationToken::new();
    let active = context.acquire("example.test", &cancel).await.unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(context.acquire("example.test", &cancelled).await.is_none());
    assert!(recovery.state.lock().unwrap().file_waiters.is_empty());
    drop(active);
    assert_eq!(recovery.state.lock().unwrap().file_count, 0);
    assert!(context.acquire("example.test", &cancel).await.is_some());
}

#[tokio::test]
async fn eight_concurrent_jobs_can_fill_the_global_transfer_limit() {
    let recovery = Arc::new(Recovery::default());
    let cancel = CancellationToken::new();
    let mut permits = Vec::new();
    for job in 1..=8 {
        let context = recovery.transfer(job, 0, 1, |_| {});
        permits.push(context.acquire("example.test", &cancel).await.unwrap());
    }
    let ninth = recovery.transfer(9, 0, 1, |_| {});
    assert!(
        tokio::time::timeout(
            Duration::from_millis(15),
            ninth.acquire("example.test", &cancel)
        )
        .await
        .is_err()
    );
    permits.pop();
    assert!(ninth.acquire("example.test", &cancel).await.is_some());
}

#[tokio::test]
async fn existing_waiters_obey_live_raises_and_reductions_and_the_sequential_ceiling() {
    use futures_util::poll;
    use std::pin::pin;
    let recovery = Arc::new(Recovery::default());
    recovery.admit_media(1, true, 1);
    let transfer = recovery.transfer(1, 0, 1, |_| {});
    let cancel = CancellationToken::new();
    let first = transfer.acquire("example.test", &cancel).await.unwrap();
    let mut second = pin!(transfer.acquire("example.test", &cancel));
    assert!(poll!(&mut second).is_pending());
    recovery.update_media_limits(2);
    let second = second.await.unwrap();
    let mut third = pin!(transfer.acquire("example.test", &cancel));
    assert!(poll!(&mut third).is_pending());
    recovery.update_media_limits(1);
    drop(second);
    assert!(poll!(&mut third).is_pending());
    drop(first);
    let third = third.await.unwrap();
    recovery.update_media_limits(usize::MAX);
    let mut held = vec![third];
    while held.len() < SEQUENTIAL_MEDIA_LIMIT {
        held.push(transfer.acquire("example.test", &cancel).await.unwrap());
    }
    let mut excess = pin!(transfer.acquire("example.test", &cancel));
    assert!(poll!(&mut excess).is_pending());
    drop(held.pop());
    assert!(excess.await.is_some());
    drop(held);
    assert_eq!(recovery.state.lock().unwrap().file_count, 0);
}

#[tokio::test]
async fn pause_invalidates_grants_and_resume_releases_existing_waiters() {
    use futures_util::poll;
    use std::pin::pin;
    let recovery = Arc::new(Recovery::default());
    recovery.admit_media(1, true, 1);
    let context = recovery.transfer(1, 0, 1, |_| {});
    let cancel = CancellationToken::new();
    let active = context.acquire("example.test", &cancel).await.unwrap();
    let signal = active.pause.clone();
    recovery.set_media_paused(true);
    assert!(signal.is_cancelled());
    drop(active);
    assert_eq!(recovery.state.lock().unwrap().file_count, 0);
    let mut waiting = pin!(context.acquire("example.test", &cancel));
    assert!(poll!(&mut waiting).is_pending());
    assert!(!recovery.request(2).acquire().await.probing());
    recovery.set_media_paused(false);
    let resumed = waiting.await.unwrap();
    assert!(!resumed.pause.is_cancelled());
    assert!(signal.is_cancelled());
}
#[tokio::test]
async fn file_host_cooldowns_leave_other_hosts_and_accounts_eligible() {
    let recovery = Arc::new(Recovery::default());
    let context = recovery.transfer(1, 0, 8, |_| {});
    let cancel = CancellationToken::new();
    let file = context.acquire("limited.test", &cancel).await.unwrap();
    file.rate_limited_host("limited.test", Some("60"));
    drop(file);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(15),
            context.acquire("limited.test", &cancel)
        )
        .await
        .is_err()
    );
    assert!(context.acquire("other.test", &cancel).await.is_some());
    let account = recovery.request(1);
    account
        .acquire()
        .await
        .finish(ApiOutcome::RateLimited(Some("60")));
    let other = recovery.request(2);
    assert!(!other.acquire().await.probing());
    assert!(
        !recovery
            .state
            .lock()
            .unwrap()
            .file_waiters
            .iter()
            .any(|waiter| waiter.host == "limited.test")
    );
}

#[test]
fn http_date_waits_and_windows_drive_holds_keep_their_scope() {
    let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(1200));
    let mut fallback = 0;
    assert!(seconds_left(cooldown_deadline(Some(&date), &mut fallback)).unwrap() >= 1198);
    assert_eq!(fallback, 0);
    #[cfg(windows)]
    {
        let recovery = Arc::new(Recovery::default());
        let context = recovery.transfer(1, 0, 4, |_| {});
        context.disk_full(Path::new("C:\\Music\\file.flac"));
        assert_eq!(
            recovery.drive_blocked(Path::new("C:\\Other")),
            Some("C:".into())
        );
        assert!(recovery.drive_blocked(Path::new("D:\\Music")).is_none());
        recovery.release_drive(Path::new("C:\\Music"));
        assert!(recovery.drive_blocked(Path::new("C:\\Other")).is_none());
    }
}
