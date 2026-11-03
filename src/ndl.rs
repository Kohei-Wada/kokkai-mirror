use jiff::civil::Date;
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const API: &str = "https://kokkai.ndl.go.jp/api";
pub const FLOOR: Date = Date::constant(1947, 5, 20);

const MEETING_KEYS: [&str; 12] = [
    "issueID",
    "imageKind",
    "searchObject",
    "session",
    "nameOfHouse",
    "nameOfMeeting",
    "issue",
    "date",
    "closing",
    "speechRecord",
    "meetingURL",
    "pdfURL",
];

const SPEECH_KEYS: [&str; 21] = [
    "speechID",
    "issueID",
    "imageKind",
    "searchObject",
    "session",
    "nameOfHouse",
    "nameOfMeeting",
    "issue",
    "date",
    "closing",
    "speechOrder",
    "speaker",
    "speakerYomi",
    "speakerGroup",
    "speakerPosition",
    "speakerRole",
    "speech",
    "startPage",
    "speechURL",
    "meetingURL",
    "pdfURL",
];

const MEETING_SPEECH_KEYS: [&str; 12] = [
    "speechID",
    "speechOrder",
    "speaker",
    "speakerYomi",
    "speakerGroup",
    "speakerPosition",
    "speakerRole",
    "speech",
    "startPage",
    "createTime",
    "updateTime",
    "speechURL",
];

const LIST_SPEECH_KEYS: [&str; 4] = ["speechID", "speechOrder", "speaker", "speechURL"];

#[derive(Debug, PartialEq)]
pub enum NdlError {
    Busy(String),
    Api(String),
    Parse(String),
}

impl std::fmt::Display for NdlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NdlError::Busy(m) => write!(f, "NDL busy: {m}"),
            NdlError::Api(m) => write!(f, "NDL error: {m}"),
            NdlError::Parse(m) => write!(f, "unexpected NDL response: {m}"),
        }
    }
}

impl std::error::Error for NdlError {}

pub struct Meeting {
    pub issue_id: String,
    pub date: Date,
    pub record: Value,
}

impl Meeting {
    pub fn sha256(&self) -> String {
        let bytes = serde_json::to_vec(&self.record).expect("a Value always serializes");
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

pub struct MeetingPage {
    pub total: u32,
    pub next: Option<u32>,
    pub meetings: Vec<Meeting>,
}

pub struct Ordered(pub Vec<(&'static str, Value)>);

struct SpeechEntry<'a>(&'a serde_json::Map<String, Value>);

impl Serialize for SpeechEntry<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut entries: Vec<_> = self.0.iter().collect();
        entries.sort_by_key(|(k, _)| MEETING_SPEECH_KEYS.iter().position(|x| x == k));
        let mut map = s.serialize_map(Some(entries.len()))?;
        for (k, v) in entries {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl Serialize for Ordered {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            match (*k, v) {
                ("speechRecord", Value::Array(items)) => {
                    let entries: Vec<_> = items
                        .iter()
                        .map(|i| match i {
                            Value::Object(o) => Ok(SpeechEntry(o)),
                            _ => Err(serde::ser::Error::custom(
                                "speechRecord entry not an object",
                            )),
                        })
                        .collect::<Result<_, _>>()?;
                    map.serialize_entry(k, &entries)?;
                }
                _ => map.serialize_entry(k, v)?,
            }
        }
        map.end()
    }
}

pub fn day_url(date: Date, start: u32) -> String {
    format!(
        "{API}/meeting?from={date}&until={date}&maximumRecords=10&recordPacking=json&startRecord={start}"
    )
}

pub fn parse_page(body: &[u8]) -> Result<MeetingPage, NdlError> {
    let v: Value = serde_json::from_slice(body).map_err(|e| NdlError::Parse(e.to_string()))?;
    if let Some(msg) = v.get("message").and_then(Value::as_str) {
        return Err(if msg.starts_with("(19001)") {
            NdlError::Busy(msg.to_string())
        } else {
            NdlError::Api(msg.to_string())
        });
    }
    let count = |key: &str| -> Result<Option<u32>, NdlError> {
        match v.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(n) => n
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| NdlError::Parse(format!("{key} is not a count"))),
        }
    };
    let total = count("numberOfRecords")?
        .ok_or_else(|| NdlError::Parse("numberOfRecords missing".into()))?;
    let next = count("nextRecordPosition")?;
    let mut meetings = Vec::new();
    if let Some(records) = v.get("meetingRecord") {
        let records = records
            .as_array()
            .ok_or_else(|| NdlError::Parse("meetingRecord is not an array".into()))?;
        for record in records {
            let text = |key: &str| {
                record
                    .get(key)
                    .and_then(Value::as_str)
                    .ok_or_else(|| NdlError::Parse(format!("meeting without {key}")))
            };
            let date = text("date")?
                .parse::<Date>()
                .map_err(|e| NdlError::Parse(format!("bad date: {e}")))?;
            meetings.push(Meeting {
                issue_id: text("issueID")?.to_string(),
                date,
                record: record.clone(),
            });
        }
    }
    Ok(MeetingPage {
        total,
        next,
        meetings,
    })
}

