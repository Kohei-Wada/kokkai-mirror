use std::collections::hash_map::{Entry, HashMap};

use jiff::civil::Date;
use rusqlite::types::Value as Sql;
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{json, Value};

use crate::ndl::{self, Ordered};
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Speech,
    MeetingList,
    Meeting,
}

#[derive(Debug, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

#[derive(Debug)]
enum House {
    Exact(&'static str),
    Joint,
}

#[derive(Debug)]
pub struct Query {
    endpoint: Endpoint,
    start: u32,
    max: u32,
    from: Option<Date>,
    until: Option<Date>,
    id_date: Option<Date>,
    house: Option<House>,
    meeting_names: Vec<String>,
    speakers: Vec<String>,
    any: Vec<String>,
    position: Option<String>,
    group: Option<String>,
    role: Option<String>,
    speech_number: Option<i64>,
    speech_id: Option<String>,
    issue_id: Option<String>,
    session: Option<(i64, i64)>,
    issue: Option<(i64, i64)>,
    unsupported: Vec<String>,
}

const SUPPORTED: [&str; 19] = [
    "startRecord",
    "maximumRecords",
    "nameOfHouse",
    "nameOfMeeting",
    "any",
    "speaker",
    "from",
    "until",
    "speechNumber",
    "speakerPosition",
    "speakerGroup",
    "speakerRole",
    "speechID",
    "issueID",
    "sessionFrom",
    "sessionTo",
    "issueFrom",
    "issueTo",
    "recordPacking",
];

const CONDITIONS: [&str; 16] = [
    "nameOfHouse",
    "nameOfMeeting",
    "any",
    "speaker",
    "from",
    "until",
    "speechNumber",
    "speakerPosition",
    "speakerGroup",
    "speakerRole",
    "speechID",
    "issueID",
    "sessionFrom",
    "sessionTo",
    "issueFrom",
    "issueTo",
];

fn ndl_message(code: u32) -> &'static str {
    match code {
        19004 => "startRecordには 1から検索件数までの値を指定してください。",
        19005 => "maximumRecordsには1～100の値を指定してください。",
        19006 => "maximumRecordsには1～10の値を指定してください。",
        19007 => "検索条件を指定してください。",
        19012 => "from:開会日付をYYYY-MM-DD形式で入力してください。",
        19013 => "until:開会日付をYYYY-MM-DD形式で入力してください。",
        19014 => "fromはuntil以下、もしくはuntilはfrom以上の日付で入力してください。",
        19018 => "from:存在しない日付です。検索条件を見直し、再度検索してください。",
        19019 => "until:存在しない日付です。検索条件を見直し、再度検索してください。",
        _ => "検索条件の入力に誤りがあります。",
    }
}

enum Field {
    Json(Value),
    Records(Vec<Ordered>),
    Note(Ordered),
}

struct Obj(Vec<(&'static str, Field)>);

impl Serialize for Obj {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            match v {
                Field::Json(v) => map.serialize_entry(k, v)?,
                Field::Records(r) => map.serialize_entry(k, r)?,
                Field::Note(o) => map.serialize_entry(k, o)?,
            }
        }
        map.end()
    }
}

fn body(obj: Obj) -> String {
    serde_json::to_string(&obj).expect("JSON values always serialize")
}

fn ndl_error(code: u32) -> Reply {
    let message = format!("({code}){}", ndl_message(code));
    Reply {
        status: 200,
        body: body(Obj(vec![
            ("message", Field::Json(json!(message))),
            ("details", Field::Json(json!([message]))),
        ])),
    }
}

fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn terms(v: &str) -> Vec<String> {
    v.split(' ')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn all_hiragana(term: &str) -> bool {
    term.chars()
        .all(|c| ('\u{3041}'..='\u{309F}').contains(&c) || c == 'ー')
}

fn valid_issue_id(id: &str) -> bool {
    id.len() == 21
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
}

fn valid_speech_id(id: &str) -> bool {
    id.split_once('_').is_some_and(|(issue, n)| {
        valid_issue_id(issue) && (3..=4).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())
    })
}

fn id_date(issue_id: &str) -> Option<Date> {
    let d = &issue_id[13..];
    Date::new(
        d[..4].parse().ok()?,
        d[4..6].parse().ok()?,
        d[6..].parse().ok()?,
    )
    .ok()
}

fn date_param(v: Option<&str>, format_code: u32, exists_code: u32) -> Result<Option<Date>, Reply> {
    let Some(v) = v else { return Ok(None) };
    let shaped = v.len() == 10
        && v.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if !shaped {
        return Err(ndl_error(format_code));
    }
    v.parse().map(Some).map_err(|_| ndl_error(exists_code))
}

