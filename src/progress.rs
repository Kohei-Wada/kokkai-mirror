use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use crate::crawl::Task;
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

    fn line(&self, text: &str) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{text}");
    }
}
