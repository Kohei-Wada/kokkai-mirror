# kokkai-mirror Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A single Rust binary that crawls the NDL 会議録 API into one SQLite file, newest days first and then back to 1947, keeps every version of every meeting, re-verifies continuously, and serves `/api/speech`, `/api/meeting_list` and `/api/meeting` in the NDL's shape within the date range it holds completely.

**Architecture:** The crawl unit is one meeting day: `/api/meeting?from=D&until=D&maximumRecords=10` pages give every meeting of that day with all speeches and their `createTime`/`updateTime`. `speech` and `meeting_list` answers are derived from those meeting records, so there is one crawl. `kokkai-mirror crawl` (the only writer, one request at a time) and `kokkai-mirror serve` (read-only) share the SQLite file in WAL mode.

**Tech Stack:** Rust 1.95 (`rust-toolchain.toml`, as kokkai-cli). Crates: `clap 4` (derive), `ureq 2`, `anyhow 1`, `flate2 1` (`default-features = false, features = ["zlib-rs"]`), `jiff 0.2` (serde), `serde 1`, `serde_json 1` (without `preserve_order`), `sha2 0.11`, `rusqlite 0.41` (`bundled`), `tiny_http 0.12`; dev: `tempfile 3`. Verified 2026-11-03: the first eight are kokkai-cli's; `rusqlite 0.41` bundled ships SQLite 3.54.1 with FTS5 and the `trigram` tokenizer (a `MATCH '科学技術'` hit in a probe); `tiny_http::Server::http`, `Response::from_string(..).with_status_code(..)` compile; `std::fs::File::try_lock` refuses a second lock on the same file.

**Spec:** `docs/superpowers/specs/2026-11-03-kokkai-mirror-design.md` (updated in this PR: order is approximate). Reuse from `~/ghq/github.com/Kohei-Wada/kokkai-cli`: `src/http.rs` (pacing, retries, tests), `src/cache.rs` (`sha256_hex`, gzip blobs), `src/progress.rs` (stderr progress).

## Global Constraints

- "one request at a time; wait 3 s after each response is fully received; honour 429/5xx with backoff and Retry-After." Not configurable; no flag, env var or code path makes it faster.
- "Max 100 records per `speech`/`meeting_list` request, 10 per `meeting`."
- "A descriptive User-Agent with a reachable contact (the operator's choice; the mirror must make the self-hoster set one)."
- "Every fetched response is kept with its fetch time and content hash." A changed record is never overwritten.
- "A query that reaches outside that range returns an error that states the covered range. No partial answers." A query without a date range means all time.
- Result set and `numberOfRecords` match the NDL for structured parameters; order and `any` are approximate, and every response says so.
- Older versions and dropped records are "never mixed into NDL-shaped answers".
- "a single binary and a single data file."
- English for code and docs; Conventional Commits; no features beyond the design (no web UI, no other sources, no NixOS module in this plan).

## Findings (live requests 2026-11-03, 11 in total, sequential, ≥4 s apart)

- `/api/meeting?issueID=…` returned a 502-speech meeting in one 645 KB response. A meeting record holds, per speech, every `/api/speech` field plus `createTime` and `updateTime`.
- An `/api/speech` record equals its meeting's fields plus the speech's fields, except `searchObject`: 0 in meeting records, equal to `speechOrder` in speech records (31 of 31 checked). Speech records have no `createTime`/`updateTime`, so meetings cannot be rebuilt from speeches; the reverse works.
- `/api/meeting_list` speech entries are `speechID`, `speechOrder`, `speaker`, `speechURL`. With a `speaker` filter it still listed all 27 speeches of the meeting (`/api/meeting` too), though `speech` matched 23. `any` filtering of `meeting_list` was not tested.
- Order: `speech` is date descending, then within a day 衆議院 before 参議院 and then meeting name; `meeting_list` is meeting name first (本会議, 予算委員会, then roughly by reading), then date descending, then house. The api.html claim "会議開催日の新しい順" holds for `speech` only. Readings are in no response, hence approximate order.
- `numberOfRecords` is exact for a date window; `nextRecordPosition` is present as `null` on the last page; zero hits is `{"numberOfRecords":0}` (kokkai-cli fixture). Errors come as `{"message":"(190xx)…","details":[…]}` in the body; `19001` means busy.
- Late registration and corrections are not documented by the NDL. Observed: a 2026-10-20 meeting was created 2026-10-23; the 2025-02-06 衆議院予算委員会 was created 2025-05-08 and all 502 speeches were updated 2025-07-25. So new days need re-checking for months, and an update can touch a whole meeting.
- On 2026-11-03 the newest meeting held by the NDL was 2026-10-29. The first meeting is 1947-05-20 (`100105254X00119470520`, api.html example).

