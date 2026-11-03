use std::collections::BTreeSet;

use jiff::civil::Date;
use jiff::Timestamp;

use crate::http::{Http, HttpError};
use crate::ndl::{day_url, parse_page, Meeting, NdlError};
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
