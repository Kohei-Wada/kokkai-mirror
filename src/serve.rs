use std::net::ToSocketAddrs;

use jiff::civil::Date;
use jiff::Timestamp;
use serde::Serialize;
use serde_json::json;
use tiny_http::{Header, Response, Server};

use crate::crawl::jst_date;
use crate::query::{answer, decode, parse, Endpoint, Reply};
use crate::store::Store;

fn json_reply(status: u16, value: serde_json::Value) -> Reply {
    Reply {
        status,
        body: value.to_string(),
    }
}

fn message(status: u16, text: impl std::fmt::Display) -> Reply {
    json_reply(status, json!({ "message": text.to_string() }))
}

fn to_reply<T: Serialize>(value: &T) -> anyhow::Result<Reply> {
    Ok(Reply {
        status: 200,
        body: serde_json::to_string(value)?,
    })
}

fn param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(k)? == name).then(|| decode(v)).flatten()
    })
}

fn date_param(query: &str, name: &str) -> Result<Date, Reply> {
    param(query, name)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| message(400, format!("{name} is required as YYYY-MM-DD")))
}

fn record(store: &Store, query: &str) -> anyhow::Result<Reply> {
    let Some(id) = param(query, "issueID").or_else(|| param(query, "speechID")) else {
        return Ok(message(400, "issueID or speechID is required"));
    };
    let issue_id = id.split('_').next().unwrap_or_default();
    let Some(history) = store.record_history(issue_id)? else {
        return Ok(message(404, format!("no meeting {issue_id}")));
    };
    let Some(sha) = param(query, "version") else {
        return to_reply(&history);
    };
    match history.versions.iter().find(|v| v.sha256 == sha) {
        Some(v) => Ok(json_reply(200, store.version_record(v.id)?)),
        None => Ok(message(404, format!("no version {sha} of {issue_id}"))),
    }
}

fn mirror(store: &Store, path: &str, query: &str) -> anyhow::Result<Reply> {
    match path {
        "/mirror/coverage" => match store.coverage()? {
            Some((from, until)) => to_reply(&json!({ "from": from, "until": until })),
            None => to_reply(&json!({ "from": null, "until": null })),
        },
        "/mirror/record" => record(store, query),
        "/mirror/changes" => {
            let (from, until) = match (date_param(query, "from"), date_param(query, "until")) {
                (Ok(f), Ok(u)) => (f, u),
                (Err(r), _) | (_, Err(r)) => return Ok(r),
            };
            to_reply(&store.changes(from, until)?)
        }
        _ => Ok(message(404, "not found")),
    }
}

pub fn handle(store: &Store, url: &str, today: Date) -> Reply {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let result = match path {
        "/api/speech" => api(store, Endpoint::Speech, query, today),
        "/api/meeting_list" => api(store, Endpoint::MeetingList, query, today),
        "/api/meeting" => api(store, Endpoint::Meeting, query, today),
        p if p.starts_with("/mirror/") => mirror(store, p, query),
        _ => Ok(message(404, "not found")),
    };
    result.unwrap_or_else(|e| {
        eprintln!("error answering {url}: {e:#}");
        message(500, format!("{e:#}"))
    })
}

fn api(store: &Store, endpoint: Endpoint, query: &str, today: Date) -> anyhow::Result<Reply> {
    match parse(endpoint, query) {
        Ok(q) => answer(store, &q, today),
        Err(reply) => Ok(reply),
    }
}

pub fn respond(store: &Store, request: tiny_http::Request, today: Date) {
    let reply = handle(store, request.url(), today);
    let header = Header::from_bytes("Content-Type", "application/json; charset=utf-8")
        .expect("static header is valid");
    let response = Response::from_string(reply.body)
        .with_status_code(reply.status)
        .with_header(header);
    if let Err(e) = request.respond(response) {
        eprintln!("error sending response: {e}");
    }
}

