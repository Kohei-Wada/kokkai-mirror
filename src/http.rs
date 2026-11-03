use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ATTEMPTS: u32 = 5;
const BACKOFF: f64 = 2.0;
const RETRY_AFTER_CAP: f64 = 300.0;
pub const NDL_SPACING: f64 = 3.0;

#[derive(Debug)]
pub enum HttpError {
    NotFound(String),
    GaveUp(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::NotFound(url) => write!(f, "404 {url}"),
            HttpError::GaveUp(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for HttpError {}

pub trait Http: Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>, HttpError>;
}

enum Failure {
    Status(u16, Option<String>),
    Other(String),
}

type Opener = Box<dyn Fn(&str, &str) -> Result<Vec<u8>, Failure> + Send + Sync>;
type Sleep = Box<dyn Fn(f64) + Send + Sync>;
type Now = Box<dyn Fn() -> f64 + Send + Sync>;

pub struct Client {
    user_agent: String,
    opener: Opener,
    sleep: Sleep,
    now: Now,
    mark: Mutex<Option<f64>>,
}

pub fn user_agent(operator: &str) -> anyhow::Result<String> {
    let operator = operator.trim();
    if operator.is_empty() || !(operator.contains('@') || operator.contains("http")) {
        anyhow::bail!("the operator contact must be an email address or a URL, got {operator:?}");
    }
    Ok(format!(
        "kokkai-mirror/{} (+https://github.com/Kohei-Wada/kokkai-mirror) {operator}",
        env!("CARGO_PKG_VERSION")
    ))
}

fn urlopen(agent: &ureq::Agent, url: &str, user_agent: &str) -> Result<Vec<u8>, Failure> {
    match agent.get(url).set("User-Agent", user_agent).call() {
        Ok(resp) => {
            let mut body = Vec::new();
            resp.into_reader()
                .read_to_end(&mut body)
                .map_err(|e| Failure::Other(e.to_string()))?;
            Ok(body)
        }
        Err(ureq::Error::Status(code, resp)) => Err(Failure::Status(
            code,
            resp.header("Retry-After").map(str::to_string),
        )),
        Err(e) => Err(Failure::Other(e.to_string())),
    }
}

impl Client {
    pub fn new(user_agent: String) -> Client {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(120))
            .build();
        let epoch = Instant::now();
        Self::with(
            user_agent,
            Box::new(move |url, ua| urlopen(&agent, url, ua)),
            Box::new(|s| std::thread::sleep(Duration::from_secs_f64(s))),
            Box::new(move || epoch.elapsed().as_secs_f64()),
        )
    }

    fn with(user_agent: String, opener: Opener, sleep: Sleep, now: Now) -> Client {
        Client {
            user_agent,
            opener,
            sleep,
            now,
            mark: Mutex::new(None),
        }
    }

    // NDL asks for a pause after each response (https://kokkai.ndl.go.jp/api.html), so the wait counts from its end.
    fn space(&self, mark: &mut Option<f64>) {
        let now = (self.now)();
        let wait = mark.map_or(0.0, |m| (m + NDL_SPACING - now).max(0.0));
        if wait > 0.0 {
            (self.sleep)(wait);
        }
        *mark = Some(now + wait);
    }
}

impl Http for Client {
    fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        // Held for the whole call: one request at a time.
        let mut mark = self.mark.lock().unwrap();
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            self.space(&mut mark);
            let mut pause = BACKOFF * 2f64.powi(attempt as i32);
            let reply = (self.opener)(url, &self.user_agent);
            *mark = Some((self.now)());
            match reply {
                Ok(body) => return Ok(body),
                Err(Failure::Status(404, _)) => return Err(HttpError::NotFound(url.to_string())),
                Err(Failure::Status(code, _)) if code < 500 && !matches!(code, 408 | 429) => {
                    return Err(HttpError::GaveUp(format!("gave up: {url}: HTTP {code}")));
                }
                Err(Failure::Status(code, retry_after)) => {
                    last = format!("HTTP {code}");
                    if matches!(code, 429 | 503) {
                        if let Some(secs) = retry_after
                            .and_then(|v| v.trim().parse::<f64>().ok())
                            .filter(|s| *s >= 0.0)
                        {
                            if secs > RETRY_AFTER_CAP {
                                return Err(HttpError::GaveUp(format!(
                                    "gave up: {url}: {last}, Retry-After {secs}s exceeds {RETRY_AFTER_CAP}s"
                                )));
                            }
                            pause = secs;
                        }
                    }
                }
                Err(Failure::Other(e)) => last = e,
            }
            if attempt < ATTEMPTS - 1 {
                (self.sleep)(pause);
            }
        }
        Err(HttpError::GaveUp(format!(
            "gave up after {ATTEMPTS} attempts: {url}: {last}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Log<T> = Arc<Mutex<Vec<T>>>;
    type Reply = Result<Vec<u8>, Failure>;

    fn client(responses: Vec<Reply>, times: Vec<f64>) -> (Client, Log<String>, Log<f64>) {
        let seen = Arc::new(Mutex::new(vec![]));
        let sleeps = Arc::new(Mutex::new(vec![]));
        let queue = Mutex::new(responses.into_iter());
        let clock = Mutex::new(times.into_iter());
        let (c, s) = (seen.clone(), sleeps.clone());
        let client = Client::with(
            "test-agent".to_string(),
            Box::new(move |url, ua| {
                c.lock().unwrap().push(format!("{url} {ua}"));
                queue
                    .lock()
                    .unwrap()
                    .next()
                    .unwrap_or(Err(Failure::Other("down".into())))
            }),
            Box::new(move |secs| s.lock().unwrap().push(secs)),
            Box::new(move || {
                let mut t = clock.lock().unwrap();
                t.next().unwrap_or(1e9)
            }),
        );
        (client, seen, sleeps)
    }

    fn spread(n: usize) -> Vec<f64> {
        (0..n).map(|i| i as f64 * 1000.0).collect()
    }

    #[test]
    fn ndl_waits_three_seconds_after_the_previous_response_ends() {
        let (c, _, sleeps) = client(vec![Ok(vec![]), Ok(vec![])], vec![0.0, 5.0, 6.0, 9.0]);
        c.get("https://kokkai.ndl.go.jp/api/speech?a=1").unwrap();
        c.get("https://kokkai.ndl.go.jp/api/speech?a=2").unwrap();
        let s = sleeps.lock().unwrap();
        assert_eq!(s.len(), 1);
        assert!((s[0] - 2.0).abs() < 1e-9, "{s:?}");
    }

    #[test]
    fn ndl_counts_a_failed_response_as_received() {
        let (c, seen, sleeps) = client(
            vec![Err(Failure::Status(500, None)), Ok(b"ok".to_vec())],
            vec![0.0, 4.0, 6.0, 7.0],
        );
        assert_eq!(c.get("https://kokkai.ndl.go.jp/api/speech").unwrap(), b"ok");
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(*sleeps.lock().unwrap(), [2.0, 1.0]);
    }

    #[test]
    fn retry_after_is_honoured() {
        let (c, _, sleeps) = client(
            vec![
                Err(Failure::Status(503, Some("7".into()))),
                Ok(b"ok".to_vec()),
            ],
            spread(10),
        );
        assert_eq!(c.get("https://x").unwrap(), b"ok");
        assert!(sleeps.lock().unwrap().contains(&7.0));
    }

    #[test]
    fn retry_after_over_cap_gives_up() {
        let (c, seen, _) = client(
            vec![Err(Failure::Status(429, Some("301".into()))), Ok(vec![])],
            spread(10),
        );
        assert!(matches!(c.get("https://x"), Err(HttpError::GaveUp(_))));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn invalid_retry_after_falls_back_to_backoff() {
        for value in ["-1", "NaN"] {
            let (c, seen, sleeps) = client(
                vec![
                    Err(Failure::Status(503, Some(value.into()))),
                    Ok(b"ok".to_vec()),
                ],
                spread(10),
            );
            assert_eq!(c.get("https://x").unwrap(), b"ok", "{value}");
            assert_eq!(seen.lock().unwrap().len(), 2, "{value}");
            assert_eq!(*sleeps.lock().unwrap(), [2.0], "{value}");
        }
    }

    #[test]
    fn not_found_is_not_retried() {
        let (c, seen, _) = client(vec![Err(Failure::Status(404, None))], spread(10));
        let e = c.get("https://x/missing").unwrap_err();
        assert!(matches!(e, HttpError::NotFound(_)));
        assert!(e.to_string().contains("https://x/missing"));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn other_client_errors_are_not_retried() {
        for code in [400, 401, 403, 410] {
            let (c, seen, sleeps) = client(
                vec![Err(Failure::Status(code, None)), Ok(vec![])],
                spread(10),
            );
            let e = c.get("https://x/bad").unwrap_err();
            assert!(matches!(e, HttpError::GaveUp(_)), "{code}");
            let msg = e.to_string();
            assert!(
                msg.contains("https://x/bad") && msg.contains(&format!("HTTP {code}")),
                "{msg}"
            );
            assert_eq!(seen.lock().unwrap().len(), 1, "{code}");
            assert!(sleeps.lock().unwrap().is_empty(), "{code}");
        }
    }

    #[test]
    fn timeouts_throttling_and_server_errors_are_retried() {
        for code in [408, 429, 500, 502, 503] {
            let (c, seen, _) = client(
                vec![Err(Failure::Status(code, None)), Ok(b"ok".to_vec())],
                spread(10),
            );
            assert_eq!(c.get("https://x").unwrap(), b"ok", "{code}");
            assert_eq!(seen.lock().unwrap().len(), 2, "{code}");
        }
    }

    #[test]
    fn gives_up_after_five_attempts_naming_the_url() {
        let (c, seen, sleeps) = client(vec![], spread(10));
        let e = c.get("https://x/down").unwrap_err();
        assert!(matches!(e, HttpError::GaveUp(_)));
        let msg = e.to_string();
        assert!(
            msg.contains("https://x/down") && msg.contains("5 attempts"),
            "{msg}"
        );
        assert_eq!(seen.lock().unwrap().len(), 5);
        assert_eq!(*sleeps.lock().unwrap(), [2.0, 4.0, 8.0, 16.0]);
    }

    #[test]
    fn sends_the_user_agent() {
        let (c, seen, _) = client(vec![Ok(vec![])], spread(2));
        c.get("https://x").unwrap();
        assert_eq!(*seen.lock().unwrap(), ["https://x test-agent".to_string()]);
    }

    #[test]
    fn user_agent_requires_a_contact() {
        assert!(user_agent("").is_err());
        assert!(user_agent("me").is_err());
        assert!(user_agent("me@example.org")
            .unwrap()
            .starts_with("kokkai-mirror/"));
        assert!(user_agent("https://example.org/contact").is_ok());
    }
}
