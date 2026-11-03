use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

use jiff::civil::Date;
use jiff::Timestamp;

use crate::http::{Http, HttpError, NDL_SPACING};
use crate::ndl::{day_url, parse_page, Meeting, NdlError};
use crate::progress::{LoggedHttp, Progress};
use crate::store::{DayReport, Store};

#[derive(Debug)]
pub enum DayOutcome {
    Applied(DayReport),
    Inconsistent(String),
}

#[derive(Debug)]
pub enum CrawlError {
    Busy,
    Ndl(String),
    Http(HttpError),
    Store(anyhow::Error),
}

impl std::fmt::Display for CrawlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrawlError::Busy => f.write_str("NDL busy"),
            CrawlError::Ndl(m) => f.write_str(m),
            CrawlError::Http(e) => e.fmt(f),
            CrawlError::Store(e) => write!(f, "store: {e:#}"),
        }
    }
}

impl std::error::Error for CrawlError {}

pub fn crawl_day(
    http: &dyn Http,
    store: &Store,
    date: Date,
    now: &dyn Fn() -> Timestamp,
) -> Result<DayOutcome, CrawlError> {
    let mut meetings: Vec<(Meeting, i64)> = Vec::new();
    let mut total: Option<u32> = None;
    let mut start = 1;
    loop {
        let url = day_url(date, start);
        let body = http.get(&url).map_err(CrawlError::Http)?;
        let fetch_id = store
            .record_fetch(&url, now(), &body)
            .map_err(CrawlError::Store)?;
        let page = parse_page(&body).map_err(|e| match e {
            NdlError::Busy(_) => CrawlError::Busy,
            e => CrawlError::Ndl(e.to_string()),
        })?;
        if let Some(first) = total {
            if first != page.total {
                return Ok(DayOutcome::Inconsistent(format!(
                    "numberOfRecords changed from {first} to {} while paging {date}",
                    page.total
                )));
            }
        }
        total = Some(page.total);
        meetings.extend(page.meetings.into_iter().map(|m| (m, fetch_id)));
        match page.next {
            None => break,
            Some(next) if next <= start => {
                return Err(CrawlError::Ndl(format!(
                    "nextRecordPosition {next} does not advance past {start} for {date}"
                )));
            }
            Some(next) => start = next,
        }
    }
    let total = total.unwrap_or(0);
    if let Some((m, _)) = meetings.iter().find(|(m, _)| m.date != date) {
        return Ok(DayOutcome::Inconsistent(format!(
            "meeting {} is dated {} in the response for {date}",
            m.issue_id, m.date
        )));
    }
    let distinct = meetings
        .iter()
        .map(|(m, _)| m.issue_id.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    if distinct != total as usize {
        return Ok(DayOutcome::Inconsistent(format!(
            "{distinct} distinct meetings of {total} reported for {date}"
        )));
    }
    store
        .apply_day(date, &meetings, now())
        .map(DayOutcome::Applied)
        .map_err(CrawlError::Store)
}

const RECENT_DAYS: i32 = 180;
const DAILY_DAYS: i32 = 30;
const DAY_SECS: i64 = 86_400;
const OLD_RECHECK_SECS: i64 = 30 * DAY_SECS;
const IDLE: Duration = Duration::from_secs(60);
const BUSY: Duration = Duration::from_secs(300);
const FAILED: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Head(Date),
    Recent(Date),
    Backfill(Date),
    Recheck(Date),
}

impl Task {
    pub fn date(&self) -> Date {
        match *self {
            Task::Head(d) | Task::Recent(d) | Task::Backfill(d) | Task::Recheck(d) => d,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Task::Head(_) => "head",
            Task::Recent(_) => "recent",
            Task::Backfill(_) => "backfill",
            Task::Recheck(_) => "recheck",
        }
    }
}

// `days` must be sorted by date ascending, as `Store::days` returns them.
pub fn next_task(
    days: &[(Date, Timestamp)],
    today: Date,
    now: Timestamp,
    last_slot_was_backfill: bool,
) -> Option<Task> {
    next_task_skipping(days, today, now, last_slot_was_backfill, None)
}