pub fn serve(store: &Store, listen: impl ToSocketAddrs) -> anyhow::Result<()> {
    let server = Server::http(listen.to_socket_addrs()?.collect::<Vec<_>>().as_slice())
        .map_err(|e| anyhow::anyhow!("binding: {e}"))?;
    eprintln!("listening on http://{}", server.server_addr());
    for request in server.incoming_requests() {
        respond(store, request, jst_date(Timestamp::now()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndl::{day_url, parse_page};
    use serde_json::Value;

    const P1: &[u8] = include_bytes!("../tests/fixtures/meeting_day_p1.json");
    const P2: &[u8] = include_bytes!("../tests/fixtures/meeting_day_p2.json");
    const DAY: Date = Date::constant(2026, 10, 20);
    const M1: &str = "999900001X00120261020";

    fn at(day: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + day * 86_400).unwrap()
    }

    fn fetch_day(store: &Store, when: Timestamp, changed: bool) -> Vec<(crate::ndl::Meeting, i64)> {
        let mut out = Vec::new();
        for (page, start) in [(P1, 1), (P2, 11)] {
            let f = store
                .record_fetch(&day_url(DAY, start), when, page)
                .unwrap();
            for mut m in parse_page(page).unwrap().meetings {
                if changed && m.issue_id == M1 {
                    m.record["speechRecord"][0]["speech"] = json!("corrected");
                }
                out.push((m, f));
            }
        }
        out
    }

    fn filled() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("m.sqlite")).unwrap();
        let day = fetch_day(&store, at(0), false);
        store.apply_day(DAY, &day, at(0)).unwrap();
        let day = fetch_day(&store, at(1), true);
        store.apply_day(DAY, &day, at(1)).unwrap();
        (dir, store)
    }

    fn get(store: &Store, url: &str) -> (u16, Value) {
        let r = handle(store, url, DAY);
        (r.status, serde_json::from_str(&r.body).unwrap())
    }

    #[test]
    fn routes_api_endpoints() {
        let (_d, store) = filled();
        let q = "from=2026-10-20&until=2026-10-20&recordPacking=json";
        let (s, v) = get(&store, &format!("/api/meeting_list?{q}"));
        assert_eq!(s, 200);
        assert!(v["numberOfRecords"].as_i64().unwrap() > 0);
        let (s, v) = get(&store, &format!("/api/meeting?{q}"));
        assert_eq!(s, 200);
        assert!(v["meetingRecord"].is_array());
        let (s, v) = get(&store, &format!("/api/speech?{q}"));
        assert_eq!(s, 200);
        assert!(v["speechRecord"].is_array());
        let (s, v) = get(&store, "/api/speech?recordPacking=json");
        assert_eq!(s, 400);
        assert!(v["message"].is_string());
        assert_eq!(
            get(&store, "/mirror/coverage").1,
            json!({"from": "2026-10-20", "until": "2026-10-20"})
        );
    }

    #[test]
    fn coverage_of_an_empty_store_is_null() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("m.sqlite")).unwrap();
        let (s, v) = get(&store, "/mirror/coverage");
        assert_eq!((s, v), (200, json!({"from": null, "until": null})));
    }

    #[test]
    fn record_lists_versions_and_returns_an_old_one() {
        let (_d, store) = filled();
        let speech = format!("{M1}_001");
        for id in [format!("issueID={M1}"), format!("speechID={speech}")] {
            let (s, v) = get(&store, &format!("/mirror/record?{id}"));
            assert_eq!(s, 200);
            let versions = v["versions"].as_array().unwrap();
            assert_eq!(versions.len(), 2);
            assert!(v["last_verified"].is_string());
            assert!(v["dropped_at"].is_null());
            assert!(versions[0]["url"].is_string());
        }
        let (_, v) = get(&store, &format!("/mirror/record?issueID={M1}"));
        let old = v["versions"][0]["sha256"].as_str().unwrap();
        let new = v["versions"][1]["sha256"].as_str().unwrap();
        let (s, rec) = get(
            &store,
            &format!("/mirror/record?issueID={M1}&version={old}"),
        );
        assert_eq!(s, 200);
        assert_ne!(rec["speechRecord"][0]["speech"], json!("corrected"));
        let (_, rec) = get(
            &store,
            &format!("/mirror/record?issueID={M1}&version={new}"),
        );
        assert_eq!(rec["speechRecord"][0]["speech"], json!("corrected"));
        let (s, v) = get(&store, &format!("/mirror/record?issueID={M1}&version=nope"));
        assert_eq!(s, 404);
        assert!(v["message"].is_string());
        let (s, _) = get(&store, "/mirror/record?issueID=000000000X00020200101");
        assert_eq!(s, 404);
        let (s, _) = get(&store, "/mirror/record");
        assert_eq!(s, 400);
    }

    #[test]
    fn changes_reports_last_verified_for_a_range() {
        let (_d, store) = filled();
        let (s, v) = get(&store, "/mirror/changes?from=2026-10-20&until=2026-10-20");
        assert_eq!(s, 200);
        assert_eq!(v["days"], json!(1));
        assert!(v["newest_verified"].is_string());
        assert_eq!(v["changes"][0]["issue_id"], json!(M1));
        assert_eq!(v["changes"][0]["kind"], json!("changed"));
        for bad in [
            "/mirror/changes",
            "/mirror/changes?from=2026-10-20",
            "/mirror/changes?from=x&until=2026-10-20",
        ] {
            let (s, v) = get(&store, bad);
            assert_eq!(s, 400, "{bad}");
            assert!(v["message"].is_string());
        }
    }

    #[test]
    fn unknown_path_is_404() {
        let (_d, store) = filled();
        for p in ["/", "/api/other", "/mirror/nothing"] {
            let (s, v) = get(&store, p);
            assert_eq!(s, 404, "{p}");
            assert!(v["message"].is_string());
        }
    }

    #[test]
    fn serves_over_http() {
        let (dir, _store) = filled();
        let path = dir.path().join("m.sqlite");
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let t = std::thread::spawn(move || {
            let store = Store::open_read_only(&path).unwrap();
            respond(&store, server.recv().unwrap(), DAY);
        });
        let resp = ureq::get(&format!("http://127.0.0.1:{port}/mirror/coverage"))
            .call()
            .unwrap();
        assert_eq!(resp.content_type(), "application/json");
        assert_eq!(
            resp.header("Content-Type"),
            Some("application/json; charset=utf-8")
        );
        let v: Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        assert_eq!(v["from"], json!("2026-10-20"));
        t.join().unwrap();
    }
}