fn bounds(lo: Option<&str>, hi: Option<&str>) -> Result<Option<(i64, i64)>, Reply> {
    let num = |v: &str| {
        v.parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .ok_or_else(|| ndl_error(19011))
    };
    Ok(match (lo.map(num).transpose()?, hi.map(num).transpose()?) {
        (Some(a), Some(b)) => Some((a, b)),
        (Some(a), None) | (None, Some(a)) => Some((a, a)),
        (None, None) => None,
    })
}

pub fn parse(endpoint: Endpoint, query: &str) -> Result<Query, Reply> {
    let mut params: Vec<(String, String)> = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.push((
            decode(k).ok_or_else(|| ndl_error(19011))?,
            decode(v).ok_or_else(|| ndl_error(19011))?,
        ));
    }
    let get = |name: &str| {
        params
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };

    let start = match get("startRecord") {
        None => 1,
        Some(v) => v
            .parse()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| ndl_error(19004))?,
    };
    let (limit, limit_code) = match endpoint {
        Endpoint::Meeting => (10, 19006),
        _ => (100, 19005),
    };
    let max = match get("maximumRecords") {
        None if endpoint == Endpoint::Meeting => 3,
        None => 30,
        Some(v) => v
            .parse()
            .ok()
            .filter(|n| (1..=limit).contains(n))
            .ok_or_else(|| ndl_error(limit_code))?,
    };
    let from = date_param(get("from"), 19012, 19018)?;
    let until = date_param(get("until"), 19013, 19019)?;
    if let (Some(f), Some(u)) = (from, until) {
        if f > u {
            return Err(ndl_error(19014));
        }
    }
    let role = get("speakerRole").map(str::to_string);
    if role
        .as_deref()
        .is_some_and(|r| !["証人", "参考人", "公述人"].contains(&r))
    {
        return Err(ndl_error(19011));
    }
    let issue_id = get("issueID").map(str::to_string);
    let speech_id = get("speechID").map(str::to_string);
    if issue_id.as_deref().is_some_and(|i| !valid_issue_id(i))
        || speech_id.as_deref().is_some_and(|i| !valid_speech_id(i))
    {
        return Err(ndl_error(19011));
    }
    let speech_number = get("speechNumber")
        .map(|v| {
            v.parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
                .ok_or_else(|| ndl_error(19011))
        })
        .transpose()?;
    let session = bounds(get("sessionFrom"), get("sessionTo"))?;
    let issue = bounds(get("issueFrom"), get("issueTo"))?;
    if !CONDITIONS.iter().any(|c| get(c).is_some()) {
        return Err(ndl_error(19007));
    }

    let mut unsupported: Vec<String> = Vec::new();
    for (k, v) in &params {
        let name = match k.as_str() {
            "recordPacking" if !v.is_empty() && v != "json" => format!("recordPacking={v}"),
            k if SUPPORTED.contains(&k) => continue,
            k => k.to_string(),
        };
        if !unsupported.contains(&name) {
            unsupported.push(name);
        }
    }
    let meeting_names = get("nameOfMeeting").map(terms).unwrap_or_default();
    let speakers = get("speaker").map(terms).unwrap_or_default();
    for (k, list) in [("nameOfMeeting", &meeting_names), ("speaker", &speakers)] {
        for t in list.iter().filter(|t| all_hiragana(t)) {
            unsupported.push(format!("{k}={t} (readings are not mirrored)"));
        }
    }

    let house = match get("nameOfHouse") {
        Some("衆議院") => Some(House::Exact("衆議院")),
        Some("参議院") => Some(House::Exact("参議院")),
        Some("両院" | "両院協議会") => Some(House::Joint),
        _ => None,
    };
    let id_date = issue_id
        .as_deref()
        .or(speech_id.as_deref().map(|s| &s[..21]))
        .and_then(id_date);
    Ok(Query {
        endpoint,
        start,
        max,
        from,
        until,
        id_date,
        house,
        meeting_names,
        speakers,
        any: get("any").map(terms).unwrap_or_default(),
        position: get("speakerPosition").map(str::to_string),
        group: get("speakerGroup").map(str::to_string),
        role,
        speech_number,
        speech_id,
        issue_id,
        session,
        issue,
        unsupported,
    })
}

#[derive(Default)]
struct Where {
    sql: Vec<String>,
    args: Vec<Sql>,
}

impl Where {
    fn push(&mut self, sql: impl Into<String>, args: impl IntoIterator<Item = Sql>) {
        self.sql.push(sql.into());
        self.args.extend(args);
    }