fn next_task_skipping(
    days: &[(Date, Timestamp)],
    today: Date,
    now: Timestamp,
    last_slot_was_backfill: bool,
    skip: Option<Date>,
) -> Option<Task> {
    let Some(&(newest, _)) = days.last() else {
        return Some(Task::Head(today)).filter(|t| Some(t.date()) != skip);
    };
    // Head: the newest missing day above the oldest crawled day.
    let known: BTreeSet<Date> = days.iter().map(|&(d, _)| d).collect();
    let oldest = days[0].0;
    let mut d = today;
    while d > oldest {
        if !known.contains(&d) && Some(d) != skip {
            return Some(Task::Head(d));
        }
        d = d.yesterday().ok()?;
    }

    let elapsed = |v: Timestamp| now.duration_since(v).as_secs();
    let age = |d: Date| (today - d).get_days();
    let eligible = |d: Date| Some(d) != skip;

    let recent = days
        .iter()
        .filter(|&&(d, _)| age(d) <= RECENT_DAYS && eligible(d))
        .filter_map(|&(d, v)| {
            let every = if age(d) <= DAILY_DAYS { 1 } else { 7 } * DAY_SECS;
            let overdue = elapsed(v) - every;
            (overdue > 0).then_some((overdue, d))
        })
        .max_by_key(|&(overdue, _)| overdue);
    if let Some((_, d)) = recent {
        return Some(Task::Recent(d));
    }

    let mut run_start = newest;
    for &(d, _) in days.iter().rev().skip(1) {
        if d.tomorrow().ok() != Some(run_start) {
            break;
        }
        run_start = d;
    }
    let backfill = run_start
        .yesterday()
        .ok()
        .filter(|&d| d >= crate::ndl::FLOOR && eligible(d))
        .map(Task::Backfill);
    // "Nothing due" exists only because old days are re-checked just after OLD_RECHECK_SECS.
    let recheck = days
        .iter()
        .filter(|&&(d, v)| age(d) > RECENT_DAYS && elapsed(v) > OLD_RECHECK_SECS && eligible(d))
        .min_by_key(|&&(_, v)| v)
        .map(|&(d, _)| Task::Recheck(d));
    if last_slot_was_backfill {
        recheck.or(backfill)
    } else {
        backfill.or(recheck)
    }
}

#[derive(Debug, Default)]
pub struct RunState {
    last_slot_was_backfill: bool,
    deferred: Option<Task>,
    ran_other_since_deferred: bool,
}

pub fn jst_date(now: Timestamp) -> Date {
    // NDL dates are Japanese; Japan has no daylight saving time.
    now.to_zoned(jiff::tz::TimeZone::fixed(jiff::tz::offset(9)))
        .date()
}

pub fn step(
    http: &dyn Http,
    store: &Store,
    state: &mut RunState,
    today: Date,
    clock: &dyn Fn() -> Timestamp,
    sleep: &dyn Fn(Duration),
    progress: &Progress,
) -> anyhow::Result<()> {
    let http = LoggedHttp::new(http, progress);
    let retry = state.deferred.is_some() && state.ran_other_since_deferred;
    let task = if retry {
        state.ran_other_since_deferred = false;
        state.deferred.take()
    } else {
        let days = store.days()?;
        let skip = state.deferred.map(|t| t.date());
        let next = next_task_skipping(&days, today, clock(), state.last_slot_was_backfill, skip);
        if next.is_some() {
            state.ran_other_since_deferred = true;
            next
        } else if state.deferred.is_some() {
            progress.sleeping(IDLE, "only a deferred day is left");
            sleep(IDLE);
            state.ran_other_since_deferred = false;
            state.deferred.take()
        } else {
            progress.sleeping(IDLE, "nothing due");
            sleep(IDLE);
            return Ok(());
        }
    };
    let Some(task) = task else {
        return Ok(());
    };
    match task {
        Task::Backfill(_) => state.last_slot_was_backfill = true,
        Task::Recheck(_) => state.last_slot_was_backfill = false,
        _ => {}
    }
    let wait = match crawl_day(&http, store, task.date(), clock) {
        Ok(DayOutcome::Applied(report)) => {
            progress.applied(&task, &report);
            return Ok(());
        }
        Ok(DayOutcome::Inconsistent(why)) => {
            progress.failed(&task, &format!("inconsistent, will retry: {why}"));
            state.deferred = Some(task);
            state.ran_other_since_deferred = false;
            return Ok(());
        }
        Err(CrawlError::Store(e)) => return Err(e),
        Err(CrawlError::Busy) => {
            progress.failed(&task, "NDL busy");
            BUSY
        }
        Err(e) => {
            progress.failed(&task, &e.to_string());
            state.deferred = Some(task);
            state.ran_other_since_deferred = false;
            FAILED
        }
    };
    progress.sleeping(wait, "backing off");
    sleep(wait);
    Ok(())
}

