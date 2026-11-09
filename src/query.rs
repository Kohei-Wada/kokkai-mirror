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
        19014 => "fromはuntil以下、もしくはuntilはfrom以上の西暦年で入力してください。",
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

fn ndl_message_line(code: u32) -> String {
    format!("({code}){}", ndl_message(code))
}

fn ndl_errors(codes: &[u32]) -> Reply {
    let details: Vec<String> = codes
        .iter()
        .enumerate()
        .map(|(i, c)| format!("({i}) {}", ndl_message_line(*c)))
        .collect();
    Reply {
        status: 400,
        body: body(Obj(vec![
            ("message", Field::Json(json!(ndl_message_line(19011)))),
            ("details", Field::Json(json!(details))),
        ])),
    }
}

fn no_condition() -> Reply {
    Reply {
        status: 400,
        body: body(Obj(vec![(
            "message",
            Field::Json(json!(ndl_message_line(19007))),
        )])),
    }
}

pub(crate) fn decode(s: &str) -> Option<String> {
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

fn date_param(
    v: Option<&str>,
    format_code: u32,
    exists_code: u32,
    errs: &mut Vec<u32>,
) -> Option<Date> {
    let v = v?;
    let shaped = v.len() == 10
        && v.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if !shaped {
        errs.push(format_code);
        return None;
    }
    v.parse().map_err(|_| errs.push(exists_code)).ok()
}

fn number(v: Option<&str>, errs: &mut Vec<u32>) -> Option<i64> {
    let n = v?.parse::<i64>().ok().filter(|n| *n >= 0);
    if n.is_none() {
        errs.push(19011);
    }
    n
}

fn bounds(lo: Option<&str>, hi: Option<&str>, errs: &mut Vec<u32>) -> Option<(i64, i64)> {
    match (number(lo, errs), number(hi, errs)) {
        (Some(a), Some(b)) => Some((a, b)),
        (Some(a), None) | (None, Some(a)) => Some((a, a)),
        (None, None) => None,
    }
}

pub fn parse(endpoint: Endpoint, query: &str) -> Result<Query, Reply> {
    let mut params: Vec<(String, String)> = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match (decode(k), decode(v)) {
            (Some(k), Some(v)) => params.push((k, v)),
            _ => return Err(ndl_errors(&[19011])),
        }
    }
    let get = |name: &str| {
        params
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    if !CONDITIONS.iter().any(|c| get(c).is_some()) {
        return Err(no_condition());
    }

    let mut errs = Vec::new();
    let start = match get("startRecord") {
        None => 1,
        Some(v) => v.parse().ok().filter(|n| *n >= 1).unwrap_or_else(|| {
            errs.push(19004);
            1
        }),
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
            .unwrap_or_else(|| {
                errs.push(limit_code);
                1
            }),
    };
    let from = date_param(get("from"), 19012, 19018, &mut errs);
    let until = date_param(get("until"), 19013, 19019, &mut errs);
    if let (Some(f), Some(u)) = (from, until) {
        if f > u {
            errs.push(19014);
        }
    }
    let role = get("speakerRole").map(str::to_string);
    if role
        .as_deref()
        .is_some_and(|r| !["証人", "参考人", "公述人"].contains(&r))
    {
        errs.push(19011);
    }
    let issue_id = get("issueID").map(str::to_string);
    if issue_id.as_deref().is_some_and(|i| !valid_issue_id(i)) {
        errs.push(19011);
    }
    let speech_id = get("speechID").map(str::to_string);
    if speech_id.as_deref().is_some_and(|i| !valid_speech_id(i)) {
        errs.push(19011);
    }
    let speech_number = number(get("speechNumber"), &mut errs);
    let session = bounds(get("sessionFrom"), get("sessionTo"), &mut errs);
    let issue = bounds(get("issueFrom"), get("issueTo"), &mut errs);
    if !errs.is_empty() {
        return Err(ndl_errors(&errs));
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

    /// Substring match on the speaker like `any_of`, but scanning only the speaker index.
    fn speaker_any_of(&mut self, terms: &[String]) {
        if terms.is_empty() {
            return;
        }
        let ors = vec!["instr(speaker, ?) > 0"; terms.len()].join(" OR ");
        self.push(
            format!("s.speaker IN (SELECT speaker FROM speeches INDEXED BY speeches_speaker WHERE {ors})"),
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
    w.speaker_any_of(&q.speakers);
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
                "s.id IN (SELECT rowid FROM speech_fts WHERE speech_fts MATCH ?)",
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
    // One read transaction, so coverage, the count and the page come from one snapshot.
    let _snapshot = store.conn().unchecked_transaction()?;
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
        return Ok(ndl_errors(&[19004]));
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

        let r = reply(
            &store,
            Endpoint::Speech,
            &format!("{q}&startRecord=4"),
            TODAY,
        );
        assert_eq!(r.status, 400);
        assert_eq!(r.body, BEYOND_HITS);
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
    fn any_survives_a_vacuum_and_a_dump_restore() {
        let (_dir, store) = filled();
        // A dump restores rows by their columns, with the triggers created after the data.
        store
            .conn()
            .execute_batch(&format!(
                "DELETE FROM speeches WHERE issue_id IN ('{M1}', '999900002X00220261020');
                 VACUUM;
                 DROP TRIGGER speeches_ai; DROP TRIGGER speeches_ad; DROP TRIGGER speeches_au;
                 CREATE TEMP TABLE saved AS SELECT * FROM speeches;
                 DELETE FROM speeches;
                 INSERT INTO speeches SELECT * FROM saved;"
            ))
            .unwrap();
        let (_, v) = ask(&store, Endpoint::Speech, &format!("{RANGE}&any=会議を開"));
        assert_eq!(speech_ids(&v), ["999900009X00120261021_001"]);
        let (_, v) = ask(&store, Endpoint::Speech, &format!("{RANGE}&any=了解しま"));
        assert_eq!(speech_ids(&v), ["999900003X00320261020_002"]);
    }

    #[test]
    fn an_answer_is_one_snapshot() {
        let (dir, _store) = filled();
        let path = dir.path().join("m.sqlite");
        let writer = Store::open(&path).unwrap();
        let mut reader = Store::open_read_only(&path).unwrap();
        let mut n = 0;
        reader
            .conn_mut()
            .progress_handler(
                10,
                Some(move || {
                    n += 1;
                    writer
                        .conn()
                        .execute(
                            "INSERT INTO meetings SELECT ?1, date, session, house, name, issue_no,
                                 version_id, NULL, last_verified FROM meetings WHERE issue_id = ?2",
                            rusqlite::params![format!("w{n}"), M1],
                        )
                        .unwrap();
                    false
                }),
            )
            .unwrap();
        let (_, v) = ask(
            &reader,
            Endpoint::MeetingList,
            &format!("{RANGE}&maximumRecords=100"),
        );
        assert_eq!(v["numberOfRecords"], v["numberOfReturn"]);
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
    fn speaker_search_reads_the_speaker_index_not_the_speeches() {
        let (_dir, store) = filled();
        let q = parse(Endpoint::Speech, &format!("speaker=見本%20会議録&{RANGE}")).unwrap();
        let (m, s) = (meeting_where(&q, DAY, NEXT), speech_where(&q));
        let mut args = m.args;
        args.extend(s.args);
        let mut cond = m.sql;
        cond.extend(s.sql);
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT count(*) FROM speeches s JOIN meetings m ON m.issue_id = s.issue_id WHERE {}",
            cond.join(" AND ")
        );
        let plan: Vec<String> = store
            .conn()
            .prepare(&sql)
            .unwrap()
            .query_map(rusqlite::params_from_iter(&args), |r| r.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let plan = plan.join("\n");
        assert!(plan.contains("speeches_speaker"), "{plan}");
        assert!(
            !plan.contains("SCAN s\n") && !plan.ends_with("SCAN s"),
            "{plan}"
        );
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

    const BEYOND_HITS: &str = "{\"message\":\"(19011)検索条件の入力に誤りがあります。\",\"details\":[\"(0) (19004)startRecordには 1から検索件数までの値を指定してください。\"]}";

    fn ndl_error(ep: Endpoint, q: &str) -> Reply {
        let r = parse(ep, q).unwrap_err();
        assert_eq!(r.status, 400);
        assert!(!r.body.contains("\"mirror\""), "{}", r.body);
        r
    }

    fn detail_codes(ep: Endpoint, q: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(&ndl_error(ep, q).body).unwrap();
        assert_eq!(v["message"], "(19011)検索条件の入力に誤りがあります。");
        v["details"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap()[4..11].to_string())
            .collect()
    }

    #[test]
    fn maximum_records_over_limit_is_19005_or_19006() {
        assert_eq!(
            ndl_error(Endpoint::Speech, "any=x&maximumRecords=101").body,
            "{\"message\":\"(19011)検索条件の入力に誤りがあります。\",\"details\":[\"(0) (19005)maximumRecordsには1～100の値を指定してください。\"]}"
        );
        assert_eq!(
            detail_codes(Endpoint::MeetingList, "any=x&maximumRecords=0"),
            ["(19005)"]
        );
        assert_eq!(
            ndl_error(Endpoint::Meeting, "any=x&maximumRecords=11").body,
            "{\"message\":\"(19011)検索条件の入力に誤りがあります。\",\"details\":[\"(0) (19006)maximumRecordsには1～10の値を指定してください。\"]}"
        );
        assert!(parse(Endpoint::Meeting, "any=x&maximumRecords=10").is_ok());
        assert!(parse(Endpoint::MeetingList, "any=x&maximumRecords=100").is_ok());
    }

    #[test]
    fn from_after_until_is_19014() {
        assert_eq!(
            ndl_error(Endpoint::Speech, "from=2026-10-21&until=2026-10-20").body,
            "{\"message\":\"(19011)検索条件の入力に誤りがあります。\",\"details\":[\"(0) (19014)fromはuntil以下、もしくはuntilはfrom以上の西暦年で入力してください。\"]}"
        );
    }

    #[test]
    fn input_errors_are_collected() {
        assert_eq!(
            ndl_error(
                Endpoint::Speech,
                "maximumRecords=200&from=2026-10-10&until=2026-10-01"
            )
            .body,
            "{\"message\":\"(19011)検索条件の入力に誤りがあります。\",\"details\":[\"(0) (19005)maximumRecordsには1～100の値を指定してください。\",\"(1) (19014)fromはuntil以下、もしくはuntilはfrom以上の西暦年で入力してください。\"]}"
        );
    }

    #[test]
    fn no_condition_is_19007_without_details() {
        assert_eq!(
            ndl_error(Endpoint::Speech, "maximumRecords=5").body,
            "{\"message\":\"(19007)検索条件を指定してください。\"}"
        );
    }

    #[test]
    fn other_input_errors_reuse_ndl_codes() {
        let code = |q| detail_codes(Endpoint::Speech, q);
        assert_eq!(code("any=x&startRecord=0"), ["(19004)"]);
        assert_eq!(
            ndl_error(Endpoint::Speech, "any=x&startRecord=0").body,
            BEYOND_HITS
        );
        assert_eq!(code("speakerRole=委員長"), ["(19011)"]);
        assert_eq!(code("issueID=100105254x00119470520"), ["(19011)"]);
        assert_eq!(code("speechID=100105254X00119470520_00"), ["(19011)"]);
        assert_eq!(code("speechNumber=a"), ["(19011)"]);
        assert_eq!(code("sessionFrom=x"), ["(19011)"]);
        assert_eq!(code("from=2026/10/20"), ["(19012)"]);
        assert_eq!(code("until=20261020"), ["(19013)"]);
        assert_eq!(code("from=2026-02-30"), ["(19018)"]);
        assert_eq!(code("until=2026-13-01"), ["(19019)"]);
        assert_eq!(
            code("from=2026-02-30&until=x&speakerRole=a"),
            ["(19018)", "(19013)", "(19011)"]
        );
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
