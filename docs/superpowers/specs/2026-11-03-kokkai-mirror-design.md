# kokkai-mirror design

Date: 2026-11-03. Repository: `Kohei-Wada/kokkai-mirror` (private for now; meant to be public later,
like kokkai-cli). Status: design.

## Why

kokkai-cli (`Kohei-Wada/kokkai-cli`) reads the NDL 国会会議録検索システム API. The NDL asks
clients to avoid concurrent requests and to wait "数秒程度" after each response before the next
(https://kokkai.ndl.go.jp/api.html), and may block heavy users without notice. So every analysis
that needs many speeches is slow (高市早苗's speeches alone are ~93 requests ≈ 8 minutes at 3 s), and
anything that compares many members is out of reach. There is no bulk download (NDL Lab lists the
会議録 text dataset as not provided).

kokkai-mirror is software that builds **your own local copy** of the 会議録 API by crawling it
slowly within the NDL's terms, keeps it up to date, and serves it back with the same API shape. Once
you have it, kokkai-cli and your own analyses can hit the mirror as hard as they like.

It is an experiment and a toy for exploring the Diet's record. The software is what gets shared;
**the data is not distributed**: each person runs the mirror and crawls for themselves.

## Goals

- Crawl the NDL 会議録 API (speeches and meetings) into a local store.
- **Order: newest first, then backwards in time.** New meetings are always fetched first (daily);
  remaining time walks back towards 1947. A fresh mirror is useful within hours (recent years) and
  deepens the longer it runs (all 11.25M speeches since 1947 in roughly a week at the NDL's pace).
  Stopping and restarting resumes where it left off.
- Serve the same endpoints and JSON as the NDL API (`/api/speech`, `/api/meeting_list`,
  `/api/meeting`, with the same query parameters and response shape), so an NDL API client only
  changes its base URL. No rate limit on the mirror itself.
  - For structured parameters (speaker, house, meeting name, session, dates, issue/speech IDs,
    paging) results match the NDL: same hits, `numberOfRecords`, order. *(my call)*
  - Full-text search (`any`) may be approximate (e.g. SQLite FTS); the response says so. *(my call)*
  - By default the mirror returns the current version of each record, as the NDL would now.
    Older versions and records the NDL has dropped are kept and available through mirror-only
    endpoints or parameters, never mixed into NDL-shaped answers. *(my call)*
- **Coverage is explicit.** The mirror knows and reports the date range it holds completely. A query
  that reaches outside that range returns an error that states the covered range. No partial
  answers. *(user)* A query without a date range means "all time", so until the backfill reaches
  1947 it gets that error too; clients ask within the covered range. *(my call, following the user's
  rule; kokkai-cli is not a consideration for now)*
- **The store is evidence.** Every fetched response is kept with its fetch time and content hash.
  If the NDL later returns different content for the same record, the old version is kept, not
  overwritten, and the change is recorded and reportable. *(user: to check whether the server side
  changed records behind our back)*
- **Re-verification.** Hashes only catch a change when a record is fetched again, so the mirror
  re-fetches what it already holds on a slow, continuous cycle and compares hashes: recent months
  often (the NDL publishes and corrects them for weeks), older ones rarely, so that over time every
  record gets re-checked. This shares the same NDL pace budget as new meetings and the backfill, with
  new meetings first. The mirror can report, for any record or range, when it was last verified and
  whether it ever changed. Scope: **all records, all time** (a full re-check cycle takes about as long
  as the full crawl). *(user; the split of the budget is my call, details left to the builder)*
- Easy to self-host: a single binary and a single data file.

## Non-goals

- Distributing the data, or a public service serving speech text. (Self-hosters serve themselves.)
- Mirroring other sources (bills, members, votes, e-Gov). kokkai-cli keeps getting those as now.
- Search features beyond what the NDL API offers. Analyses run against the store or the API.
- A web UI.

## Anti-goals

- Breaking the NDL's terms: concurrent requests, waiting less than the configured pause, retrying
  aggressively, or any mode that crawls faster. The pace is not user-tunable below the NDL's ask.
- Answering a query with data the mirror doesn't fully have, without saying so.
- Silently replacing a record with a changed version.

## Constraints

- **NDL pace:** one request at a time; wait 3 s after each response is fully received; honour 429/5xx
  with backoff and Retry-After. Max 100 records per `speech`/`meeting_list` request, 10 per `meeting`.
  A descriptive User-Agent with a reachable contact (the operator's choice; the mirror must make the
  self-hoster set one).
- **Ask the NDL first.** Before the first full crawl on thor, the user sends the NDL an inquiry
  (form on api.html) stating purpose, pace and duration. Not a code constraint, but the README tells
  every self-hoster to read the NDL terms and do the same for full crawls.
- **Copyright.** Speech text belongs to the speakers; the mirror is for the self-hoster's own use and
  analysis. The README says not to republish the text.
- Size: ~7.8 GB of text for everything (NDL Lab figure), plus metadata and versions.

## Decided

- Software shared, data not; each person self-hosts and crawls. *(user)*
- Newest first, then backwards. *(user)*
- Out-of-coverage queries are errors stating the covered range. *(user)*
- kokkai-cli is out of scope for now; kokkai-mirror stands on its own as an NDL-API-shaped mirror.
  *(user)*
- Rust, single binary; SQLite as the store. *(my call; user had no preference)*
- Same API shape as the NDL. *(my call, from the user's "kokkai-cli just points at it")*
- Keep every version; report changes. *(my call)*
- Name: kokkai-mirror. *(user)*

## Left to the builder

- Crawl strategy details: how to page backwards (by date windows on `meeting_list`/`speech`), how to
  detect "new" vs late-registered meetings (the NDL publishes days to weeks after a meeting), how to
  checkpoint, how to re-check recent months for corrections.
- Schema, versioning model, how the coverage range is computed and reported.
- How faithfully to reproduce the NDL API's query parameters (start with what kokkai-cli uses; list
  unsupported ones with an error).
- Process model (one binary with `crawl` and `serve`, or both in one), packaging (a NixOS module for
  thor is welcome; kokkai-mirror itself stays generic).
- Whether `/api/meeting` and `/api/meeting_list` can be built from crawled `speech` records or need
  their own crawl (fields differ); prefer one crawl if it reproduces both faithfully.