## Decisions

- **Day crawl:** pages of `/api/meeting?from=D&until=D&maximumRecords=10&recordPacking=json&startRecord=1,11,…`. A day is applied only if every page reports the same `numberOfRecords` and the distinct `issueID`s equal it; otherwise its fetches are kept as evidence and the day is retried later.
- **Scheduler**, one task per step, in this priority:
  1. Head: each day from the newest crawled day + 1 to today (JST), newest first.
  2. Recent re-check: days whose age (today − date) is ≤ 180 days, due when last verified more than 1 day ago (age ≤ 30) or 7 days ago (age 31–180); most overdue first.
  3. Otherwise alternate: backfill (the day before the oldest crawled day, floor 1947-05-20) and old re-check (the day older than 180 days with the oldest `last_verified`). After the floor is reached, old re-check only.
  4. Nothing due: sleep 60 s.
- **Record hash:** SHA-256 of `serde_json::to_vec(&Value)` of the meeting record (keys sorted by serde_json's default map). `createTime`/`updateTime` are part of it.
- **Coverage:** the longest run of consecutive crawled days that ends at the newest crawled day. Empty store: no coverage. A query's range is `[from or 0000-01-01, until or 9999-12-31]`; `issueID`/`speechID` narrow it to the date in the ID (its last 8 digits) when no `from`/`until` is given.
- **Mirror order (approximate):** `speech`: date desc, house (衆議院, 参議院, then others), `issueID`, `speechOrder`. `meeting`/`meeting_list`: date desc, house, `issueID`.
- **Response note:** every NDL-shaped answer and mirror error carries a top-level `"mirror": {"coverage": {"from": "YYYY-MM-DD", "until": "YYYY-MM-DD"}, "approximate": [...]}`, where `approximate` is `["order"]`, plus `"any"` when `any` was given.
- **Parameters:** supported: `startRecord`, `maximumRecords`, `nameOfHouse`, `nameOfMeeting`, `any`, `speaker`, `from`, `until`, `speechNumber`, `speakerPosition`, `speakerGroup`, `speakerRole`, `speechID`, `issueID`, `sessionFrom`, `sessionTo`, `issueFrom`, `issueTo`, `recordPacking=json`. Unsupported, answered with an error naming them: `recordPacking=xml`, `supplementAndAppendix`, `contentsAndIndex`, `searchRange`, `closing`, any unknown parameter, and an all-hiragana term in `nameOfMeeting` or `speaker` (the NDL matches readings the mirror does not have).
- **`meeting_list`/`meeting` with speech filters:** hits are meetings with at least one matching speech; all their speeches are listed (as observed for `speaker`).
- **Errors:** NDL validation errors reuse the NDL code and message (19004, 19005, 19006, 19007, 19011, 19012, 19013, 19014, 19018, 19019) with HTTP 200 and the NDL body shape, as clients already parse that. Coverage and unsupported-parameter errors are HTTP 400 with the same shape: `{"message": "...", "details": [...], "mirror": {...}}`.
- **Locking:** `crawl` takes `File::try_lock` on the database file and exits with an error if another crawler holds it.

## Review Focus

1. A meeting appears or disappears between two pages of one day's crawl: the day is not applied, its fetches are kept, and it is retried later. Test `day_with_shifting_pages_is_not_applied` (Task 4).
2. The NDL answers 200 with `(19001)` busy in the body: no day is applied, the crawler waits 300 s before its next request. Test `busy_body_backs_off` (Task 5).
3. A second `crawl` on the same database: it refuses to start, so two crawlers can never double the pace. Test `second_crawler_is_refused` (Task 5).
4. A query without dates, or an `issueID` outside coverage, before the backfill reaches 1947: coverage error stating the range. Test `undated_query_is_a_coverage_error_until_the_floor` (Task 6).
5. The NDL drops a meeting from a re-checked day: it stays in versions, leaves NDL-shaped answers, and is reported. Test `dropped_meeting_is_hidden_and_reported` (Task 3).

---

### Task 1: Crate skeleton and paced NDL client

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore` (`/target`), `src/lib.rs`, `src/main.rs` (stub), `src/http.rs`

**Interfaces:**
- Produces:
  - `pub trait Http: Sync { fn get(&self, url: &str) -> Result<Vec<u8>, HttpError>; }`, `pub enum HttpError { NotFound(String), GaveUp(String) }`.
  - `pub struct Client`; `Client::new(user_agent: String) -> Client`.
  - `pub fn user_agent(operator: &str) -> anyhow::Result<String>`: errors unless `operator.trim()` is non-empty and contains `@` or `http`; returns `"kokkai-mirror/<version> (+https://github.com/Kohei-Wada/kokkai-mirror) <operator>"`.
  - `pub const NDL_SPACING: f64 = 3.0;`
- Port kokkai-cli `src/http.rs` and its tests unchanged in behaviour (attempts 5, backoff 2 s doubling, Retry-After cap 300 s, NDL spacing counted from the end of the previous response), with the User-Agent passed in instead of a constant. Drop the 1 s non-NDL spacing: every request goes to the NDL, so `spacing()` is `NDL_SPACING`.

- [ ] Port the tests: `ndl_waits_three_seconds_after_the_previous_response_ends`, `ndl_counts_a_failed_response_as_received`, `retry_after_is_honoured`, `retry_after_over_cap_gives_up`, `invalid_retry_after_falls_back_to_backoff`, `not_found_is_not_retried`, `other_client_errors_are_not_retried`, `timeouts_throttling_and_server_errors_are_retried`, `gives_up_after_five_attempts_naming_the_url`, `sends_the_user_agent`. Add `user_agent_requires_a_contact` (`""`, `"me"` fail; `"me@example.org"`, `"https://example.org/contact"` pass; result starts with `kokkai-mirror/`).
- [ ] Run `cargo test http`; expect failures, implement, rerun; expect all pass. `cargo clippy --all-targets -- -D warnings` clean.
- [ ] Commit `feat: add the paced NDL client`.

### Task 2: NDL records, hashes and derived shapes

**Files:**
- Create: `src/ndl.rs`, `tests/fixtures/meeting_day_p1.json`, `tests/fixtures/meeting_day_p2.json`, `tests/fixtures/speech_of_meeting.json`
- Fixtures are hand-written with invented text in the exact field set and key order seen in the findings (never real speech text: the repo will be public). `speech_of_meeting.json` is the `/api/speech` answer for one meeting of `meeting_day_p1.json`.

**Interfaces:**
- Produces:
  - `pub const API: &str = "https://kokkai.ndl.go.jp/api";`, `pub const FLOOR: Date` = 1947-05-20.
  - `pub fn day_url(date: Date, start: u32) -> String` (`/meeting?from=D&until=D&maximumRecords=10&recordPacking=json&startRecord=N`, params in that order).
  - `pub struct MeetingPage { pub total: u32, pub next: Option<u32>, pub meetings: Vec<Meeting> }`; `pub fn parse_page(body: &[u8]) -> Result<MeetingPage, NdlError>`.
  - `pub enum NdlError { Busy(String), Api(String), Parse(String) }`: a body with `message` starting `(19001)` is `Busy`, other `message` bodies `Api`.
  - `pub struct Meeting { pub issue_id: String, pub date: Date, pub record: serde_json::Value }` with `fn sha256(&self) -> String` (Decisions: record hash).
  - `pub struct Ordered(pub Vec<(&'static str, Value)>)` with a `Serialize` impl that writes fields in vector order (serde_json's `Map` sorts keys without `preserve_order`, and `preserve_order` would change the record hash's canonical form).
  - `pub fn speech_records(meeting: &Value) -> Vec<Ordered>`: NDL `speech` key order (`speechID, issueID, imageKind, searchObject, session, nameOfHouse, nameOfMeeting, issue, date, closing, speechOrder, speaker, speakerYomi, speakerGroup, speakerPosition, speakerRole, speech, startPage, speechURL, meetingURL, pdfURL`), `searchObject` = `speechOrder`, no `createTime`/`updateTime`.
  - `pub fn meeting_record(meeting: &Value) -> Ordered`: NDL `meeting` key order (`issueID, imageKind, searchObject, session, nameOfHouse, nameOfMeeting, issue, date, closing, speechRecord, meetingURL, pdfURL`; speech entries `speechID, speechOrder, speaker, speakerYomi, speakerGroup, speakerPosition, speakerRole, speech, startPage, createTime, updateTime, speechURL`).
  - `pub fn list_record(meeting: &Value) -> Ordered`: the same with `speechRecord` entries reduced to `speechID, speechOrder, speaker, speechURL`.

- [ ] Tests: `parses_a_day_page` (total, next, issue ids, dates), `last_page_has_null_next`, `meeting_record_keeps_ndl_key_order`, `speech_records_match_the_speech_api` (derived from the fixture meeting == `speech_of_meeting.json` records, field by field and in key order), `list_record_keeps_four_speech_fields`, `hash_ignores_key_order_but_not_update_time`, `busy_message_is_busy`, `other_message_is_an_api_error`.
- [ ] Run `cargo test ndl`, implement, rerun, commit `feat: parse NDL meeting pages and derive speech and list records`.

### Task 3: Store: versions, evidence, coverage

**Files:**
- Create: `src/store.rs`

**Schema** (`PRAGMA journal_mode=WAL`, `user_version = 1`):
- `blobs(sha256 TEXT PRIMARY KEY, body BLOB)`: gzip of each distinct response or record.
- `fetches(id INTEGER PRIMARY KEY, url TEXT, fetched_at TEXT, sha256 TEXT)`: every NDL response, applied or not.
- `meeting_versions(id INTEGER PRIMARY KEY, issue_id TEXT, sha256 TEXT, first_fetch INTEGER, last_fetch INTEGER, UNIQUE(issue_id, sha256))`; the record is in `blobs`.
- `meetings(issue_id TEXT PRIMARY KEY, date TEXT, session INTEGER, house TEXT, name TEXT, issue_no INTEGER, version_id INTEGER, dropped_at TEXT, last_verified TEXT)`; `issue_no` is the digits of `issue` (`第4号` → 4), 0 when there are none.
- `speeches(speech_id TEXT PRIMARY KEY, issue_id TEXT, speech_order INTEGER, speaker TEXT, yomi TEXT, grp TEXT, position TEXT, role TEXT, text TEXT)`: current versions of non-dropped meetings only; `speech_fts` = `fts5(text, content='speeches', tokenize='trigram')`.
- `changes(id INTEGER PRIMARY KEY, issue_id TEXT, kind TEXT, from_version INTEGER, to_version INTEGER, detected_at TEXT, speech_ids TEXT)`; `kind` ∈ `changed`, `dropped`, `returned`; `speech_ids` is a JSON array of speeches added, removed or differing.
- `days(date TEXT PRIMARY KEY, first_fetched TEXT, last_verified TEXT, meetings INTEGER)`.

**Interfaces:**
- Consumes: `Meeting` (Task 2).
- Produces:
  - `Store::open(path: &Path) -> anyhow::Result<Store>`, `Store::open_read_only(path: &Path)`.
  - `fn record_fetch(&self, url: &str, at: Timestamp, body: &[u8]) -> anyhow::Result<i64>`.
  - `fn apply_day(&self, date: Date, meetings: &[(Meeting, i64)], at: Timestamp) -> anyhow::Result<DayReport>` in one transaction; `DayReport { new: usize, changed: usize, dropped: usize, returned: usize }`. Unchanged meeting: `last_fetch`/`last_verified` updated. A previously held meeting absent from the day: `dropped_at` set, its speeches removed from `speeches`, a `dropped` change.
  - `fn coverage(&self) -> anyhow::Result<Option<(Date, Date)>>`, `fn days(&self) -> anyhow::Result<Vec<(Date, Timestamp)>>` (date, last_verified).
  - `fn record_history(&self, issue_id: &str) -> anyhow::Result<Option<History>>` and `fn changes(&self, from: Date, until: Date) -> anyhow::Result<RangeReport>`: `History { versions: Vec<VersionInfo>, last_verified, dropped_at }`, `VersionInfo { sha256, first_seen, last_seen, url }`; `RangeReport { days, oldest_verified, newest_verified, changes: Vec<ChangeRow> }`.
  - `fn version_record(&self, version_id: i64) -> anyhow::Result<Value>`.

- [ ] Tests (temp files): `first_apply_stores_meetings_and_speeches`, `same_content_only_bumps_verification`, `changed_meeting_keeps_the_old_version_and_reports_speeches`, `dropped_meeting_is_hidden_and_reported`, `returning_meeting_is_reported`, `coverage_is_the_run_ending_at_the_newest_day` (days 10-01, 10-03, 10-04 → 10-03..10-04), `every_fetch_is_kept_with_its_hash`.
- [ ] Run `cargo test store`, implement, rerun, commit `feat: store meeting versions, evidence and coverage`.

### Task 4: Crawl one day

**Files:**
- Create: `src/crawl.rs`

**Interfaces:**
- Consumes: `Http` (Task 1), `day_url`, `parse_page`, `NdlError` (Task 2), `Store::record_fetch`, `Store::apply_day` (Task 3).
- Produces: `pub fn crawl_day(http: &dyn Http, store: &Store, date: Date, now: &dyn Fn() -> Timestamp) -> Result<DayOutcome, CrawlError>`; `pub enum DayOutcome { Applied(DayReport), Inconsistent(String) }`; `pub enum CrawlError { Busy, Ndl(String), Http(HttpError), Store(anyhow::Error) }`.
- Follows `nextRecordPosition`; a position that does not advance is `Ndl`. Every response is recorded before parsing.

- [ ] Tests with a fake `Http` serving fixtures: `crawls_both_pages_of_a_day`, `empty_day_is_applied_with_no_meetings` (`{"numberOfRecords":0}`), `day_with_shifting_pages_is_not_applied` (page 2 reports another total, or repeats an issueID), `busy_body_is_busy_and_kept_as_evidence`, `requests_are_one_at_a_time_in_page_order`.
- [ ] Run `cargo test crawl`, implement, rerun, commit `feat: crawl one meeting day`.

### Task 5: Scheduler and `crawl` command

**Files:**
- Modify: `src/crawl.rs`, `src/main.rs`
- Create: `src/progress.rs` (port the parts of kokkai-cli's that print one line per request on stderr)

**Interfaces:**
- Produces:
  - `pub enum Task { Head(Date), Recent(Date), Backfill(Date), Recheck(Date) }`; `pub fn next_task(days: &[(Date, Timestamp)], today: Date, now: Timestamp, last_slot_was_backfill: bool) -> Option<Task>` (Decisions: scheduler).
  - `pub fn run(http: &dyn Http, store: &Store, progress: &Progress) -> anyhow::Result<()>`: loops forever; `Inconsistent` → retry that day after the next task; `Busy` → sleep 300 s; `Http::GaveUp` or `Ndl` → log and sleep 600 s; never a shorter wait than the client's pace.
  - CLI: `kokkai-mirror crawl --db <path> --user-agent <contact>` (both required), takes the file lock, then `run`.
- Today is the JST date (fixed +9 offset, as kokkai-cli `ndl::today`).

- [ ] Tests: `head_days_come_first_newest_first`, `recent_days_are_rechecked_daily_then_weekly` (age 10 verified 2 days ago is due; age 60 verified 3 days ago is not), `backfill_and_old_recheck_alternate`, `backfill_stops_at_the_floor`, `nothing_due_is_none`, `busy_body_backs_off` (fake sleep sees 300), `second_crawler_is_refused` (temp db, two locks).
- [ ] Run `cargo test`, implement, rerun. Manual check against the NDL (the only live step in this plan): `cargo run -- crawl --db /tmp/m.sqlite --user-agent <contact>` for ~1 minute; stderr shows requests ≥3 s apart (response end to next start) and today's head days; stop with Ctrl-C and restart: it resumes without refetching applied days.
- [ ] Commit `feat: schedule head, backfill and re-verification in one paced loop`.

### Task 6: Query engine

**Files:**
- Create: `src/query.rs`

**Interfaces:**
- Consumes: `Store` (Task 3), `speech_records`, `meeting_record`, `list_record`, `Ordered` (Task 2).
- Produces:
  - `pub enum Endpoint { Speech, MeetingList, Meeting }`.
  - `pub fn parse(endpoint: Endpoint, query: &str) -> Result<Query, Reply>` (URL-decoded pairs; validation per Decisions: parameters, errors; `maximumRecords` default 30/3, max 100/10).
  - `pub fn answer(store: &Store, q: &Query) -> anyhow::Result<Reply>`; `pub struct Reply { pub status: u16, pub body: String }`.
- Matching: `nameOfMeeting` and `speaker` partial and OR on spaces; `any` AND on spaces (FTS5 trigram `MATCH` for terms of ≥3 characters, `instr` for shorter ones); `speakerPosition`, `speakerGroup` partial; `speakerRole` exact, one of 証人/参考人/公述人, else 19011; `speechNumber` exact; `speechID`/`issueID` exact with the NDL format check (`[0-9A-Z]{21}`, then `_` and 3–4 digits); `nameOfHouse` 衆議院/参議院 exact, 両院 or 両院協議会 → houses starting with 両院, other values ignored; session and issue: one bound alone is an exact match, both a range. Only meetings with `dropped_at IS NULL` and their current speeches are searched.
- Coverage is checked before searching (Decisions: coverage); `startRecord` beyond the hit count is 19004 as in the NDL.

- [ ] Tests on a store filled with the fixtures: `speech_by_speaker_counts_and_pages` (`numberOfRecords`, `numberOfReturn`, `startRecord`, `nextRecordPosition` null on the last page), `meeting_list_lists_all_speeches_of_matching_meetings`, `meeting_returns_full_records_with_times`, `any_is_marked_approximate`, `order_is_always_marked_approximate`, `zero_hits_is_number_of_records_zero`, `undated_query_is_a_coverage_error_until_the_floor`, `issue_id_outside_coverage_is_a_coverage_error`, `range_inside_coverage_is_answered`, `unsupported_parameter_is_named`, `hiragana_speaker_is_unsupported`, `maximum_records_over_limit_is_19005_or_19006`, `from_after_until_is_19014`, `dropped_meetings_are_not_answered`.
- [ ] Run `cargo test query`, implement, rerun, commit `feat: answer NDL-shaped queries within coverage`.

### Task 7: `serve` command and mirror-only endpoints

**Files:**
- Create: `src/serve.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `query::{parse, answer, Endpoint}`, `Store::{open_read_only, coverage, record_history, changes}`.
- Produces:
  - `pub fn handle(store: &Store, url: &str) -> Reply`: `/api/speech`, `/api/meeting_list`, `/api/meeting`; `/mirror/coverage` → `{"from","until"}` or `{"from":null,"until":null}`; `/mirror/record?issueID=…` (or `speechID`) → versions with `sha256`, `first_seen`, `last_seen`, source `url`, `last_verified`, `dropped_at`, and with `&version=<sha256>` that version's record; `/mirror/changes?from=…&until=…` → `RangeReport` as JSON. Anything else 404.
  - CLI: `kokkai-mirror serve --db <path> [--listen 127.0.0.1:8080]`, answering requests one by one from `incoming_requests()`. Content type `application/json; charset=utf-8`.

- [ ] Tests: `routes_api_endpoints`, `record_lists_versions_and_returns_an_old_one`, `changes_reports_last_verified_for_a_range`, `unknown_path_is_404`, and `serves_over_http` (bind `127.0.0.1:0`, one request with `ureq`).
- [ ] Run `cargo test`, implement, rerun. Manual: with the database from Task 5, `curl 'http://127.0.0.1:8080/api/speech?from=<covered>&until=<covered>&maximumRecords=3&recordPacking=json'` gives records and a `mirror` note; without `from` it gives the coverage error.
- [ ] Commit `feat: serve the mirror and its version reports`.

### Task 8: README

**Files:**
- Create: `README.md`, `LICENSE` (MIT, as kokkai-cli)

- [ ] Write: what it is; build and run (`crawl`, `serve`, flags); the pace and that it cannot be lowered; the NDL terms (https://kokkai.ndl.go.jp/api.html) and the ask to send the NDL an inquiry stating purpose, pace and duration before a full crawl; copyright: speech text belongs to the speakers, the mirror is for the operator's own use and analysis, do not republish the text; coverage errors; what is approximate (order, `any`, `searchObject` in speech records follows the observed rule) and the unsupported parameters; the `mirror` note; mirror-only endpoints; storage size (~7.8 GB of text plus versions) and timing (head within hours; the full backfill on the order of days).
- [ ] Check every flag and endpoint in the README against `--help` and `serve.rs`. Commit `docs: add the README`.

---

## Self-review notes

- Spec coverage: crawl order, resume (Task 5), NDL-shaped endpoints (6–7), coverage errors (3, 6), evidence and versions (3), re-verification and report (3, 5, 7), shared pace budget (5), single binary and file (1, 3, 5), README items (8). Not covered by design: NixOS module (welcome but optional; left out).
- Open risk: `searchObject` = `speechOrder` and the `meeting_list` speech listing under `any` are inferred from few observations; both are listed as approximate.