pub fn run(http: &dyn Http, store: &Store, progress: &Progress) -> anyhow::Result<()> {
    let sleep = |d: Duration| std::thread::sleep(d);
    sleep(Duration::from_secs_f64(NDL_SPACING));
    let mut state = RunState::default();
    loop {
        let today = jst_date(Timestamp::now());
        step(
            http,
            store,
            &mut state,
            today,
            &Timestamp::now,
            &sleep,
            progress,
        )?;
    }
}

pub fn lock_database(path: &Path) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another crawler holds {}; only one may run per database",
            path.display()
        )
    })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct Fake {
        pages: HashMap<String, Vec<u8>>,
        log: Mutex<Vec<String>>,
    }

    impl Http for Fake {
        fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
            self.log.lock().unwrap().push(url.to_string());
            self.pages
                .get(url)
                .cloned()
                .ok_or_else(|| HttpError::NotFound(url.to_string()))
        }
    }

    fn fake(pages: Vec<(u32, Vec<u8>)>) -> Fake {
        Fake {
            pages: pages
                .into_iter()
                .map(|(start, body)| (day_url(day(), start), body))
                .collect(),
            log: Mutex::new(vec![]),
        }
    }

    fn day() -> Date {
        date(2026, 10, 20)
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("tests/fixtures/{name}")).unwrap()
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    fn now() -> Timestamp {
        "2026-10-21T00:00:00Z".parse().unwrap()
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn crawls_both_pages_of_a_day() {
        let (_d, store) = store();
        let http = fake(vec![
            (1, fixture("meeting_day_p1.json")),
            (3, fixture("meeting_day_p2.json")),
        ]);
        let out = crawl_day(&http, &store, day(), &now).unwrap();
        match out {
            DayOutcome::Applied(r) => assert_eq!(r.new, 3),
            other => panic!("{other:?}"),
        }
        assert_eq!(count(&store, "SELECT COUNT(*) FROM fetches"), 2);
    }

    #[test]
    fn empty_day_is_applied_with_no_meetings() {
        let (_d, store) = store();
        let http = fake(vec![(1, br#"{"numberOfRecords":0}"#.to_vec())]);
        let out = crawl_day(&http, &store, day(), &now).unwrap();
        match out {
            DayOutcome::Applied(r) => assert_eq!(r.new, 0),
            other => panic!("{other:?}"),
        }
        assert_eq!(store.days().unwrap().len(), 1);
    }

    #[test]
    fn day_with_shifting_pages_is_not_applied() {
        let p2 = String::from_utf8(fixture("meeting_day_p2.json"))
            .unwrap()
            .replace("\"numberOfRecords\": 3", "\"numberOfRecords\": 4");
        let (_d, store) = store();
        let http = fake(vec![
            (1, fixture("meeting_day_p1.json")),
            (3, p2.into_bytes()),
        ]);
        let out = crawl_day(&http, &store, day(), &now).unwrap();
        assert!(matches!(out, DayOutcome::Inconsistent(_)), "{out:?}");
        assert_eq!(store.days().unwrap().len(), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM fetches"), 2);

        let p2 = String::from_utf8(fixture("meeting_day_p2.json"))
            .unwrap()
            .replace("999900003X00320261020", "999900002X00220261020");
        let http = fake(vec![
            (1, fixture("meeting_day_p1.json")),
            (3, p2.into_bytes()),
        ]);
        let out = crawl_day(&http, &store, day(), &now).unwrap();
        assert!(matches!(out, DayOutcome::Inconsistent(_)), "{out:?}");
        assert_eq!(store.days().unwrap().len(), 0);
    }

    #[test]
    fn meeting_dated_otherwise_is_not_applied() {
        let p1 = String::from_utf8(fixture("meeting_day_p1.json"))
            .unwrap()
            .replacen("\"date\": \"2026-10-20\"", "\"date\": \"2026-10-19\"", 1);
        let (_d, store) = store();
        let http = fake(vec![
            (1, p1.into_bytes()),
            (3, fixture("meeting_day_p2.json")),
        ]);
        let out = crawl_day(&http, &store, day(), &now).unwrap();
        assert!(matches!(out, DayOutcome::Inconsistent(_)), "{out:?}");
        assert_eq!(store.days().unwrap().len(), 0);
    }

    #[test]
    fn busy_body_is_busy_and_kept_as_evidence() {
        let (_d, store) = store();
        let http = fake(vec![(1, br#"{"message":"(19001) busy"}"#.to_vec())]);
        let err = crawl_day(&http, &store, day(), &now).unwrap_err();
        assert!(matches!(err, CrawlError::Busy), "{err:?}");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM fetches"), 1);
        assert_eq!(store.days().unwrap().len(), 0);
    }

    #[test]
    fn non_advancing_position_is_an_ndl_error() {
        let (_d, store) = store();
        let body = br#"{"numberOfRecords":5,"nextRecordPosition":1}"#.to_vec();
        let http = fake(vec![(1, body)]);
        let err = crawl_day(&http, &store, day(), &now).unwrap_err();
        assert!(matches!(err, CrawlError::Ndl(_)), "{err:?}");
    }

    #[test]
    fn http_failure_is_reported() {
        let (_d, store) = store();
        let http = fake(vec![]);
        let err = crawl_day(&http, &store, day(), &now).unwrap_err();
        assert!(matches!(err, CrawlError::Http(_)), "{err:?}");
        assert!(err.to_string().contains("404"));
    }

    #[test]
    fn requests_are_one_at_a_time_in_page_order() {
        let (_d, store) = store();
        let http = fake(vec![
            (1, fixture("meeting_day_p1.json")),
            (3, fixture("meeting_day_p2.json")),
        ]);
        crawl_day(&http, &store, day(), &now).unwrap();
        assert_eq!(
            *http.log.lock().unwrap(),
            [day_url(day(), 1), day_url(day(), 3)]
        );
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    use crate::ndl::FLOOR;
    use jiff::civil::date;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn now() -> Timestamp {
        ts("2026-11-03T03:00:00Z")
    }

    fn today() -> Date {
        date(2026, 11, 3)
    }

    fn days_back(from_age: i32, to_age: i32, verified: Timestamp) -> Vec<(Date, Timestamp)> {
        (to_age..=from_age)
            .rev()
            .map(|a| {
                (
                    today().checked_sub(jiff::Span::new().days(a)).unwrap(),
                    verified,
                )
            })
            .collect()
    }

    #[test]
    fn head_days_come_first_newest_first() {
        assert_eq!(
            next_task(&[], today(), now(), false),
            Some(Task::Head(today()))
        );
        let mut days = vec![(date(2026, 10, 28), now()), (date(2026, 10, 29), now())];
        for expected in [date(2026, 11, 3), date(2026, 11, 2), date(2026, 11, 1)]
            .into_iter()
            .chain([date(2026, 10, 31), date(2026, 10, 30)])
        {
            assert_eq!(
                next_task(&days, today(), now(), false),
                Some(Task::Head(expected))
            );
            days.push((expected, now()));
            days.sort();
        }
        assert!(!matches!(
            next_task(&days, today(), now(), false),
            Some(Task::Head(_))
        ));
    }

    #[test]
    fn recent_days_are_rechecked_daily_then_weekly() {
        let two_days = now() - jiff::SignedDuration::from_hours(48);
        let three_days = now() - jiff::SignedDuration::from_hours(72);
        let fresh = now();
        let mut days = days_back(60, 0, fresh);
        days[0].1 = three_days; // age 60: weekly, not due
        let age10 = days
            .iter()
            .position(|&(d, _)| (today() - d).get_days() == 10)
            .unwrap();
        days[age10].1 = two_days; // age 10: daily, due
        assert_eq!(
            next_task(&days, today(), now(), false),
            Some(Task::Recent(days[age10].0))
        );
        days[age10].1 = fresh;
        let backfill = days[0].0.yesterday().unwrap();
        assert_eq!(
            next_task(&days, today(), now(), false),
            Some(Task::Backfill(backfill))
        );
    }

    #[test]
    fn most_overdue_recent_day_goes_first() {
        let mut days = days_back(5, 0, now());
        days[0].1 = now() - jiff::SignedDuration::from_hours(30);
        days[1].1 = now() - jiff::SignedDuration::from_hours(60);
        assert_eq!(
            next_task(&days, today(), now(), false),
            Some(Task::Recent(days[1].0))
        );
    }

    #[test]
    fn backfill_and_old_recheck_alternate() {
        let old = now() - jiff::SignedDuration::from_hours(24 * 40);
        let mut days = days_back(400, 0, now());
        days[10].1 = old;
        let oldest = days[0].0;
        assert_eq!(
            next_task(&days, today(), now(), false),
            Some(Task::Backfill(oldest.yesterday().unwrap()))
        );
        assert_eq!(
            next_task(&days, today(), now(), true),
            Some(Task::Recheck(days[10].0))
        );
        days[10].1 = now();
        assert_eq!(
            next_task(&days, today(), now(), true),
            Some(Task::Backfill(oldest.yesterday().unwrap()))
        );
    }

    #[test]
    fn backfill_stops_at_the_floor() {
        let old = now() - jiff::SignedDuration::from_hours(24 * 40);
        let mut days = vec![(FLOOR, old)];
        let mut d = FLOOR;
        while d < today() {
            d = d.tomorrow().unwrap();
            days.push((d, now()));
        }
        assert_eq!(
            next_task(&days, today(), now(), false),
            Some(Task::Recheck(FLOOR))
        );
        assert_eq!(
            next_task(&days, today(), now(), true),
            Some(Task::Recheck(FLOOR))
        );
    }

    #[test]
    fn nothing_due_is_none() {
        let mut days = vec![(FLOOR, now())];
        let mut d = FLOOR;
        while d < today() {
            d = d.tomorrow().unwrap();
            days.push((d, now()));
        }
        assert_eq!(next_task(&days, today(), now(), false), None);
        assert_eq!(next_task(&days, today(), now(), true), None);
    }

    #[test]
    fn backfill_starts_below_the_newest_contiguous_run() {
        let days = days_back(3, 0, now());
        assert_eq!(
            next_task(&days, today(), now(), false),
            Some(Task::Backfill(
                today().checked_sub(jiff::Span::new().days(4)).unwrap()
            ))
        );
    }

    struct Fake {
        pages: HashMap<String, Vec<u8>>,
        log: Mutex<Vec<String>>,
    }

    impl Http for Fake {
        fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
            self.log.lock().unwrap().push(url.to_string());
            self.pages
                .get(url)
                .cloned()
                .ok_or_else(|| HttpError::GaveUp(format!("no page for {url}")))
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        store: Store,
        state: RunState,
        today: Date,
        slept: RefCell<Vec<Duration>>,
        text: Arc<Mutex<Vec<u8>>>,
        progress: Progress,
    }

    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Harness {
        fn new() -> Harness {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(&dir.path().join("t.db")).unwrap();
            let text = Arc::new(Mutex::new(Vec::new()));
            Harness {
                _dir: dir,
                store,
                state: RunState::default(),
                today: today(),
                slept: RefCell::new(vec![]),
                progress: Progress::new(Box::new(Sink(text.clone()))),
                text,
            }
        }

        fn step(&mut self, http: &Fake) {
            let slept = &self.slept;
            step(
                http,
                &self.store,
                &mut self.state,
                self.today,
                &now,
                &|d| slept.borrow_mut().push(d),
                &self.progress,
            )
            .unwrap();
        }

        fn fill(&self, from: Date, to: Date, skip: &[Date], stale_day: Date, stale: Timestamp) {
            let mut d = from;
            while d <= to {
                if !skip.contains(&d) {
                    let at = if d == stale_day { stale } else { now() };
                    self.store.apply_day(d, &[], at).unwrap();
                }
                d = d.tomorrow().unwrap();
            }
        }

        fn output(&self) -> String {
            String::from_utf8(self.text.lock().unwrap().clone()).unwrap()
        }
    }

    fn fake(pages: Vec<(Date, &str)>) -> Fake {
        Fake {
            pages: pages
                .into_iter()
                .map(|(d, body)| (day_url(d, 1), body.as_bytes().to_vec()))
                .collect(),
            log: Mutex::new(vec![]),
        }
    }

    #[test]
    fn busy_body_backs_off() {
        let mut h = Harness::new();
        let http = fake(vec![(today(), r#"{"message":"(19001) busy"}"#)]);
        h.step(&http);
        assert_eq!(*h.slept.borrow(), [Duration::from_secs(300)]);
        assert_eq!(h.store.days().unwrap().len(), 0);
        assert!(h.output().contains("busy"), "{}", h.output());
    }

    #[test]
    fn failed_request_backs_off_ten_minutes() {
        let mut h = Harness::new();
        h.step(&fake(vec![]));
        assert_eq!(*h.slept.borrow(), [Duration::from_secs(600)]);
    }

    #[test]
    fn applied_day_is_logged_without_sleeping() {
        let mut h = Harness::new();
        let http = fake(vec![(today(), r#"{"numberOfRecords":0}"#)]);
        h.step(&http);
        assert!(h.slept.borrow().is_empty());
        assert_eq!(h.store.days().unwrap().len(), 1);
        assert!(h.output().contains("head 2026-11-03"), "{}", h.output());
    }

    #[test]
    fn inconsistent_day_is_retried_after_the_next_task() {
        let fixture = |n: &str| std::fs::read(format!("tests/fixtures/{n}")).unwrap();
        let shifted = String::from_utf8(fixture("meeting_day_p2.json"))
            .unwrap()
            .replace("\"numberOfRecords\": 3", "\"numberOfRecords\": 4");
        let target = date(2026, 10, 20);
        let old = date(2026, 3, 1);
        let mut pages = HashMap::new();
        pages.insert(day_url(target, 1), fixture("meeting_day_p1.json"));
        pages.insert(day_url(target, 3), shifted.into_bytes());
        pages.insert(day_url(old, 1), br#"{"numberOfRecords":0}"#.to_vec());
        let http = Fake {
            pages,
            log: Mutex::new(vec![]),
        };
        let mut h = Harness::new();
        let stale = now() - jiff::SignedDuration::from_hours(24 * 40);
        h.fill(old, date(2026, 10, 21), &[target], old, stale);
        h.today = date(2026, 10, 21);
        for _ in 0..3 {
            h.step(&http);
        }
        let starts = |d: Date| day_url(d, 1);
        let log = http.log.lock().unwrap().clone();
        let firsts: Vec<_> = log
            .iter()
            .filter(|u| **u == starts(target) || **u == starts(old))
            .map(|u| if *u == starts(old) { "old" } else { "target" })
            .collect();
        assert_eq!(firsts, ["target", "old", "target"], "{log:?}");
        assert!(h.output().contains("will retry"), "{}", h.output());
    }

    #[test]
    fn lone_inconsistent_day_waits_a_minute_before_its_retry() {
        let fixture = |n: &str| std::fs::read(format!("tests/fixtures/{n}")).unwrap();
        let shifted = String::from_utf8(fixture("meeting_day_p2.json"))
            .unwrap()
            .replace("\"numberOfRecords\": 3", "\"numberOfRecords\": 4");
        let target = date(2026, 10, 20);
        let mut pages = HashMap::new();
        pages.insert(day_url(target, 1), fixture("meeting_day_p1.json"));
        pages.insert(day_url(target, 3), shifted.into_bytes());
        let http = Fake {
            pages,
            log: Mutex::new(vec![]),
        };
        let mut h = Harness::new();
        h.today = target;
        h.step(&http);
        assert!(h.slept.borrow().is_empty());
        h.step(&http);
        assert_eq!(*h.slept.borrow(), [Duration::from_secs(60)]);
        let starts = http
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|u| **u == day_url(target, 1))
            .count();
        assert_eq!(starts, 2);
    }

    #[test]
    fn failed_day_does_not_block_other_due_days() {
        let mut h = Harness::new();
        let old = date(2026, 3, 1);
        let stale = now() - jiff::SignedDuration::from_hours(24 * 40);
        h.fill(old, today().yesterday().unwrap(), &[], old, stale);
        let mut pages = HashMap::new();
        pages.insert(day_url(old, 1), br#"{"numberOfRecords":0}"#.to_vec());
        let http = Fake {
            pages,
            log: Mutex::new(vec![]),
        };
        h.step(&http);
        assert_eq!(*h.slept.borrow(), [Duration::from_secs(600)]);
        h.step(&http);
        let log = http.log.lock().unwrap().clone();
        assert_eq!(
            log,
            [day_url(today(), 1), day_url(old.yesterday().unwrap(), 1)]
        );
    }

    #[test]
    fn every_request_is_logged_with_time_url_and_result() {
        let mut h = Harness::new();
        let http = fake(vec![(today(), r#"{"numberOfRecords":0}"#)]);
        h.step(&http);
        let out = h.output();
        let line = out.lines().next().unwrap();
        assert!(
            line.contains(&day_url(today(), 1)) && line.ends_with("ok"),
            "{out}"
        );
        assert!(line.starts_with("20"), "{out}");
    }

    #[test]
    fn second_crawler_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite");
        let first = lock_database(&path).unwrap();
        let err = lock_database(&path).unwrap_err();
        assert!(err.to_string().contains("another crawler"), "{err}");
        drop(first);
        lock_database(&path).unwrap();
    }
}