    fn any_of(&mut self, col: &str, terms: &[String]) {
        if terms.is_empty() {
            return;
        }
        let ors = vec![format!("instr({col}, ?) > 0"); terms.len()].join(" OR ");
        self.push(
            format!("({ors})"),
            terms.iter().map(|t| Sql::Text(t.clone())),
        );
    }

    fn text(&self) -> String {
        self.sql.join(" AND ")
    }
}

fn meeting_where(q: &Query, lo: Date, hi: Date) -> Where {
    let mut w = Where::default();
    w.push("m.dropped_at IS NULL", []);
    w.push(
        "m.date BETWEEN ? AND ?",
        [Sql::Text(lo.to_string()), Sql::Text(hi.to_string())],
    );
    match q.house {
        Some(House::Exact(h)) => w.push("m.house = ?", [Sql::Text(h.into())]),
        Some(House::Joint) => w.push("m.house LIKE '両院%'", []),
        None => {}
    }
    w.any_of("m.name", &q.meeting_names);
    if let Some(id) = &q.issue_id {
        w.push("m.issue_id = ?", [Sql::Text(id.clone())]);
    }
    for (col, b) in [("m.session", q.session), ("m.issue_no", q.issue)] {
        if let Some((a, b)) = b {
            w.push(
                format!("{col} BETWEEN ? AND ?"),
                [Sql::Integer(a), Sql::Integer(b)],
            );
        }
    }
    w
}

fn speech_where(q: &Query) -> Where {
    let mut w = Where::default();
    w.any_of("s.speaker", &q.speakers);
    for (col, v) in [("s.position", &q.position), ("s.grp", &q.group)] {
        if let Some(v) = v {
            w.push(format!("instr({col}, ?) > 0"), [Sql::Text(v.clone())]);
        }
    }
    if let Some(r) = &q.role {
        w.push("s.role = ?", [Sql::Text(r.clone())]);
    }
    if let Some(n) = q.speech_number {
        w.push("s.speech_order = ?", [Sql::Integer(n)]);
    }
    if let Some(id) = &q.speech_id {
        w.push("s.speech_id = ?", [Sql::Text(id.clone())]);
    }
    for t in &q.any {
        if t.chars().count() >= 3 {
            w.push(
                "s.rowid IN (SELECT rowid FROM speech_fts WHERE speech_fts MATCH ?)",
                [Sql::Text(format!("\"{}\"", t.replace('"', "\"\"")))],
            );
        } else {
            w.push("instr(s.text, ?) > 0", [Sql::Text(t.clone())]);
        }
    }
    w
}

const HOUSE_RANK: &str = "CASE m.house WHEN '衆議院' THEN 0 WHEN '参議院' THEN 1 ELSE 2 END";

