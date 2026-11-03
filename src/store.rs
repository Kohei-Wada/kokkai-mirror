use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use jiff::civil::Date;
use jiff::Timestamp;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ndl::Meeting;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct DayReport {
    pub new: usize,
    pub changed: usize,
    pub dropped: usize,
    pub returned: usize,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct VersionInfo {
    pub id: i64,
    pub sha256: String,
    pub first_seen: Timestamp,
    pub last_seen: Timestamp,
    pub url: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct History {
    pub versions: Vec<VersionInfo>,
    pub last_verified: Timestamp,
    pub dropped_at: Option<Timestamp>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ChangeRow {
    pub issue_id: String,
    pub date: Date,
    pub kind: String,
    pub from_version: Option<i64>,
    pub to_version: Option<i64>,
    pub detected_at: Timestamp,
    pub speech_ids: Vec<String>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RangeReport {
    pub days: usize,
    pub oldest_verified: Option<Timestamp>,
    pub newest_verified: Option<Timestamp>,
    pub changes: Vec<ChangeRow>,
}

const SCHEMA: &str = "
CREATE TABLE blobs(sha256 TEXT PRIMARY KEY, body BLOB);
CREATE TABLE fetches(id INTEGER PRIMARY KEY, url TEXT, fetched_at TEXT, sha256 TEXT);
CREATE TABLE meeting_versions(
    id INTEGER PRIMARY KEY, issue_id TEXT, sha256 TEXT,
    first_fetch INTEGER, last_fetch INTEGER, UNIQUE(issue_id, sha256));
CREATE TABLE meetings(
    issue_id TEXT PRIMARY KEY, date TEXT, session INTEGER, house TEXT, name TEXT,
    issue_no INTEGER, version_id INTEGER, dropped_at TEXT, last_verified TEXT);
CREATE INDEX meetings_date ON meetings(date);
CREATE TABLE speeches(
    speech_id TEXT PRIMARY KEY, issue_id TEXT, speech_order INTEGER, speaker TEXT,
    yomi TEXT, grp TEXT, position TEXT, role TEXT, text TEXT);
CREATE INDEX speeches_issue ON speeches(issue_id, speech_order);
CREATE VIRTUAL TABLE speech_fts USING fts5(text, content='speeches', tokenize='trigram');
CREATE TRIGGER speeches_ai AFTER INSERT ON speeches BEGIN
    INSERT INTO speech_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER speeches_ad AFTER DELETE ON speeches BEGIN
    INSERT INTO speech_fts(speech_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
END;
CREATE TRIGGER speeches_au AFTER UPDATE ON speeches BEGIN
    INSERT INTO speech_fts(speech_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
    INSERT INTO speech_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TABLE changes(
    id INTEGER PRIMARY KEY, issue_id TEXT, kind TEXT, from_version INTEGER,
    to_version INTEGER, detected_at TEXT, speech_ids TEXT);
CREATE INDEX changes_issue ON changes(issue_id);
CREATE TABLE days(date TEXT PRIMARY KEY, first_fetched TEXT, last_verified TEXT, meetings INTEGER);
PRAGMA user_version = 1;
";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

fn sha256_hex(b: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(b)
        .iter()
        .fold(String::with_capacity(64), |mut s, byte| {
            write!(s, "{byte:02x}").unwrap();
            s
        })
}

fn gzip(body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(body)?;
    Ok(enc.finish()?)
}

fn gunzip(gz: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    GzDecoder::new(gz).read_to_end(&mut body)?;
    Ok(body)
}

fn put_blob(conn: &Connection, body: &[u8]) -> anyhow::Result<String> {
    let sha = sha256_hex(body);
    let known = conn
        .query_row("SELECT 1 FROM blobs WHERE sha256 = ?1", [&sha], |_| Ok(()))
        .optional()?
        .is_some();
    if !known {
        conn.execute(
            "INSERT INTO blobs(sha256, body) VALUES (?1, ?2)",
            params![sha, gzip(body)?],
        )?;
    }
    Ok(sha)
}

fn parse_ts(s: &str) -> anyhow::Result<Timestamp> {
    s.parse()
        .with_context(|| format!("bad timestamp in store: {s}"))
}

fn parse_date(s: &str) -> anyhow::Result<Date> {
    s.parse().with_context(|| format!("bad date in store: {s}"))
}

fn issue_no(issue: &str) -> i64 {
    let ascii = |c: char| match c {
        '０'..='９' => char::from_u32(c as u32 - '０' as u32 + '0' as u32),
        c => Some(c),
    };
    let digits: String = issue
        .chars()
        .filter_map(ascii)
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(0)
}

fn speech_map(record: &Value) -> BTreeMap<String, &Value> {
    record
        .get("speechRecord")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|s| Some((s.get("speechID")?.as_str()?.to_string(), s)))
        .collect()
}

fn differing_speeches(old: &Value, new: &Value) -> Vec<String> {
    let (old, new) = (speech_map(old), speech_map(new));
    let ids: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    ids.into_iter()
        .filter(|id| old.get(*id) != new.get(*id))
        .cloned()
        .collect()
}

fn all_speeches(record: &Value) -> Vec<String> {
    speech_map(record).into_keys().collect()
}

struct Held {
    version_id: i64,
    sha256: String,
    dropped: bool,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        anyhow::ensure!(mode.eq_ignore_ascii_case("wal"), "WAL mode refused: {mode}");
        match conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? {
            0 => conn.execute_batch(&format!("BEGIN; {SCHEMA} COMMIT;"))?,
            1 => {}
            v => anyhow::bail!("{}: unknown schema version {v}", path.display()),
        }
        Ok(Store { conn })
    }

    pub fn open_read_only(path: &Path) -> anyhow::Result<Store> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        anyhow::ensure!(
            version == 1,
            "{}: not a kokkai-mirror database (schema version {version})",
            path.display()
        );
        Ok(Store { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn record_fetch(&self, url: &str, at: Timestamp, body: &[u8]) -> anyhow::Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let sha = put_blob(&tx, body)?;
        tx.execute(
            "INSERT INTO fetches(url, fetched_at, sha256) VALUES (?1, ?2, ?3)",
            params![url, at.to_string(), sha],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    pub fn apply_day(
        &self,
        date: Date,
        meetings: &[(Meeting, i64)],
        at: Timestamp,
    ) -> anyhow::Result<DayReport> {
        let tx = self.conn.unchecked_transaction()?;
        let now = at.to_string();
        let mut report = DayReport::default();
        let mut seen = BTreeSet::new();
        for (meeting, fetch) in meetings {
            seen.insert(meeting.issue_id.as_str());
            let held = tx
                .query_row(
                    "SELECT m.version_id, v.sha256, m.dropped_at IS NOT NULL
                     FROM meetings m JOIN meeting_versions v ON v.id = m.version_id
                     WHERE m.issue_id = ?1",
                    [&meeting.issue_id],
                    |r| {
                        Ok(Held {
                            version_id: r.get(0)?,
                            sha256: r.get(1)?,
                            dropped: r.get(2)?,
                        })
                    },
                )
                .optional()?;
            let sha = put_blob(&tx, &serde_json::to_vec(&meeting.record)?)?;
            tx.execute(
                "INSERT INTO meeting_versions(issue_id, sha256, first_fetch, last_fetch)
                 VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(issue_id, sha256) DO UPDATE SET last_fetch = excluded.last_fetch",
                params![meeting.issue_id, sha, fetch],
            )?;
            let version_id: i64 = tx.query_row(
                "SELECT id FROM meeting_versions WHERE issue_id = ?1 AND sha256 = ?2",
                params![meeting.issue_id, sha],
                |r| r.get(0),
            )?;
            if let Some(h) = &held {
                if !h.dropped && h.sha256 == sha {
                    tx.execute(
                        "UPDATE meetings SET last_verified = ?2 WHERE issue_id = ?1",
                        params![meeting.issue_id, now],
                    )?;
                    continue;
                }
            }
            let rec = &meeting.record;
            let text = |k: &str| rec.get(k).and_then(Value::as_str);
            tx.execute(
                "INSERT INTO meetings(issue_id, date, session, house, name, issue_no,
                     version_id, dropped_at, last_verified)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8)
                 ON CONFLICT(issue_id) DO UPDATE SET date = excluded.date,
                     session = excluded.session, house = excluded.house, name = excluded.name,
                     issue_no = excluded.issue_no, version_id = excluded.version_id,
                     dropped_at = NULL, last_verified = excluded.last_verified",
                params![
                    meeting.issue_id,
                    meeting.date.to_string(),
                    rec.get("session").and_then(Value::as_i64),
                    text("nameOfHouse"),
                    text("nameOfMeeting"),
                    issue_no(text("issue").unwrap_or("")),
                    version_id,
                    now,
                ],
            )?;
            replace_speeches(&tx, &meeting.issue_id, rec)?;
            let Some(h) = held else {
                report.new += 1;
                continue;
            };
            let (kind, ids) = if h.dropped {
                report.returned += 1;
                ("returned", all_speeches(rec))
            } else {
                report.changed += 1;
                let old = self.version_record(h.version_id)?;
                ("changed", differing_speeches(&old, rec))
            };
            insert_change(
                &tx,
                &meeting.issue_id,
                kind,
                Some(h.version_id),
                Some(version_id),
                &now,
                &ids,
            )?;
        }

        let held: Vec<(String, i64)> = tx
            .prepare(
                "SELECT issue_id, version_id FROM meetings
                 WHERE date = ?1 AND dropped_at IS NULL",
            )?
            .query_map([date.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (issue_id, version_id) in held {
            if seen.contains(issue_id.as_str()) {
                continue;
            }
            let ids = all_speeches(&self.version_record(version_id)?);
            tx.execute(
                "UPDATE meetings SET dropped_at = ?2 WHERE issue_id = ?1",
                params![issue_id, now],
            )?;
            tx.execute("DELETE FROM speeches WHERE issue_id = ?1", [&issue_id])?;
            insert_change(
                &tx,
                &issue_id,
                "dropped",
                Some(version_id),
                None,
                &now,
                &ids,
            )?;
            report.dropped += 1;
        }

        tx.execute(
            "INSERT INTO days(date, first_fetched, last_verified, meetings) VALUES (?1, ?2, ?2, ?3)
             ON CONFLICT(date) DO UPDATE SET last_verified = excluded.last_verified,
                 meetings = excluded.meetings",
            params![date.to_string(), now, meetings.len() as i64],
        )?;
        tx.commit()?;
        Ok(report)
    }

    pub fn coverage(&self) -> anyhow::Result<Option<(Date, Date)>> {
        let days = self.days()?;
        let Some(&(until, _)) = days.last() else {
            return Ok(None);
        };
        let mut from = until;
        for &(d, _) in days.iter().rev().skip(1) {
            if d.tomorrow()? != from {
                break;
            }
            from = d;
        }
        Ok(Some((from, until)))
    }

    pub fn days(&self) -> anyhow::Result<Vec<(Date, Timestamp)>> {
        let rows: Vec<(String, String)> = self
            .conn
            .prepare("SELECT date, last_verified FROM days ORDER BY date")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        rows.iter()
            .map(|(d, t)| Ok((parse_date(d)?, parse_ts(t)?)))
            .collect()
    }

    pub fn record_history(&self, issue_id: &str) -> anyhow::Result<Option<History>> {
        let Some((last_verified, dropped_at)) = self
            .conn
            .query_row(
                "SELECT last_verified, dropped_at FROM meetings WHERE issue_id = ?1",
                [issue_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let rows: Vec<(i64, String, String, String, String)> = self
            .conn
            .prepare(
                "SELECT v.id, v.sha256, f.fetched_at, l.fetched_at, f.url
                 FROM meeting_versions v
                 JOIN fetches f ON f.id = v.first_fetch
                 JOIN fetches l ON l.id = v.last_fetch
                 WHERE v.issue_id = ?1 ORDER BY v.id",
            )?
            .query_map([issue_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?;
        let versions = rows
            .into_iter()
            .map(|(id, sha256, first, last, url)| {
                Ok(VersionInfo {
                    id,
                    sha256,
                    first_seen: parse_ts(&first)?,
                    last_seen: parse_ts(&last)?,
                    url,
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Some(History {
            versions,
            last_verified: parse_ts(&last_verified)?,
            dropped_at: dropped_at.as_deref().map(parse_ts).transpose()?,
        }))
    }

    pub fn changes(&self, from: Date, until: Date) -> anyhow::Result<RangeReport> {
        let range = params![from.to_string(), until.to_string()];
        let verified: Vec<String> = self
            .conn
            .prepare("SELECT last_verified FROM days WHERE date BETWEEN ?1 AND ?2")?
            .query_map(range, |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let verified: Vec<Timestamp> = verified
            .iter()
            .map(|t| parse_ts(t))
            .collect::<anyhow::Result<_>>()?;
        type Row = (
            String,
            String,
            String,
            Option<i64>,
            Option<i64>,
            String,
            String,
        );
        let rows: Vec<Row> = self
            .conn
            .prepare(
                "SELECT c.issue_id, m.date, c.kind, c.from_version, c.to_version,
                     c.detected_at, c.speech_ids
                 FROM changes c JOIN meetings m USING (issue_id)
                 WHERE m.date BETWEEN ?1 AND ?2 ORDER BY c.id",
            )?
            .query_map(range, |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let changes = rows
            .into_iter()
            .map(
                |(issue_id, date, kind, from_version, to_version, detected_at, ids)| {
                    Ok(ChangeRow {
                        issue_id,
                        date: parse_date(&date)?,
                        kind,
                        from_version,
                        to_version,
                        detected_at: parse_ts(&detected_at)?,
                        speech_ids: serde_json::from_str(&ids)?,
                    })
                },
            )
            .collect::<anyhow::Result<_>>()?;
        Ok(RangeReport {
            days: verified.len(),
            oldest_verified: verified.iter().min().copied(),
            newest_verified: verified.iter().max().copied(),
            changes,
        })
    }

    pub fn version_record(&self, version_id: i64) -> anyhow::Result<Value> {
        let gz: Vec<u8> = self
            .conn
            .query_row(
                "SELECT b.body FROM meeting_versions v JOIN blobs b USING (sha256) WHERE v.id = ?1",
                [version_id],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("no meeting version {version_id}"))?;
        Ok(serde_json::from_slice(&gunzip(&gz)?)?)
    }
}

fn replace_speeches(conn: &Connection, issue_id: &str, record: &Value) -> anyhow::Result<()> {
    conn.execute("DELETE FROM speeches WHERE issue_id = ?1", [issue_id])?;
    let mut insert = conn.prepare(
        "INSERT INTO speeches(speech_id, issue_id, speech_order, speaker, yomi, grp, position,
             role, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for s in speech_map(record).into_values() {
        let text = |k: &str| s.get(k).and_then(Value::as_str);
        insert.execute(params![
            text("speechID"),
            issue_id,
            s.get("speechOrder").and_then(Value::as_i64),
            text("speaker"),
            text("speakerYomi"),
            text("speakerGroup"),
            text("speakerPosition"),
            text("speakerRole"),
            text("speech"),
        ])?;
    }
    Ok(())
}

fn insert_change(
    conn: &Connection,
    issue_id: &str,
    kind: &str,
    from_version: Option<i64>,
    to_version: Option<i64>,
    at: &str,
    speech_ids: &[String],
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO changes(issue_id, kind, from_version, to_version, detected_at, speech_ids)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            issue_id,
            kind,
            from_version,
            to_version,
            at,
            serde_json::to_string(speech_ids)?
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndl::{day_url, parse_page};
    use serde_json::json;
    use std::io::Read;

    const P1: &[u8] = include_bytes!("../tests/fixtures/meeting_day_p1.json");
    const P2: &[u8] = include_bytes!("../tests/fixtures/meeting_day_p2.json");
    const DAY: Date = Date::constant(2026, 10, 20);
    const M1: &str = "999900001X00120261020";
    const M3: &str = "999900003X00320261020";

    fn at(day: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + day * 86_400).unwrap()
    }

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("m.sqlite")).unwrap();
        (dir, store)
    }

    fn fetch_day(store: &Store, when: Timestamp) -> Vec<(Meeting, i64)> {
        let f1 = store.record_fetch(&day_url(DAY, 1), when, P1).unwrap();
        let f2 = store.record_fetch(&day_url(DAY, 11), when, P2).unwrap();
        let mut out: Vec<_> = parse_page(P1)
            .unwrap()
            .meetings
            .into_iter()
            .map(|m| (m, f1))
            .collect();
        out.extend(
            parse_page(P2)
                .unwrap()
                .meetings
                .into_iter()
                .map(|m| (m, f2)),
        );
        out
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn speech_ids(store: &Store, issue_id: &str) -> Vec<String> {
        let mut stmt = store
            .conn()
            .prepare("SELECT speech_id FROM speeches WHERE issue_id = ?1 ORDER BY speech_order")
            .unwrap();
        stmt.query_map([issue_id], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn fts_hits(store: &Store, term: &str) -> i64 {
        store
            .conn()
            .query_row(
                "SELECT count(*) FROM speech_fts WHERE speech_fts MATCH ?1",
                [term],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn first_apply_stores_meetings_and_speeches() {
        let (_dir, store) = temp_store();
        let day = fetch_day(&store, at(0));
        let report = store.apply_day(DAY, &day, at(0)).unwrap();
        assert_eq!(
            report,
            DayReport {
                new: 3,
                ..DayReport::default()
            }
        );
        assert_eq!(count(&store, "SELECT count(*) FROM meetings"), 3);
        assert_eq!(count(&store, "SELECT count(*) FROM speeches"), 9);
        assert_eq!(count(&store, "SELECT count(*) FROM meeting_versions"), 3);
        assert_eq!(count(&store, "SELECT count(*) FROM changes"), 0);
        let (session, house, name, issue_no): (i64, String, String, i64) = store
            .conn()
            .query_row(
                "SELECT session, house, name, issue_no FROM meetings WHERE issue_id = ?1",
                [M3],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((session, house.as_str(), issue_no), (999, "参議院", 3));
        assert_eq!(name, day[2].0.record["nameOfMeeting"].as_str().unwrap());
        assert_eq!(fts_hits(&store, "これより会議"), 1);
        assert_eq!(store.days().unwrap(), vec![(DAY, at(0))]);

        let history = store.record_history(M1).unwrap().unwrap();
        assert_eq!(history.versions.len(), 1);
        assert_eq!(history.versions[0].sha256, day[0].0.sha256());
        assert_eq!(history.versions[0].url, day_url(DAY, 1));
        assert_eq!(history.last_verified, at(0));
        assert_eq!(history.dropped_at, None);
        assert_eq!(
            store.version_record(history.versions[0].id).unwrap(),
            day[0].0.record
        );
        assert!(store.record_history("nope").unwrap().is_none());
    }

    #[test]
    fn same_content_only_bumps_verification() {
        let (_dir, store) = temp_store();
        let day = fetch_day(&store, at(0));
        store.apply_day(DAY, &day, at(0)).unwrap();
        let again = fetch_day(&store, at(1));
        let report = store.apply_day(DAY, &again, at(1)).unwrap();
        assert_eq!(report, DayReport::default());
        assert_eq!(count(&store, "SELECT count(*) FROM meeting_versions"), 3);
        assert_eq!(count(&store, "SELECT count(*) FROM changes"), 0);
        assert_eq!(store.days().unwrap(), vec![(DAY, at(1))]);
        let first_fetched: String = store
            .conn()
            .query_row("SELECT first_fetched FROM days", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first_fetched, at(0).to_string());
        let history = store.record_history(M1).unwrap().unwrap();
        assert_eq!(history.versions.len(), 1);
        assert_eq!(history.versions[0].first_seen, at(0));
        assert_eq!(history.versions[0].last_seen, at(1));
        assert_eq!(history.last_verified, at(1));
    }

    #[test]
    fn changed_meeting_keeps_the_old_version_and_reports_speeches() {
        let (_dir, store) = temp_store();
        let day = fetch_day(&store, at(0));
        store.apply_day(DAY, &day, at(0)).unwrap();
        let old = day[0].0.record.clone();

        let mut again = fetch_day(&store, at(1));
        let record = &mut again[0].0.record;
        record["speechRecord"][1]["updateTime"] = json!("2030-01-01 00:00:00");
        let speeches = record["speechRecord"].as_array_mut().unwrap();
        speeches.remove(2);
        speeches.push(json!({
            "speechID": "999900001X00120261020_003",
            "speechOrder": 3,
            "speaker": "追加次郎",
            "speech": "追加された発言です。",
        }));
        let report = store.apply_day(DAY, &again, at(1)).unwrap();
        assert_eq!(
            report,
            DayReport {
                changed: 1,
                ..DayReport::default()
            }
        );
        assert_eq!(
            speech_ids(&store, M1),
            [
                "999900001X00120261020_000",
                "999900001X00120261020_001",
                "999900001X00120261020_003"
            ]
        );
        assert_eq!(fts_hits(&store, "追加された"), 1);

        let history = store.record_history(M1).unwrap().unwrap();
        assert_eq!(history.versions.len(), 2);
        assert_eq!(store.version_record(history.versions[0].id).unwrap(), old);
        assert_eq!(
            store.version_record(history.versions[1].id).unwrap(),
            again[0].0.record
        );
        let r = store.changes(DAY, DAY).unwrap();
        assert_eq!(r.days, 1);
        assert_eq!(r.oldest_verified, Some(at(1)));
        assert_eq!(r.newest_verified, Some(at(1)));
        assert_eq!(r.changes.len(), 1);
        let c = &r.changes[0];
        assert_eq!(
            (c.issue_id.as_str(), c.kind.as_str(), c.date),
            (M1, "changed", DAY)
        );
        assert_eq!(c.from_version, Some(history.versions[0].id));
        assert_eq!(c.to_version, Some(history.versions[1].id));
        assert_eq!(c.detected_at, at(1));
        assert_eq!(
            c.speech_ids,
            [
                "999900001X00120261020_001",
                "999900001X00120261020_002",
                "999900001X00120261020_003"
            ]
        );
        assert!(store
            .changes(Date::constant(2026, 10, 21), Date::constant(2026, 10, 31))
            .unwrap()
            .changes
            .is_empty());
    }

    fn other_day_meeting() -> Meeting {
        let mut record = parse_page(P1).unwrap().meetings.remove(0).record;
        record["issueID"] = json!("999900009X00120261021");
        record["date"] = json!("2026-10-21");
        for s in record["speechRecord"].as_array_mut().unwrap() {
            let id = s["speechID"]
                .as_str()
                .unwrap()
                .replace(M1, "999900009X00120261021");
            s["speechID"] = json!(id);
        }
        Meeting {
            issue_id: "999900009X00120261021".into(),
            date: Date::constant(2026, 10, 21),
            record,
        }
    }

    #[test]
    fn dropped_meeting_is_hidden_and_reported() {
        let (_dir, store) = temp_store();
        let next = Date::constant(2026, 10, 21);
        let f = store.record_fetch("u", at(0), b"x").unwrap();
        store
            .apply_day(next, &[(other_day_meeting(), f)], at(0))
            .unwrap();
        let day = fetch_day(&store, at(0));
        store.apply_day(DAY, &day, at(0)).unwrap();
        let before = fts_hits(&store, "これより会議");

        let mut again = fetch_day(&store, at(1));
        again.remove(2);
        let report = store.apply_day(DAY, &again, at(1)).unwrap();
        assert_eq!(
            report,
            DayReport {
                dropped: 1,
                ..DayReport::default()
            }
        );
        assert!(speech_ids(&store, M3).is_empty());
        assert_eq!(speech_ids(&store, "999900009X00120261021").len(), 3);
        assert_eq!(fts_hits(&store, "これより会議"), before);
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM meetings WHERE dropped_at IS NOT NULL"
            ),
            1
        );

        let history = store.record_history(M3).unwrap().unwrap();
        assert_eq!(history.dropped_at, Some(at(1)));
        assert_eq!(history.versions.len(), 1);
        assert_eq!(
            store.version_record(history.versions[0].id).unwrap(),
            day[2].0.record
        );
        let r = store.changes(DAY, next).unwrap();
        assert_eq!(r.days, 2);
        assert_eq!(r.oldest_verified, Some(at(0)));
        assert_eq!(r.newest_verified, Some(at(1)));
        assert_eq!(r.changes.len(), 1);
        let c = &r.changes[0];
        assert_eq!((c.issue_id.as_str(), c.kind.as_str()), (M3, "dropped"));
        assert_eq!(c.from_version, Some(history.versions[0].id));
        assert_eq!(c.to_version, None);
        assert_eq!(c.speech_ids.len(), 4);

        let report = store.apply_day(DAY, &again, at(2)).unwrap();
        assert_eq!(report, DayReport::default());
        assert_eq!(
            store.record_history(M3).unwrap().unwrap().dropped_at,
            Some(at(1))
        );
    }

    #[test]
    fn returning_meeting_is_reported() {
        let (_dir, store) = temp_store();
        let day = fetch_day(&store, at(0));
        store.apply_day(DAY, &day, at(0)).unwrap();
        let mut without = fetch_day(&store, at(1));
        without.remove(2);
        store.apply_day(DAY, &without, at(1)).unwrap();

        let back = fetch_day(&store, at(2));
        let report = store.apply_day(DAY, &back, at(2)).unwrap();
        assert_eq!(
            report,
            DayReport {
                returned: 1,
                ..DayReport::default()
            }
        );
        assert_eq!(speech_ids(&store, M3).len(), 4);
        let history = store.record_history(M3).unwrap().unwrap();
        assert_eq!(history.dropped_at, None);
        assert_eq!(history.versions.len(), 1);
        assert_eq!(history.last_verified, at(2));
        let kinds: Vec<_> = store
            .changes(DAY, DAY)
            .unwrap()
            .changes
            .into_iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(kinds, ["dropped", "returned"]);
    }

    #[test]
    fn coverage_is_the_run_ending_at_the_newest_day() {
        let (_dir, store) = temp_store();
        assert_eq!(store.coverage().unwrap(), None);
        for d in [4, 1, 3] {
            store
                .apply_day(Date::constant(2026, 10, d), &[], at(d.into()))
                .unwrap();
        }
        assert_eq!(
            store.coverage().unwrap(),
            Some((Date::constant(2026, 10, 3), Date::constant(2026, 10, 4)))
        );
        assert_eq!(
            store.days().unwrap(),
            vec![
                (Date::constant(2026, 10, 1), at(1)),
                (Date::constant(2026, 10, 3), at(3)),
                (Date::constant(2026, 10, 4), at(4)),
            ]
        );
    }

    #[test]
    fn every_fetch_is_kept_with_its_hash() {
        let (_dir, store) = temp_store();
        let a = store.record_fetch("u1", at(0), P1).unwrap();
        let b = store.record_fetch("u1", at(1), P1).unwrap();
        let c = store
            .record_fetch("u2", at(2), b"{\"message\":\"(19001)\"}")
            .unwrap();
        assert!(a < b && b < c);
        assert_eq!(count(&store, "SELECT count(*) FROM fetches"), 3);
        assert_eq!(count(&store, "SELECT count(*) FROM blobs"), 2);
        let (url, fetched_at, sha, gz): (String, String, String, Vec<u8>) = store
            .conn()
            .query_row(
                "SELECT url, fetched_at, f.sha256, body FROM fetches f JOIN blobs USING (sha256) WHERE id = ?1",
                [b],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((url.as_str(), fetched_at), ("u1", at(1).to_string()));
        assert_eq!(sha, sha256_hex(P1));
        let mut body = Vec::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, P1);
    }

    #[test]
    fn issue_no_is_the_first_number() {
        for (issue, no) in [
            ("第1号", 1),
            ("第12号", 12),
            ("第1号(その2)", 1),
            ("第１２号", 12),
            ("目次", 0),
        ] {
            assert_eq!(issue_no(issue), no, "{issue}");
        }
    }

    #[test]
    fn read_only_open_needs_an_existing_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite");
        assert!(Store::open_read_only(&path).is_err());
        assert!(!path.exists());
        drop(Connection::open(&path).unwrap());
        assert!(Store::open_read_only(&path).is_err());

        let store = Store::open(&path).unwrap();
        store.apply_day(DAY, &[], at(0)).unwrap();
        let reader = Store::open_read_only(&path).unwrap();
        assert_eq!(reader.days().unwrap(), vec![(DAY, at(0))]);
        assert!(reader.record_fetch("u", at(1), b"x").is_err());
    }
}
