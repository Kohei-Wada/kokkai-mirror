# kokkai-mirror

Mirrors the [国会会議録検索システム API](https://kokkai.ndl.go.jp/api.html) of the National Diet Library (NDL) into one SQLite file and serves `/api/speech`, `/api/meeting_list` and `/api/meeting` from it in the NDL's response shape, so a client written for the NDL can be pointed at the mirror. Every version of every meeting that the NDL has returned is kept, with its fetch time and SHA-256.

A single binary, a single data file. Each person crawls for themselves; the data is not distributed.

## Install

```sh
cargo install --git https://github.com/Kohei-Wada/kokkai-mirror
```

## Use

```sh
kokkai-mirror crawl --db <DB> --user-agent <EMAIL_OR_URL>
kokkai-mirror serve --db <DB> [--listen <ADDR>]
```

- `crawl` runs forever, one request at a time, and is the only writer. `--user-agent` is required and must contain an email address or a URL where the NDL can reach you; the request header is `kokkai-mirror/<version> (+https://github.com/Kohei-Wada/kokkai-mirror) <your contact>`. Only one crawler can run per database (a file lock); a second one refuses to start.
- `serve` is read-only and can run while `crawl` writes. `--listen` defaults to `127.0.0.1:8080`.

## Pace

The crawler waits 3 seconds before its first request and 3 seconds after each response is fully received, and retries network errors, 408, 429 and 5xx with backoff (five attempts per request, then a 10-minute pause). A `Retry-After` on 429 or 503 is honoured at any length, as seconds or as an HTTP date. There is no flag, environment variable or code path that makes it faster.

Order of work:

1. New days first: any missing day between today (JST) and the oldest crawled day, newest first.
2. Re-checks of recent days: days up to 30 days old daily, 31 to 180 days old weekly. The NDL registers and corrects records late (a meeting created three days after it was held, a whole meeting updated months later), so recent days are checked again and again.
3. Backfill towards 1947-05-20, the first meeting, alternating with re-checks of older days (a day older than 180 days is re-checked at most every 30 days).

A day that fails (an HTTP failure, an NDL error other than "busy", or an answer that is inconsistent across its pages) is put aside and retried after 1 hour, then 2, 4, ... up to 7 days, while the other work goes on; the backfill passes it, and coverage ends at it until it is crawled. This schedule is kept in memory and starts over when the crawler restarts.

The crawl unit is one meeting day (`/api/meeting` with `maximumRecords=10`), about one request per day of the calendar plus one per 10 meetings. The recent weeks are there within minutes; the full backfill takes on the order of days. The NDL Lab puts the text of everything at about 7.8 GB; the database file is larger, since it also holds every raw response page, every kept version and the full-text index.

Only response bodies the NDL returned with a success status are stored (including NDL error messages sent that way). HTTP error responses (4xx/5xx) are not stored.

## Before a full crawl

Read the NDL's terms at https://kokkai.ndl.go.jp/api.html. Before crawling everything, send the NDL an inquiry (the form linked from that page) stating your purpose, the pace above and how long it will run.

Copyright: the speech text belongs to the individual speakers, not to the NDL or to this project. The mirror is for your own use and analysis. Do not republish the text. This project distributes no speech text.

## Endpoints

NDL-shaped, from the mirror's data:

- `GET /api/speech`
- `GET /api/meeting_list`
- `GET /api/meeting`

Supported parameters: `startRecord`, `maximumRecords`, `nameOfHouse`, `nameOfMeeting`, `any`, `speaker`, `from`, `until`, `speechNumber`, `speakerPosition`, `speakerGroup`, `speakerRole`, `speechID`, `issueID`, `sessionFrom`, `sessionTo`, `issueFrom`, `issueTo`, `recordPacking=json`. Result sets and `numberOfRecords` follow the NDL. Only JSON is served; an omitted `recordPacking` answers JSON (the NDL defaults to XML). For `meeting_list` and `meeting`, a meeting is a hit if at least one of its speeches matches, and all its speeches are listed, as the NDL does.

Mirror-only, never mixed into the NDL-shaped answers:

- `GET /mirror/coverage`: `{"from", "until"}`, the range held completely (both `null` when nothing is held yet).
- `GET /mirror/record?issueID=<id>` (or `speechID=<id>`): the versions of the meeting (`sha256`, `first_seen`, `last_seen`, source `url`), `last_verified` and `dropped_at`. Add `&version=<sha256>` to get that stored version of the record. When the content flips back to an earlier version, that version's `first_seen`..`last_seen` span overlaps the others; `/mirror/changes` is the authoritative history.
- `GET /mirror/changes?from=YYYY-MM-DD&until=YYYY-MM-DD`: the days checked, the oldest and newest verification time, and the detected changes (`changed`, `dropped`, `returned`, ...) in that range.

A meeting the NDL stops returning stays in its versions, leaves the NDL-shaped answers, and shows up in `/mirror/changes`.

## Coverage and the `mirror` note

The mirror answers only for the range of consecutive days it holds completely, ending at the newest crawled day. A query reaching outside it (a query without dates counts as all time, so it is refused until the backfill reaches 1947) gets HTTP 400 with the range stated. Every `/api/*` answer and mirror-own `/api/*` error carries a top-level object (NDL input errors, below, are answered exactly as the NDL does, without it):

```json
"mirror": {"coverage": {"from": "YYYY-MM-DD", "until": "YYYY-MM-DD"}, "approximate": ["order"]}
```

NDL input errors (`(19007)` no condition, `(19011)` invalid parameter with indexed details, ...) are answered as the NDL does, HTTP 400. Mirror-own errors (outside coverage, unsupported parameter) are HTTP 400 with `message`, `details` and the `mirror` object.

## What is approximate

- `order`: the NDL sorts `speech` by date descending, house and meeting name, and `meeting_list` by meeting-name readings that no response contains. The mirror uses date descending, house, `issueID` (and `speechOrder` for speeches).
- `any`: SQLite FTS5 trigram search; terms under 3 characters use a substring scan. The NDL's matching may differ.
- `searchObject` in speech records follows the observed rule (equal to `speechOrder`).

## What is unsupported

Answered with HTTP 400 naming the parameter: `recordPacking` other than `json` (so `xml`), `supplementAndAppendix`, `contentsAndIndex`, `searchRange`, `closing`, any unknown parameter, and an all-hiragana term in `nameOfMeeting` or `speaker` (the NDL matches readings, which the mirror does not have).

## Checked against the NDL

The mirror's answers for one meeting were compared with the NDL's live `/api/speech` and `/api/meeting` answers (2026-11-03): byte-identical apart from the `mirror` object. That is one meeting, not a proof for every record.

## License

MIT