pub fn answer(store: &Store, q: &Query, today: Date) -> anyhow::Result<Reply> {
    let coverage = store.coverage()?;
    let mut approximate = vec!["order"];
    if !q.any.is_empty() {
        approximate.push("any");
    }
    let note = || {
        Field::Note(Ordered(vec![
            (
                "coverage",
                coverage.map_or(
                    Value::Null,
                    |(f, u)| json!({"from": f.to_string(), "until": u.to_string()}),
                ),
            ),
            ("approximate", json!(approximate)),
        ]))
    };
    let mirror_error = |message: String, details: Vec<String>| Reply {
        status: 400,
        body: body(Obj(vec![
            ("message", Field::Json(json!(message))),
            ("details", Field::Json(json!(details))),
            ("mirror", note()),
        ])),
    };

    if !q.unsupported.is_empty() {
        return Ok(mirror_error(
            format!("unsupported by the mirror: {}", q.unsupported.join(", ")),
            q.unsupported.clone(),
        ));
    }

    let (from, until) = match (q.from, q.until, q.id_date) {
        (None, None, Some(d)) => (d, d),
        (f, u, _) => (f.unwrap_or(ndl::FLOOR), u.unwrap_or(today)),
    };
    let (lo, hi) = (from.max(ndl::FLOOR), until.min(today));
    let covered = match coverage {
        Some((f, u)) => lo > hi || (f <= lo && hi <= u),
        None => false,
    };
    if !covered {
        let held = coverage.map_or(
            "the mirror holds no complete days yet".to_string(),
            |(f, u)| format!("covered: {f} to {u}"),
        );
        return Ok(mirror_error(
            "the query reaches outside the range the mirror holds completely".into(),
            vec![format!("requested: {lo} to {hi}"), held],
        ));
    }

    let zero = || Reply {
        status: 200,
        body: body(Obj(vec![
            ("numberOfRecords", Field::Json(json!(0))),
            ("mirror", note()),
        ])),
    };
    if lo > hi {
        return Ok(zero());
    }

    let meetings = meeting_where(q, lo, hi);
    let speeches = speech_where(q);
    let (from_sql, order, args) = if q.endpoint == Endpoint::Speech {
        let mut args = meetings.args;
        args.extend(speeches.args);
        let mut cond = meetings.sql;
        cond.extend(speeches.sql);
        (
            format!(
                "FROM speeches s JOIN meetings m ON m.issue_id = s.issue_id WHERE {}",
                cond.join(" AND ")
            ),
            format!("m.date DESC, {HOUSE_RANK}, m.issue_id, s.speech_order"),
            args,
        )
    } else {
        let mut cond = meetings.text();
        let mut args = meetings.args;
        if !speeches.sql.is_empty() {
            cond = format!(
                "{cond} AND EXISTS (SELECT 1 FROM speeches s WHERE s.issue_id = m.issue_id AND {})",
                speeches.text()
            );
            args.extend(speeches.args);
        }
        (
            format!("FROM meetings m WHERE {cond}"),
            format!("m.date DESC, {HOUSE_RANK}, m.issue_id"),
            args,
        )
    };

    let conn = store.conn();
    let total: u32 = conn.query_row(
        &format!("SELECT count(*) {from_sql}"),
        rusqlite::params_from_iter(&args),
        |r| r.get(0),
    )?;
    if total == 0 {
        return Ok(zero());
    }
    if q.start > total {
        return Ok(ndl_error(19004));
    }

    let mut page_args = args.clone();
    page_args.push(Sql::Integer(q.max.into()));
    page_args.push(Sql::Integer(i64::from(q.start) - 1));
    let (key, records) = if q.endpoint == Endpoint::Speech {
        let rows: Vec<(i64, String)> = conn
            .prepare(&format!(
                "SELECT m.version_id, s.speech_id {from_sql} ORDER BY {order} LIMIT ? OFFSET ?"
            ))?
            .query_map(rusqlite::params_from_iter(&page_args), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut by_version: HashMap<i64, HashMap<String, Ordered>> = HashMap::new();
        let mut records = Vec::with_capacity(rows.len());
        for (version, speech_id) in rows {
            let indexed = match by_version.entry(version) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(
                    ndl::speech_records(&store.version_record(version)?)
                        .into_iter()
                        .filter_map(|o| {
                            let id = o.0.iter().find(|(k, _)| *k == "speechID")?.1.as_str()?;
                            Some((id.to_string(), o))
                        })
                        .collect(),
                ),
            };
            let record = indexed.remove(&speech_id).ok_or_else(|| {
                anyhow::anyhow!("speech {speech_id} missing from version {version}")
            })?;
            records.push(record);
        }
        ("speechRecord", records)
    } else {
        let versions: Vec<i64> = conn
            .prepare(&format!(
                "SELECT m.version_id {from_sql} ORDER BY {order} LIMIT ? OFFSET ?"
            ))?
            .query_map(rusqlite::params_from_iter(&page_args), |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let shape = if q.endpoint == Endpoint::Meeting {
            ndl::meeting_record
        } else {
            ndl::list_record
        };
        let records = versions
            .into_iter()
            .map(|v| Ok(shape(&store.version_record(v)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        ("meetingRecord", records)
    };

    let returned = records.len() as u32;
    let next = q.start + returned;
    Ok(Reply {
        status: 200,
        body: body(Obj(vec![
            ("numberOfRecords", Field::Json(json!(total))),
            ("numberOfReturn", Field::Json(json!(returned))),
            ("startRecord", Field::Json(json!(q.start))),
            (
                "nextRecordPosition",
                Field::Json(if next <= total {
                    json!(next)
                } else {
                    Value::Null
                }),
            ),
            (key, Field::Records(records)),
            ("mirror", note()),
        ])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndl::{self, day_url, parse_page, Meeting};
    use jiff::Timestamp;
    use serde_json::{json, Value};

    const P1: &[u8] = include_bytes!("../tests/fixtures/meeting_day_p1.json");
    const P2: &[u8] = include_bytes!("../tests/fixtures/meeting_day_p2.json");
    const DAY: Date = Date::constant(2026, 10, 20);
    const NEXT: Date = Date::constant(2026, 10, 21);
    const TODAY: Date = NEXT;
    const M1: &str = "999900001X00120261020";
    const M3: &str = "999900003X00320261020";
    const M9: &str = "999900009X00120261021";
    const RANGE: &str = "from=2026-10-20&until=2026-10-21";

    fn at(day: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + day * 86_400).unwrap()
    }

    fn day_meetings(store: &Store, when: Timestamp) -> Vec<(Meeting, i64)> {
        let mut out = Vec::new();
        for (page, start) in [(P1, 1), (P2, 11)] {
            let f = store
                .record_fetch(&day_url(DAY, start), when, page)
                .unwrap();
            out.extend(
                parse_page(page)
                    .unwrap()
                    .meetings
                    .into_iter()
                    .map(|m| (m, f)),
            );
        }
        out
    }

    fn next_day_meeting() -> Meeting {
        let mut record = parse_page(P1).unwrap().meetings.remove(0).record;
        record["issueID"] = json!(M9);
        record["date"] = json!("2026-10-21");
        for s in record["speechRecord"].as_array_mut().unwrap() {
            let id = s["speechID"].as_str().unwrap().replace(M1, M9);
            s["speechID"] = json!(id);
        }
        Meeting {
            issue_id: M9.into(),
            date: NEXT,
            record,
        }
    }

    fn filled() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("m.sqlite")).unwrap();
        let day = day_meetings(&store, at(0));
        store.apply_day(DAY, &day, at(0)).unwrap();
        let f = store.record_fetch("u", at(0), b"x").unwrap();
        store
            .apply_day(NEXT, &[(next_day_meeting(), f)], at(0))
            .unwrap();
        (dir, store)
    }

    fn reply(store: &Store, ep: Endpoint, q: &str, today: Date) -> Reply {
        match parse(ep, q) {
            Ok(query) => answer(store, &query, today).unwrap(),
            Err(r) => r,
        }
    }

    fn ask(store: &Store, ep: Endpoint, q: &str) -> (u16, Value) {
        let r = reply(store, ep, q, TODAY);
        (r.status, serde_json::from_str(&r.body).unwrap())
    }

    fn stored(store: &Store, issue_id: &str) -> Value {
        let v: i64 = store
            .conn()
            .query_row(
                "SELECT version_id FROM meetings WHERE issue_id = ?1",
                [issue_id],
                |r| r.get(0),
            )
            .unwrap();
        store.version_record(v).unwrap()
    }

    fn reparse<T: serde::Serialize>(v: &T) -> Value {
        serde_json::from_str(&serde_json::to_string(v).unwrap()).unwrap()
    }

    fn speech_ids(v: &Value) -> Vec<&str> {
        v["speechRecord"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["speechID"].as_str().unwrap())
            .collect()
    }

    fn issue_ids(v: &Value) -> Vec<&str> {
        v["meetingRecord"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["issueID"].as_str().unwrap())
            .collect()
    }

    fn coverage_note() -> Value {
        json!({"from": "2026-10-20", "until": "2026-10-21"})
    }

    #[test]
    fn speech_by_speaker_counts_and_pages() {
        let (_dir, store) = filled();
        let q = "speaker=架空太郎&from=2026-10-20&until=2026-10-20&maximumRecords=2";
        let r = reply(&store, Endpoint::Speech, q, TODAY);
        assert_eq!(r.status, 200);
        assert!(
            r.body.starts_with(
                "{\"numberOfRecords\":3,\"numberOfReturn\":2,\"startRecord\":1,\"nextRecordPosition\":3,\"speechRecord\":[{\"speechID\":"
            ),
            "{}",
            r.body
        );
        assert!(r.body.contains("}],\"mirror\":{\"coverage\":"));
        let v: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            speech_ids(&v),
            ["999900001X00120261020_001", "999900002X00220261020_001"]
        );
        let expected = reparse(&ndl::speech_records(&stored(&store, M1))[1]);
        assert_eq!(v["speechRecord"][0], expected);
        assert_eq!(v["speechRecord"][0]["searchObject"], json!(1));

        let (status, v) = ask(&store, Endpoint::Speech, &format!("{q}&startRecord=3"));
        assert_eq!(status, 200);
        assert_eq!(v["numberOfRecords"], 3);
        assert_eq!(v["numberOfReturn"], 1);
        assert_eq!(v["startRecord"], 3);
        assert_eq!(v["nextRecordPosition"], Value::Null);
        assert!(v.as_object().unwrap().contains_key("nextRecordPosition"));
        assert_eq!(speech_ids(&v), ["999900003X00320261020_002"]);

        let (_, v) = ask(&store, Endpoint::Speech, &format!("{q}&startRecord=4"));
        assert!(v["message"].as_str().unwrap().starts_with("(19004)"));
    }

    #[test]
    fn meeting_list_lists_all_speeches_of_matching_meetings() {
        let (_dir, store) = filled();
        let r = reply(
            &store,
            Endpoint::MeetingList,
            "speaker=見本花子&from=2026-10-20&until=2026-10-20",
            TODAY,
        );
        assert!(r.body.starts_with(
            "{\"numberOfRecords\":2,\"numberOfReturn\":2,\"startRecord\":1,\"nextRecordPosition\":null,\"meetingRecord\":[{\"issueID\":"
        ));
        let v: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(issue_ids(&v), [M1, M3]);
        assert_eq!(
            v["meetingRecord"][0],
            reparse(&ndl::list_record(&stored(&store, M1)))
        );
        assert_eq!(
            v["meetingRecord"][1],
            reparse(&ndl::list_record(&stored(&store, M3)))
        );
        assert_eq!(
            v["meetingRecord"][1]["speechRecord"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn meeting_returns_full_records_with_times() {
        let (_dir, store) = filled();
        let (status, v) = ask(&store, Endpoint::Meeting, RANGE);
        assert_eq!(status, 200);
        assert_eq!(v["numberOfRecords"], 4);
        assert_eq!(v["numberOfReturn"], 3);
        assert_eq!(v["nextRecordPosition"], 4);
        assert_eq!(issue_ids(&v), [M9, M1, "999900002X00220261020"]);
        assert_eq!(
            v["meetingRecord"][1],
            reparse(&ndl::meeting_record(&stored(&store, M1)))
        );
        assert_eq!(
            v["meetingRecord"][1]["speechRecord"][1]["updateTime"],
            "2026-11-01 08:00:00"
        );
    }

    #[test]
    fn any_is_marked_approximate() {
        let (_dir, store) = filled();
        let q = "from=2026-10-20&until=2026-10-20&any=";
        let (_, v) = ask(&store, Endpoint::Speech, &format!("{q}会議を開"));
        assert_eq!(speech_ids(&v), ["999900001X00120261020_001"]);
        assert_eq!(v["mirror"]["approximate"], json!(["order", "any"]));
        let (_, v) = ask(&store, Endpoint::Speech, &format!("{q}散会"));
        assert_eq!(speech_ids(&v), ["999900002X00220261020_001"]);
        let (_, v) = ask(&store, Endpoint::Speech, &format!("{q}本日%20散会"));
        assert_eq!(v["numberOfRecords"], 1);
        let (_, v) = ask(&store, Endpoint::Speech, &format!("{q}会議を開+散会"));
        assert_eq!(v["numberOfRecords"], 0);
        assert_eq!(v["mirror"]["approximate"], json!(["order", "any"]));
    }

    #[test]
    fn order_is_always_marked_approximate() {
        let (_dir, store) = filled();
        for ep in [Endpoint::Speech, Endpoint::MeetingList, Endpoint::Meeting] {
            let r = reply(&store, ep, RANGE, TODAY);
            assert!(r.body.ends_with(
                ",\"mirror\":{\"coverage\":{\"from\":\"2026-10-20\",\"until\":\"2026-10-21\"},\"approximate\":[\"order\"]}}"
            ), "{}", r.body);
        }
    }

    #[test]
    fn zero_hits_is_number_of_records_zero() {
        let (_dir, store) = filled();
        let r = reply(
            &store,
            Endpoint::Speech,
            &format!("speaker=存在しない&{RANGE}"),
            TODAY,
        );
        assert_eq!(r.status, 200);
        assert_eq!(
            r.body,
            "{\"numberOfRecords\":0,\"mirror\":{\"coverage\":{\"from\":\"2026-10-20\",\"until\":\"2026-10-21\"},\"approximate\":[\"order\"]}}"
        );
    }

    #[test]
    fn undated_query_is_a_coverage_error_until_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let empty = Store::open(&dir.path().join("e.sqlite")).unwrap();
        let (status, v) = ask(&empty, Endpoint::Speech, "speaker=架空太郎");
        assert_eq!(status, 400);
        assert_eq!(v["mirror"]["coverage"], Value::Null);
        assert_eq!(v["mirror"]["approximate"], json!(["order"]));
        assert!(v["details"].as_array().is_some());

        let (_dir, store) = filled();
        let (status, v) = ask(&store, Endpoint::Speech, "speaker=架空太郎");
        assert_eq!(status, 400);
        let text = v.to_string();
        assert!(
            text.contains("2026-10-20") && text.contains("1947-05-20"),
            "{text}"
        );
        assert_eq!(v["mirror"]["coverage"], coverage_note());

        store
            .conn()
            .execute_batch(
                "WITH RECURSIVE d(x) AS (SELECT '1947-05-20' UNION ALL
                     SELECT date(x, '+1 day') FROM d WHERE x < '2026-10-19')
                 INSERT INTO days(date, first_fetched, last_verified, meetings)
                     SELECT x, '2026-10-21T00:00:00Z', '2026-10-21T00:00:00Z', 0 FROM d",
            )
            .unwrap();
        let (status, v) = ask(&store, Endpoint::Speech, "speaker=架空太郎");
        assert_eq!(status, 200);
        assert_eq!(v["numberOfRecords"], 4);
        assert_eq!(v["mirror"]["coverage"]["from"], "1947-05-20");
    }

    #[test]
    fn issue_id_outside_coverage_is_a_coverage_error() {
        let (_dir, store) = filled();
        let (status, v) = ask(&store, Endpoint::Meeting, "issueID=100105254X00119470520");
        assert_eq!(status, 400);
        assert_eq!(v["mirror"]["coverage"], coverage_note());
        let (status, v) = ask(
            &store,
            Endpoint::Speech,
            "speechID=100105254X00119470520_000",
        );
        assert_eq!(status, 400);
        assert!(v.get("numberOfRecords").is_none());

        let (status, v) = ask(&store, Endpoint::Meeting, &format!("issueID={M1}"));
        assert_eq!(status, 200);
        assert_eq!(issue_ids(&v), [M1]);
        let (_, v) = ask(&store, Endpoint::Speech, &format!("speechID={M1}_002"));
        assert_eq!(speech_ids(&v), [format!("{M1}_002")]);
    }

    #[test]
    fn range_inside_coverage_is_answered() {
        let (_dir, store) = filled();
        let (status, v) = ask(&store, Endpoint::Speech, RANGE);
        assert_eq!(status, 200);
        assert_eq!(v["numberOfRecords"], 12);
        assert_eq!(
            &speech_ids(&v)[..2],
            [format!("{M9}_000"), format!("{M9}_001")]
        );
        assert_eq!(speech_ids(&v)[3], format!("{M1}_000"));

        let (status, v) = ask(&store, Endpoint::Speech, "from=2026-10-21&until=2026-12-31");
        assert_eq!(status, 200);
        assert_eq!(v["numberOfRecords"], 3);
        let (status, _) = ask(&store, Endpoint::Speech, "from=2026-10-19&until=2026-10-21");
        assert_eq!(status, 400);
        let r = reply(
            &store,
            Endpoint::Speech,
            "from=2026-10-20",
            Date::constant(2026, 11, 3),
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn structured_parameters_filter() {
        let (_dir, store) = filled();
        let n = |q: &str| {
            ask(&store, Endpoint::Speech, &format!("{q}&{RANGE}")).1["numberOfRecords"].clone()
        };
        assert_eq!(n("nameOfHouse=参議院"), 6);
        assert_eq!(n("nameOfHouse=衆議院"), 6);
        assert_eq!(n("nameOfHouse=両院"), 0);
        assert_eq!(n("nameOfHouse=どこか"), 12);
        assert_eq!(n("nameOfMeeting=本会議%20調査会"), 6);
        assert_eq!(n("speaker=見本%20会議録"), 8);
        assert_eq!(n("speakerPosition=大"), 2);
        assert_eq!(n("speakerGroup=架空"), 4);
        assert_eq!(n("speechNumber=3"), 1);
        assert_eq!(n("sessionFrom=999"), 12);
        assert_eq!(n("sessionTo=998"), 0);
        assert_eq!(n("issueFrom=2&issueTo=3"), 6);
        assert_eq!(n("issueTo=1"), 6);
        assert_eq!(n("speakerRole=証人"), 0);
    }

    #[test]
    fn unsupported_parameter_is_named() {
        let (_dir, store) = filled();
        let (status, v) = ask(
            &store,
            Endpoint::Speech,
            &format!("speaker=架空&supplementAndAppendix=false&foo=1&recordPacking=xml&{RANGE}"),
        );
        assert_eq!(status, 400);
        let details = v["details"].to_string();
        for name in ["supplementAndAppendix", "foo", "recordPacking=xml"] {
            assert!(details.contains(name), "{details}");
        }
        assert!(!details.contains("speaker"));
        assert_eq!(v["mirror"]["coverage"], coverage_note());
        let (status, _) = ask(
            &store,
            Endpoint::Speech,
            &format!("recordPacking=json&{RANGE}"),
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn hiragana_speaker_is_unsupported() {
        let (_dir, store) = filled();
        let (status, v) = ask(
            &store,
            Endpoint::Speech,
            &format!("speaker=架空%20かくうー&{RANGE}"),
        );
        assert_eq!(status, 400);
        assert!(v["details"].to_string().contains("speaker"));
        let (status, v) = ask(
            &store,
            Endpoint::MeetingList,
            &format!("nameOfMeeting=ほんかいぎ&{RANGE}"),
        );
        assert_eq!(status, 400);
        assert!(v["details"].to_string().contains("nameOfMeeting"));
        let (status, _) = ask(&store, Endpoint::Speech, &format!("speaker=カクウ&{RANGE}"));
        assert_eq!(status, 200);
        let (status, _) = ask(
            &store,
            Endpoint::Speech,
            &format!("speaker=見本はなこ&{RANGE}"),
        );
        assert_eq!(status, 200);
    }

    fn ndl_error(ep: Endpoint, q: &str) -> Value {
        let r = parse(ep, q).unwrap_err();
        assert_eq!(r.status, 200);
        assert!(r.body.starts_with("{\"message\":\"(190"), "{}", r.body);
        let v: Value = serde_json::from_str(&r.body).unwrap();
        assert!(v.get("mirror").is_none());
        v
    }

    #[test]
    fn maximum_records_over_limit_is_19005_or_19006() {
        let v = ndl_error(Endpoint::Speech, "any=x&maximumRecords=101");
        assert_eq!(
            v["message"],
            "(19005)maximumRecordsには1～100の値を指定してください。"
        );
        assert_eq!(
            v["details"],
            json!(["(19005)maximumRecordsには1～100の値を指定してください。"])
        );
        let v = ndl_error(Endpoint::MeetingList, "any=x&maximumRecords=0");
        assert!(v["message"].as_str().unwrap().starts_with("(19005)"));
        let v = ndl_error(Endpoint::Meeting, "any=x&maximumRecords=11");
        assert_eq!(
            v["message"],
            "(19006)maximumRecordsには1～10の値を指定してください。"
        );
        assert!(parse(Endpoint::Meeting, "any=x&maximumRecords=10").is_ok());
        assert!(parse(Endpoint::MeetingList, "any=x&maximumRecords=100").is_ok());
    }

    #[test]
    fn from_after_until_is_19014() {
        let v = ndl_error(Endpoint::Speech, "from=2026-10-21&until=2026-10-20");
        assert_eq!(
            v["message"],
            "(19014)fromはuntil以下、もしくはuntilはfrom以上の日付で入力してください。"
        );
    }

    #[test]
    fn other_input_errors_reuse_ndl_codes() {
        let code = |ep, q| ndl_error(ep, q)["message"].as_str().unwrap()[..7].to_string();
        assert_eq!(code(Endpoint::Speech, "maximumRecords=5"), "(19007)");
        assert_eq!(code(Endpoint::Speech, "any=x&startRecord=0"), "(19004)");
        assert_eq!(code(Endpoint::Speech, "speakerRole=委員長"), "(19011)");
        assert_eq!(
            code(Endpoint::Speech, "issueID=100105254x00119470520"),
            "(19011)"
        );
        assert_eq!(
            code(Endpoint::Speech, "speechID=100105254X00119470520_00"),
            "(19011)"
        );
        assert_eq!(code(Endpoint::Speech, "speechNumber=a"), "(19011)");
        assert_eq!(code(Endpoint::Speech, "sessionFrom=x"), "(19011)");
        assert_eq!(code(Endpoint::Speech, "from=2026/10/20"), "(19012)");
        assert_eq!(code(Endpoint::Speech, "until=20261020"), "(19013)");
        assert_eq!(code(Endpoint::Speech, "from=2026-02-30"), "(19018)");
        assert_eq!(code(Endpoint::Speech, "until=2026-13-01"), "(19019)");
        assert!(parse(Endpoint::Speech, "speechID=100105254X00119470520_0001").is_ok());
    }

    #[test]
    fn dropped_meetings_are_not_answered() {
        let (_dir, store) = filled();
        let mut again = day_meetings(&store, at(1));
        again.retain(|(m, _)| m.issue_id != M3);
        store.apply_day(DAY, &again, at(1)).unwrap();
        let q = format!("speaker=見本花子&{RANGE}");
        let (_, v) = ask(&store, Endpoint::MeetingList, &q);
        assert_eq!(issue_ids(&v), [M9, M1]);
        let (_, v) = ask(&store, Endpoint::Speech, &q);
        assert_eq!(v["numberOfRecords"], 2);
        let (_, v) = ask(&store, Endpoint::Meeting, &format!("issueID={M3}"));
        assert_eq!(v["numberOfRecords"], 0);
    }
}
