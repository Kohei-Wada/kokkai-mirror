use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use crate::crawl::Task;
use crate::http::{Http, HttpError};
use crate::store::DayReport;

pub struct Progress {
    out: Mutex<Box<dyn Write + Send>>,
}

impl Progress {
    pub fn new(out: Box<dyn Write + Send>) -> Progress {
        Progress {
            out: Mutex::new(out),
        }
    }

    pub fn stderr() -> Progress {
        Progress::new(Box::new(std::io::stderr()))
    }

    pub fn applied(&self, task: &Task, report: &DayReport) {
        self.line(&format!(
            "{} {}: new {}, changed {}, dropped {}, returned {}",
            task.kind(),
            task.date(),
            report.new,
            report.changed,
            report.dropped,
            report.returned
        ));
    }

    pub fn failed(&self, task: &Task, why: &str) {
        self.line(&format!("{} {}: {why}", task.kind(), task.date()));
    }

    pub fn sleeping(&self, wait: Duration, why: &str) {
        self.line(&format!("sleep {}s: {why}", wait.as_secs()));
    }

    pub fn request(&self, url: &str, result: &str) {
        self.line(&format!("{} GET {url} {result}", jiff::Timestamp::now()));
    }

    fn line(&self, text: &str) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{text}");
    }
}

pub struct LoggedHttp<'a> {
    inner: &'a dyn Http,
    progress: &'a Progress,
}

impl<'a> LoggedHttp<'a> {
    pub fn new(inner: &'a dyn Http, progress: &'a Progress) -> LoggedHttp<'a> {
        LoggedHttp { inner, progress }
    }
}

impl Http for LoggedHttp<'_> {
    fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let result = self.inner.get(url);
        match &result {
            Ok(_) => self.progress.request(url, "ok"),
            Err(e) => self.progress.request(url, &e.to_string()),
        }
        result
    }
}