fn pick(obj: &Value, keys: &[&'static str]) -> Vec<(&'static str, Value)> {
    keys.iter()
        .map(|k| (*k, obj.get(*k).cloned().unwrap_or(Value::Null)))
        .collect()
}

fn speeches(meeting: &Value) -> &[Value] {
    meeting
        .get("speechRecord")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

pub fn speech_records(meeting: &Value) -> Vec<Ordered> {
    speeches(meeting)
        .iter()
        .map(|speech| {
            let order = speech.get("speechOrder").cloned().unwrap_or(Value::Null);
            Ordered(
                SPEECH_KEYS
                    .iter()
                    .map(|k| {
                        let v = match *k {
                            "searchObject" => order.clone(),
                            "speechID" | "speechOrder" | "speaker" | "speakerYomi"
                            | "speakerGroup" | "speakerPosition" | "speakerRole" | "speech"
                            | "startPage" | "speechURL" => {
                                speech.get(*k).cloned().unwrap_or(Value::Null)
                            }
                            _ => meeting.get(*k).cloned().unwrap_or(Value::Null),
                        };
                        (*k, v)
                    })
                    .collect(),
            )
        })
        .collect()
}

pub fn meeting_record(meeting: &Value) -> Ordered {
    Ordered(pick(meeting, &MEETING_KEYS))
}

pub fn list_record(meeting: &Value) -> Ordered {
    let mut fields = pick(meeting, &MEETING_KEYS);
    let reduced = speeches(meeting)
        .iter()
        .map(|s| {
            Value::Object(
                LIST_SPEECH_KEYS
                    .iter()
                    .map(|k| (k.to_string(), s.get(*k).cloned().unwrap_or(Value::Null)))
                    .collect(),
            )
        })
        .collect();
    for (k, v) in &mut fields {
        if *k == "speechRecord" {
            *v = Value::Array(reduced);
            break;
        }
    }
    Ordered(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const P1: &[u8] = include_bytes!("../tests/fixtures/meeting_day_p1.json");
    const P2: &[u8] = include_bytes!("../tests/fixtures/meeting_day_p2.json");
    const SPEECHES: &[u8] = include_bytes!("../tests/fixtures/speech_of_meeting.json");

    fn keys(o: &Ordered) -> Vec<&'static str> {
        o.0.iter().map(|(k, _)| *k).collect()
    }

    fn json_text(o: &Ordered) -> String {
        serde_json::to_string(o).unwrap()
    }

    #[test]
    fn builds_the_day_url() {
        let d = Date::constant(2026, 10, 20);
        assert_eq!(
            day_url(d, 11),
            "https://kokkai.ndl.go.jp/api/meeting?from=2026-10-20&until=2026-10-20&maximumRecords=10&recordPacking=json&startRecord=11"
        );
    }

    #[test]
    fn parses_a_day_page() {
        let p = parse_page(P1).unwrap();
        assert_eq!(p.total, 3);
        assert_eq!(p.next, Some(3));
        let ids: Vec<_> = p.meetings.iter().map(|m| m.issue_id.as_str()).collect();
        assert_eq!(ids, ["999900001X00120261020", "999900002X00220261020"]);
        assert!(p
            .meetings
            .iter()
            .all(|m| m.date == Date::constant(2026, 10, 20)));
    }

    #[test]
    fn last_page_has_null_next() {
        let p = parse_page(P2).unwrap();
        assert_eq!(p.total, 3);
        assert_eq!(p.next, None);
        assert_eq!(p.meetings.len(), 1);
    }

    #[test]
    fn zero_hits_is_an_empty_page() {
        let p = parse_page(br#"{"numberOfRecords":0}"#).unwrap();
        assert_eq!((p.total, p.next, p.meetings.len()), (0, None, 0));
    }

    #[test]
    fn meeting_record_keeps_ndl_key_order() {
        let p = parse_page(P1).unwrap();
        let o = meeting_record(&p.meetings[0].record);
        assert_eq!(keys(&o), MEETING_KEYS);
        let text = json_text(&o);
        let at = |k: &str| text.find(&format!("\"{k}\":")).unwrap();
        assert!(at("issueID") < at("imageKind") && at("closing") < at("speechRecord"));
        let Value::Array(speeches) = &o.0[9].1 else {
            panic!("speechRecord is not an array")
        };
        let first = speeches[0].as_object().unwrap();
        assert_eq!(first.len(), MEETING_SPEECH_KEYS.len());
        let line = serde_json::to_string(&Ordered(
            MEETING_SPEECH_KEYS
                .iter()
                .map(|k| (*k, first[*k].clone()))
                .collect(),
        ))
        .unwrap();
        assert!(text.contains(&line));
    }

    #[test]
    fn speech_records_match_the_speech_api() {
        let p = parse_page(P1).unwrap();
        let derived = speech_records(&p.meetings[0].record);
        let expected: Value = serde_json::from_slice(SPEECHES).unwrap();
        let expected = expected["speechRecord"].as_array().unwrap();
        assert_eq!(derived.len(), expected.len());
        for (d, e) in derived.iter().zip(expected) {
            assert_eq!(keys(d), SPEECH_KEYS);
            for (k, v) in &d.0 {
                assert_eq!(v, &e[*k], "field {k}");
            }
            assert_eq!(e.as_object().unwrap().len(), SPEECH_KEYS.len());
        }
        assert_eq!(derived[2].0[3].1, json!(2));
    }

    #[test]
    fn speech_records_serialize_in_ndl_order() {
        let p = parse_page(P1).unwrap();
        let derived = speech_records(&p.meetings[0].record);
        let text = json_text(&derived[0]);
        let expected: Value = serde_json::from_slice(SPEECHES).unwrap();
        let reparsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed, expected["speechRecord"][0]);
        assert!(text.starts_with("{\"speechID\":"));
        assert!(text.find("\"issueID\"").unwrap() < text.find("\"imageKind\"").unwrap());
        assert!(text.find("\"speech\":").unwrap() < text.find("\"speechURL\"").unwrap());
        assert!(text.ends_with("\"pdfURL\":null}"));
    }

    #[test]
    fn list_record_keeps_four_speech_fields() {
        let p = parse_page(P1).unwrap();
        let o = list_record(&p.meetings[0].record);
        assert_eq!(keys(&o), MEETING_KEYS);
        let Value::Array(speeches) = &o.0[9].1 else {
            panic!("speechRecord is not an array")
        };
        assert_eq!(speeches.len(), 3);
        for s in speeches {
            let got: Vec<_> = s.as_object().unwrap().keys().cloned().collect();
            let mut want: Vec<_> = LIST_SPEECH_KEYS.iter().map(|k| k.to_string()).collect();
            want.sort();
            assert_eq!(got, want);
        }
        let text = json_text(&o);
        assert!(text.contains(
            "{\"speechID\":\"999900001X00120261020_000\",\"speechOrder\":0,\"speaker\":"
        ));
    }

    #[test]
    fn hash_ignores_key_order_but_not_update_time() {
        let p = parse_page(P1).unwrap();
        let m = &p.meetings[0];
        let text = serde_json::to_string(&m.record).unwrap();
        let reversed: Value = {
            let Value::Object(o) = serde_json::from_str::<Value>(&text).unwrap() else {
                unreachable!()
            };
            Value::Object(o.into_iter().rev().collect())
        };
        let same = Meeting {
            issue_id: m.issue_id.clone(),
            date: m.date,
            record: reversed,
        };
        assert_eq!(m.sha256(), same.sha256());
        assert_eq!(m.sha256().len(), 64);

        let mut changed = m.record.clone();
        changed["speechRecord"][1]["updateTime"] = json!("2030-01-01 00:00:00");
        let other = Meeting {
            issue_id: m.issue_id.clone(),
            date: m.date,
            record: changed,
        };
        assert_ne!(m.sha256(), other.sha256());
    }

    #[test]
    fn busy_message_is_busy() {
        let body = r#"{"message":"(19001) 混雑しています","details":["x"]}"#;
        assert!(matches!(
            parse_page(body.as_bytes()),
            Err(NdlError::Busy(m)) if m.starts_with("(19001)")
        ));
    }

    #[test]
    fn other_message_is_an_api_error() {
        let body = r#"{"message":"(19004) bad","details":["x"]}"#;
        assert!(matches!(
            parse_page(body.as_bytes()),
            Err(NdlError::Api(m)) if m.starts_with("(19004)")
        ));
    }

    #[test]
    fn garbage_is_a_parse_error() {
        assert!(matches!(parse_page(b"<html>"), Err(NdlError::Parse(_))));
        assert!(matches!(
            parse_page(br#"{"numberOfRecords":1,"meetingRecord":[{"issueID":"x","date":"bad"}]}"#),
            Err(NdlError::Parse(_))
        ));
    }
}
