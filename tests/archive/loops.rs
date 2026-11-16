//! The poll and backfill loops and the runner, against Postgres and a fake Instagram.
//!
//! Ported from `tests/test_ingest_loops.py`. The fake records its calls because what is
//! worth pinning is which calls the loops *choose* to make: a pass that opens every
//! thread stores the same rows as one that opens none, and only the call log differs.
//!
//! Payloads are built from `tests/fixtures/instagram/`, anonymised captures, so what the
//! loops normalize and store is the shape a live account sent.

use std::collections::VecDeque;
use std::sync::LazyLock;
use std::time::Duration;

use instagram_client::web::WebError;
use instagram_core::models::AccountRecord;
use instagram_core::store;
use instagram_ingest::backfill::{BACKFILL_RESERVE, backfill_once};
use instagram_ingest::config::IngestSettings;
use instagram_ingest::normalize;
use instagram_ingest::poll::{POLL_PAGES, PollTally, poll_once};
use instagram_ingest::runner::{self, Runner};
use instagram_ingest::source::Source;
use serde_json::{Map, Value, json};
use sqlx::postgres::PgPool;

use crate::support::db_test;

const ACCOUNT: &str = "acct";
/// 2026-08-14 in the milliseconds GraphQL counts in; `normalize` rejects any other unit.
const BASE_MS: i64 = 1_786_728_337_430;

struct Shapes {
    viewer: Value,
    users: Value,
    /// The envelopes with the per-call parts removed. The listing shell comes from the
    /// inbox query and the thread shell from the thread query, because the two differ.
    listing_shell: Map<String, Value>,
    thread_shell: Map<String, Value>,
    nodes: Vec<Value>,
}

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/instagram/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).expect("the fixture")).expect("JSON")
}

fn shell(thread: &Value) -> Map<String, Value> {
    let mut shell = thread.as_object().expect("a thread object").clone();
    shell.remove("slide_messages");
    shell.remove("users");
    shell
}

static SHAPES: LazyLock<Shapes> = LazyLock::new(|| {
    let thread =
        fixture("thread.graphql.json")["data"]["get_slide_thread_nullable"]["as_ig_direct_thread"]
            .take();
    let inbox = fixture("inbox.graphql.json")["data"]["get_slide_mailbox_for_iris_subscription"]
        ["threads_by_folder"]["edges"][0]["node"]["as_ig_direct_thread"]
        .take();
    Shapes {
        viewer: thread["viewer"].clone(),
        users: thread["users"].clone(),
        listing_shell: shell(&inbox),
        thread_shell: shell(&thread),
        nodes: thread["slide_messages"]["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .filter_map(|edge| edge.get("node").filter(|n| !n.is_null()).cloned())
            .collect(),
    }
});

fn users_count() -> i64 {
    SHAPES.users.as_array().expect("users").len() as i64
}

/// One captured message with a fresh id and clock. Higher `n` is newer. `id` moves with
/// `message_id`: they are equal on every captured message.
fn item(n: i64) -> Value {
    let mut raw = SHAPES.nodes[n as usize % SHAPES.nodes.len()].clone();
    raw["message_id"] = json!(format!("item-{n:04}"));
    raw["id"] = raw["message_id"].clone();
    // A string, as Instagram sends it.
    raw["timestamp_ms"] = json!((BASE_MS + n * 1_000).to_string());
    raw
}

/// `count` messages, newest first: the order Instagram sends a page in.
fn items(count: i64) -> Vec<Value> {
    (0..count).rev().map(item).collect()
}

fn ms(item: &Value) -> i64 {
    item["timestamp_ms"].as_str().unwrap().parse().unwrap()
}

/// One conversation: its history and its listing clock. It is keyed by `thread_id` and
/// addressed by a different `thread_fbid`, which catches a loop passing one for the other.
struct FakeThread {
    thread_id: String,
    fbid: String,
    items: Vec<Value>,
    last_activity_ms: i64,
}

impl FakeThread {
    fn new(thread_id: &str, items: Vec<Value>) -> Self {
        Self {
            thread_id: thread_id.to_owned(),
            fbid: format!(
                "99{}",
                thread_id.len() * 1000 + thread_id.bytes().map(usize::from).sum::<usize>()
            ),
            items,
            last_activity_ms: BASE_MS,
        }
    }

    fn at(mut self, last_activity_ms: i64) -> Self {
        self.last_activity_ms = last_activity_ms;
        self
    }

    /// The listing clock at the newest message, as a quiet thread's is.
    fn settled(self) -> Self {
        let newest = ms(&self.items[0]);
        self.at(newest)
    }

    /// A message arrives: onto the newest end, and the listing clock moves with it.
    fn receive(&mut self, item: Value) {
        self.last_activity_ms = ms(&item);
        self.items.insert(0, item);
    }

    /// Activity without a message: a reaction, an edit.
    fn stir(&mut self) {
        self.last_activity_ms += 1;
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Call {
    endpoint: &'static str,
    fbid: Option<String>,
    until: Option<String>,
    pages: usize,
    reserve: f64,
}

impl Call {
    fn bare(endpoint: &'static str) -> Self {
        Self {
            endpoint,
            fbid: None,
            until: None,
            pages: 0,
            reserve: 0.0,
        }
    }
}

struct Fake {
    threads: Vec<FakeThread>,
    page_size: usize,
    calls: Vec<Call>,
    /// Popped one per `inbox()` call; `None` means that pass succeeds.
    inbox_failures: VecDeque<Option<WebError>>,
    /// What `end_pass` hands back, as a rotated jar would.
    rotated: Option<String>,
}

fn fake(threads: Vec<FakeThread>, page_size: usize) -> Fake {
    Fake {
        threads,
        page_size,
        calls: Vec::new(),
        inbox_failures: VecDeque::new(),
        rotated: None,
    }
}

impl Fake {
    fn thread_mut(&mut self, thread_id: &str) -> &mut FakeThread {
        self.threads
            .iter_mut()
            .find(|t| t.thread_id == thread_id)
            .expect("a fake thread")
    }

    fn listing_entry(thread: &FakeThread) -> Value {
        let mut entry = SHAPES.listing_shell.clone();
        entry.insert("thread_id".into(), json!(thread.thread_id));
        entry.insert("users".into(), SHAPES.users.clone());
        entry.insert("thread_fbid".into(), json!(thread.fbid));
        // A string, as Instagram sends it: the poll sort has to cast it.
        entry.insert(
            "last_activity_timestamp_ms".into(),
            json!(thread.last_activity_ms.to_string()),
        );
        Value::Object(entry)
    }

    fn listing(&self) -> Vec<Value> {
        self.threads.iter().map(Self::listing_entry).collect()
    }

    fn page(thread: &FakeThread, offset: usize, page: &[Value]) -> Value {
        // Whether anything was left behind, which the reap pass reads to tell "I saw the
        // end of the window" from "I only saw the top of it".
        let more = offset + page.len() < thread.items.len();
        let mut out = SHAPES.thread_shell.clone();
        out.insert("thread_id".into(), json!(thread.thread_id));
        out.insert("thread_fbid".into(), json!(thread.fbid));
        out.insert("users".into(), SHAPES.users.clone());
        out.insert(
            "slide_messages".into(),
            json!({
                "edges": page.iter().enumerate()
                    .map(|(i, node)| json!({"cursor": format!("c{}", offset + i), "node": node}))
                    .collect::<Vec<_>>(),
                "page_info": {
                    "has_next_page": more,
                    "end_cursor": more.then(|| format!("c{}", offset + page.len())),
                    "has_previous_page": offset > 0,
                    "start_cursor": format!("c{offset}"),
                },
            }),
        );
        Value::Object(out)
    }

    fn calls_to(&self, endpoint: &str) -> Vec<&Call> {
        self.calls
            .iter()
            .filter(|c| c.endpoint == endpoint)
            .collect()
    }

    /// Which conversations were opened, by the id the archive keys on.
    fn opened(&self) -> Vec<String> {
        self.calls_to("thread")
            .iter()
            .map(|call| {
                let fbid = call.fbid.as_deref();
                let thread = self.threads.iter().find(|t| Some(t.fbid.as_str()) == fbid);
                thread.expect("an fbid the fake issued").thread_id.clone()
            })
            .collect()
    }
}

impl Source for Fake {
    async fn me(&mut self) -> Result<Value, WebError> {
        self.calls.push(Call::bare("me"));
        Ok(SHAPES.viewer.clone())
    }

    async fn inbox(&mut self) -> Result<Vec<Value>, WebError> {
        self.calls.push(Call::bare("inbox"));
        if let Some(Some(failure)) = self.inbox_failures.pop_front() {
            return Err(failure);
        }
        Ok(self.listing())
    }

    async fn inbox_head(&mut self) -> Result<Vec<Value>, WebError> {
        self.calls.push(Call::bare("inbox_head"));
        Ok(self.listing())
    }

    async fn thread(
        &mut self,
        thread_fbid: &str,
        until: Option<&str>,
        pages: usize,
        reserve: f64,
    ) -> Result<(Vec<Value>, bool), WebError> {
        self.calls.push(Call {
            endpoint: "thread",
            fbid: Some(thread_fbid.to_owned()),
            until: until.map(str::to_owned),
            pages,
            reserve,
        });
        let thread = self
            .threads
            .iter()
            .find(|t| t.fbid == thread_fbid)
            .expect("an fbid the fake issued");
        // The newest page always, then at most `pages` older ones.
        let mut payloads = Vec::new();
        let mut reached = false;
        for index in 0..=pages {
            let offset = index * self.page_size;
            let page = &thread.items
                [offset.min(thread.items.len())..(offset + self.page_size).min(thread.items.len())];
            if page.is_empty() && index > 0 {
                break;
            }
            payloads.push(Self::page(thread, offset, page));
            if until.is_some_and(|until| page.iter().any(|node| node["message_id"] == until)) {
                reached = true;
                break;
            }
        }
        Ok((payloads, reached))
    }

    fn end_pass(&mut self) -> Option<String> {
        self.calls.push(Call::bare("end_pass"));
        self.rotated.take()
    }
}

/// The parent rows the loops need but do not create: the account is the runner's job,
/// and backfill only visits threads a poll pass already listed.
async fn seed(fake: &Fake, pool: &PgPool) {
    store::upsert_account(
        pool,
        &AccountRecord {
            id: ACCOUNT.to_owned(),
            ig_user_id: "900000001".to_owned(),
            username: Some("me".to_owned()),
        },
    )
    .await
    .unwrap();
    for entry in fake.listing() {
        store::upsert_thread(
            pool,
            &normalize::thread_from_payload(&entry, ACCOUNT).unwrap(),
        )
        .await
        .unwrap();
    }
}

async fn stored_ids(pool: &PgPool, thread_id: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT item_id FROM message WHERE account_id = $1 AND thread_id = $2 ORDER BY ts",
    )
    .bind(ACCOUNT)
    .bind(thread_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    let sql = match table {
        "thread" => "SELECT count(*) FROM thread WHERE account_id = $1",
        "message" => "SELECT count(*) FROM message WHERE account_id = $1",
        _ => "SELECT count(*) FROM participant WHERE account_id = $1",
    };
    sqlx::query_scalar(sql)
        .bind(ACCOUNT)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn watermark(pool: &PgPool, thread_id: &str) -> Option<String> {
    store::get_thread(pool, ACCOUNT, thread_id)
        .await
        .unwrap()
        .expect("the thread")
        .watermark_item_id
}

async fn poll(pool: &PgPool, fake: &mut Fake, budget: usize, sweep_days: i64) -> PollTally {
    poll_once(pool, fake, ACCOUNT, budget, sweep_days, false)
        .await
        .unwrap()
}

// ------------------------------------------------------------------------- poll loop

db_test!(
    a_first_pass_stores_the_threads_the_people_and_the_messages,
    |f| {
        let mut fake = fake(
            vec![
                FakeThread::new("T1", items(3)),
                FakeThread::new("T2", items(2)),
            ],
            4,
        );
        seed(&fake, &f.pool).await;

        let tally = poll(&f.pool, &mut fake, 20, 0).await;

        assert_eq!(
            tally,
            PollTally {
                threads_seen: 2,
                threads_opened: 2,
                messages: 5,
                revisions: 0,
                deletions: 0
            }
        );
        assert_eq!(count(&f.pool, "thread").await, 2);
        assert_eq!(count(&f.pool, "message").await, 5);
        assert_eq!(count(&f.pool, "participant").await, users_count());
        // The newest item on the page, which is the newest the archive now holds.
        assert_eq!(watermark(&f.pool, "T1").await.as_deref(), Some("item-0002"));
    }
);

db_test!(a_thread_that_has_not_moved_is_never_opened, |f| {
    // The whole economy of the loop, and a regression is invisible in the database.
    let mut fake = fake(
        vec![
            FakeThread::new("T1", items(3)).settled(),
            FakeThread::new("T2", items(2)).settled(),
        ],
        4,
    );
    seed(&fake, &f.pool).await;
    poll(&f.pool, &mut fake, 20, 0).await;

    fake.thread_mut("T2").receive(item(50));
    fake.calls.clear();
    let tally = poll(&f.pool, &mut fake, 20, 0).await;

    assert_eq!(fake.opened(), ["T2"]);
    assert_eq!(
        tally,
        PollTally {
            threads_seen: 2,
            threads_opened: 1,
            messages: 1,
            revisions: 0,
            deletions: 0
        }
    );
});

db_test!(a_reopened_thread_counts_only_what_is_genuinely_new, |f| {
    // A reaction moves the clock without a message: the pass opens the thread, reports
    // nothing new, and the watermark stays where it was.
    let mut fake = fake(vec![FakeThread::new("T1", items(3)).settled()], 4);
    seed(&fake, &f.pool).await;
    poll(&f.pool, &mut fake, 20, 0).await;
    let before = watermark(&f.pool, "T1").await;

    fake.thread_mut("T1").stir();
    fake.calls.clear();
    let tally = poll(&f.pool, &mut fake, 20, 0).await;

    assert_eq!(fake.opened(), ["T1"]);
    assert_eq!(tally.messages, 0);
    assert_eq!(
        stored_ids(&f.pool, "T1").await,
        ["item-0000", "item-0001", "item-0002"]
    );
    assert_eq!(watermark(&f.pool, "T1").await, before);
});

db_test!(
    a_truncated_page_never_marks_what_it_did_not_reach_as_deleted,
    |f| {
        // The deletion pass may only judge the range it read. The payload's own
        // `has_next_page` is the only honest answer to "was this the whole window". The
        // sweep window is absurdly wide on purpose: the fixture clock is a fixed date.
        let mut fake = fake(vec![FakeThread::new("T1", items(9))], 9);
        seed(&fake, &f.pool).await;
        poll(&f.pool, &mut fake, 20, 36_500).await;
        assert_eq!(stored_ids(&f.pool, "T1").await.len(), 9);

        // Same history, now one short page at a time, and the thread has moved.
        fake.page_size = 4;
        let newest = ms(&fake.thread_mut("T1").items[0]);
        fake.thread_mut("T1").last_activity_ms = newest + 1;
        let tally = poll(&f.pool, &mut fake, 20, 36_500).await;

        assert_eq!(tally.deletions, 0);
        assert_eq!(stored_ids(&f.pool, "T1").await.len(), 9);
        let touched: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM message WHERE account_id = $1 AND thread_id = 'T1' \
         AND (missing_since IS NOT NULL OR deleted_at IS NOT NULL)",
        )
        .bind(ACCOUNT)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(touched, 0);
    }
);

db_test!(
    the_thread_budget_opens_the_most_recently_active_and_lists_the_rest,
    |f| {
        // A budget that runs out drops the threads least likely to hold anything new, and
        // listing them all anyway keeps the next pass able to see them move.
        let threads = (0..4)
            .map(|n| FakeThread::new(&format!("T{n}"), items(1)).at(BASE_MS + n))
            .collect();
        let mut fake = fake(threads, 4);
        seed(&fake, &f.pool).await;

        let tally = poll(&f.pool, &mut fake, 2, 0).await;

        assert_eq!(fake.opened(), ["T3", "T2"]);
        assert_eq!(tally.threads_seen, 4);
        assert_eq!(tally.threads_opened, 2);
        assert_eq!(count(&f.pool, "thread").await, 4);
    }
);

db_test!(
    a_thread_holding_nothing_is_opened_whatever_its_clock_says,
    |f| {
        let mut fake = fake(
            vec![FakeThread::new("T1", items(2)).at(BASE_MS - 10_000_000)],
            4,
        );
        seed(&fake, &f.pool).await;

        let tally = poll(&f.pool, &mut fake, 20, 0).await;

        assert_eq!(fake.opened(), ["T1"]);
        assert_eq!(tally.messages, 2);
    }
);

db_test!(
    a_burst_longer_than_a_page_is_paged_back_to_the_watermark,
    |f| {
        // Not in the Python suite, where the poll read its watermark off the listing (which
        // never carries one), so it always asked for the newest page alone. Ten messages
        // arriving between passes need three pages of four; taking one would leave a hole
        // below it that nothing comes back for, because backfill only extends the oldest end.
        let mut fake = fake(vec![FakeThread::new("T1", items(3))], 4);
        seed(&fake, &f.pool).await;
        poll(&f.pool, &mut fake, 20, 0).await;
        assert_eq!(fake.calls_to("thread")[0].pages, 0, "no known ground yet");

        for n in 3..13 {
            fake.thread_mut("T1").receive(item(n));
        }
        fake.calls.clear();
        let tally = poll(&f.pool, &mut fake, 20, 0).await;

        let call = fake.calls_to("thread")[0].clone();
        assert_eq!(call.until.as_deref(), Some("item-0002"));
        assert_eq!(call.pages, POLL_PAGES);
        assert_eq!(tally.messages, 10);
        assert_eq!(stored_ids(&f.pool, "T1").await.len(), 13);
        assert_eq!(watermark(&f.pool, "T1").await.as_deref(), Some("item-0012"));
    }
);

// --------------------------------------------------------------------- backfill loop

db_test!(backfill_pages_backwards_until_nothing_older_is_left, |f| {
    // Termination is a page saying nothing older is behind it, not a short page.
    let mut fake = fake(vec![FakeThread::new("T1", items(9))], 4);
    seed(&fake, &f.pool).await;

    let tally = backfill_once(&f.pool, &mut fake, ACCOUNT, 20)
        .await
        .unwrap();

    // Three pages: 4, 4, then the last 1, which reports nothing behind it.
    assert_eq!(
        (tally.requests, tally.messages, tally.threads_finished),
        (3, 9, 1)
    );
    assert_eq!(stored_ids(&f.pool, "T1").await.len(), 9);
    let thread = store::get_thread(&f.pool, ACCOUNT, "T1")
        .await
        .unwrap()
        .unwrap();
    assert!(thread.backfill_done);
    assert_eq!(thread.backfill_cursor, None);
});

db_test!(
    the_budget_stops_the_pass_and_the_next_one_goes_deeper,
    |f| {
        // A pass that stopped early re-walks what it read before reaching new ground, so it
        // has to ask for more depth than last time or the thread never finishes. The
        // re-walk stores nothing: `insert_messages` counts only rows that were new.
        let mut fake = fake(vec![FakeThread::new("T1", items(20))], 4);
        seed(&fake, &f.pool).await;

        let tally = backfill_once(&f.pool, &mut fake, ACCOUNT, 2).await.unwrap();

        assert_eq!(tally.requests, 3); // the newest page, plus the two the budget bought
        assert_eq!(stored_ids(&f.pool, "T1").await.len(), 12);
        let thread = store::get_thread(&f.pool, ACCOUNT, "T1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(thread.backfill_cursor.as_deref(), Some("3"));
        assert!(!thread.backfill_done);

        fake.calls.clear();
        let second = backfill_once(&f.pool, &mut fake, ACCOUNT, 2).await.unwrap();

        assert_eq!(fake.calls_to("thread")[0].pages, 5);
        assert_eq!(second.messages, 8);
        assert_eq!(stored_ids(&f.pool, "T1").await.len(), 20);
        let thread = store::get_thread(&f.pool, ACCOUNT, "T1")
            .await
            .unwrap()
            .unwrap();
        assert!(thread.backfill_done);
    }
);

db_test!(a_finished_thread_is_never_asked_for_again, |f| {
    let mut fake = fake(vec![FakeThread::new("T1", items(9))], 4);
    seed(&fake, &f.pool).await;
    store::set_backfill(&f.pool, ACCOUNT, "T1", None, true)
        .await
        .unwrap();

    let tally = backfill_once(&f.pool, &mut fake, ACCOUNT, 20)
        .await
        .unwrap();

    assert!(fake.calls_to("thread").is_empty());
    assert_eq!(
        (tally.requests, tally.messages, tally.threads_finished),
        (0, 0, 0)
    );
});

db_test!(a_budget_of_zero_issues_no_requests_at_all, |f| {
    // The daily ceiling arrives as a budget of zero: no traffic, not one polite page.
    let mut fake = fake(vec![FakeThread::new("T1", items(9))], 4);
    seed(&fake, &f.pool).await;

    for budget in [0, -1] {
        let tally = backfill_once(&f.pool, &mut fake, ACCOUNT, budget)
            .await
            .unwrap();
        assert_eq!(
            (tally.requests, tally.messages, tally.threads_finished),
            (0, 0, 0)
        );
    }
    assert!(fake.calls.is_empty());
});

db_test!(every_backfill_request_yields_the_bucket_to_poll, |f| {
    // The reserve is what makes the priority between the loops real, and it lives only
    // in the call.
    let mut fake = fake(
        vec![FakeThread::new("T1", items(9)).at(BASE_MS + 100_000)],
        4,
    );
    seed(&fake, &f.pool).await;

    backfill_once(&f.pool, &mut fake, ACCOUNT, 20)
        .await
        .unwrap();
    let reserves: Vec<f64> = fake.calls_to("thread").iter().map(|c| c.reserve).collect();
    assert!(!reserves.is_empty());
    assert!(reserves.iter().all(|r| *r == BACKFILL_RESERVE));
    const { assert!(BACKFILL_RESERVE > 0.0) };

    fake.calls.clear();
    poll(&f.pool, &mut fake, 20, 0).await;
    let reserves: Vec<f64> = fake.calls_to("thread").iter().map(|c| c.reserve).collect();
    assert_eq!(reserves, [0.0]);
});

// ---------------------------------------------------------------------------- runner

fn settings(extra: &[(&str, &str)]) -> IngestSettings {
    let mut env: Vec<(String, String)> = [
        ("INSTAGRAM_ACCOUNT", ACCOUNT),
        ("INSTAGRAM_DSN", "postgresql://u:p@localhost/instagram"),
        ("INSTAGRAM_BACKFILL_ENABLED", "false"),
        // No jitter, so the wait a pass returns is exact.
        ("INSTAGRAM_POLL_JITTER", "0"),
        ("INSTAGRAM_POLL_INTERVAL", "900"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
    env.extend(
        extra
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    IngestSettings::from_lookup(|name| {
        env.iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    })
    .unwrap()
}

async fn sync_error(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT last_error FROM sync_state WHERE account_id = $1 AND loop = 'poll'")
        .bind(ACCOUNT)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn auth_error(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT last_auth_error FROM account WHERE id = $1")
        .bind(ACCOUNT)
        .fetch_one(pool)
        .await
        .unwrap()
}

db_test!(
    a_dead_session_is_recorded_and_tried_again_later_not_fatal,
    |f| {
        // Python returned 2 here, and docker's `unless-stopped` restarted straight into the
        // same rejection (2026-10-07). Now the error is recorded for a human, the pass waits
        // INSTAGRAM_AUTH_RETRY, and a later pass that works lifts the flag.
        let mut fake = fake(vec![FakeThread::new("T1", items(2))], 4);
        fake.inbox_failures
            .push_back(Some(WebError::LoggedOut("login_required".into())));
        let settings = settings(&[]);
        let mut runner = Runner::default();

        let wait = runner.pass(&f.pool, &mut fake, &settings).await;

        assert_eq!(wait, Duration::from_secs(21_600));
        assert_eq!(fake.calls_to("inbox").len(), 1);
        let error = auth_error(&f.pool)
            .await
            .expect("the rejection is recorded");
        assert!(error.contains("login_required"), "{error}");

        let wait = runner.pass(&f.pool, &mut fake, &settings).await;
        assert_eq!(wait, Duration::from_secs(900));
        assert_eq!(auth_error(&f.pool).await, None);
        assert_eq!(fake.calls_to("me").len(), 1, "who we are is asked once");
    }
);

db_test!(one_bad_pass_is_recorded_and_the_loop_keeps_going, |f| {
    // A transport hiccup must not end the process.
    let mut fake = fake(vec![FakeThread::new("T1", items(2))], 4);
    fake.inbox_failures
        .push_back(Some(WebError::Other("boom".into())));
    let settings = settings(&[]);
    let mut runner = Runner::default();

    let wait = runner.pass(&f.pool, &mut fake, &settings).await;
    assert_eq!(wait, Duration::from_secs(900));
    let error = sync_error(&f.pool).await.expect("the failure is recorded");
    assert!(error.contains("boom"), "{error}");

    runner.pass(&f.pool, &mut fake, &settings).await;
    assert_eq!(fake.calls_to("inbox").len(), 2);
    assert_eq!(sync_error(&f.pool).await, None);
});

db_test!(a_rate_limited_pass_waits_twice_as_long, |f| {
    let mut fake = fake(vec![FakeThread::new("T1", items(2))], 4);
    fake.inbox_failures
        .push_back(Some(WebError::RateLimited("HTTP 429".into())));
    let settings = settings(&[]);
    let mut runner = Runner::default();

    let wait = runner.pass(&f.pool, &mut fake, &settings).await;

    assert_eq!(wait, Duration::from_secs(1_800));
    let error = sync_error(&f.pool).await.expect("the failure is recorded");
    assert!(error.contains("429"), "{error}");
});

db_test!(a_quick_pass_reads_the_inbox_head_and_never_backfills, |f| {
    // What the live socket triggers: the threads that moved, at once, and nothing that
    // spends the backfill's budget.
    let mut fake = fake(
        vec![FakeThread::new("T1", items(9)).at(BASE_MS + 100_000)],
        4,
    );
    seed(&fake, &f.pool).await;
    let settings = settings(&[("INSTAGRAM_BACKFILL_ENABLED", "true")]);
    let mut runner = Runner::default();

    assert_eq!(
        runner.quick_pass(&f.pool, &mut fake, &settings).await,
        Ok(())
    );
    assert_eq!(fake.calls_to("inbox_head").len(), 1);
    assert!(fake.calls_to("inbox").is_empty());
    let reserves: Vec<f64> = fake.calls_to("thread").iter().map(|c| c.reserve).collect();
    assert_eq!(
        reserves,
        [0.0],
        "the moved thread is opened, and nothing backfills"
    );

    // The same state on a full pass does backfill, so the quick pass really skipped it.
    fake.calls.clear();
    runner.pass(&f.pool, &mut fake, &settings).await;
    assert!(
        fake.calls_to("thread")
            .iter()
            .any(|c| c.reserve == BACKFILL_RESERVE)
    );
});

db_test!(the_daily_backfill_ceiling_survives_a_restart, |f| {
    // The count lived in the process, so a redeploy handed out a fresh day (2026-11-16).
    let mut fake = fake(
        vec![FakeThread::new("T1", items(40)).at(BASE_MS + 100_000)],
        4,
    );
    seed(&fake, &f.pool).await;
    let settings = settings(&[
        ("INSTAGRAM_BACKFILL_ENABLED", "true"),
        ("INSTAGRAM_BACKFILL_DAILY_REQUESTS", "2"),
    ]);
    let backfilled = |fake: &Fake| {
        fake.calls_to("thread")
            .iter()
            .filter(|c| c.reserve == BACKFILL_RESERVE)
            .count()
    };

    Runner::default().pass(&f.pool, &mut fake, &settings).await;
    assert!(backfilled(&fake) > 0, "the first process backfills");

    fake.calls.clear();
    Runner::default().pass(&f.pool, &mut fake, &settings).await;
    assert_eq!(
        backfilled(&fake),
        0,
        "a restarted process finds the day spent"
    );
});

db_test!(a_rotated_jar_is_saved_after_the_pass, |f| {
    let mut fake = fake(vec![FakeThread::new("T1", items(2))], 4);
    let settings = settings(&[]);
    let mut runner = Runner::default();

    runner.pass(&f.pool, &mut fake, &settings).await;
    assert_eq!(store::load_session(&f.pool, ACCOUNT).await.unwrap(), None);

    fake.rotated = Some(jar("rotated"));
    runner.pass(&f.pool, &mut fake, &settings).await;
    let saved = store::load_session(&f.pool, ACCOUNT)
        .await
        .unwrap()
        .unwrap();
    assert!(saved.contains("rotated"), "{saved}");
    assert_eq!(fake.calls_to("end_pass").len(), 2);
});

// ------------------------------------------------------------------------ session jar

fn jar(session: &str) -> String {
    format!(
        r#"[{{"name":"sessionid","value":"{session}","domain":".instagram.com","path":"/","expires":-1,"secure":true,"httpOnly":true}}]"#
    )
}

fn export(name: &str, session: &str) -> String {
    let path = std::env::temp_dir().join(format!("{name}-{}.json", std::process::id()));
    std::fs::write(&path, jar(session)).unwrap();
    path.to_string_lossy().into_owned()
}

db_test!(
    the_jar_is_seeded_once_from_the_export_and_then_read_from_the_database,
    |f| {
        let path = export("instagram-cookies-seed", "from-file");
        let settings = settings(&[("INSTAGRAM_COOKIES_FROM", path.as_str())]);

        let seeded = runner::session_jar(&f.pool, &settings)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seeded.get("sessionid"), Some("from-file"));
        let saved = store::load_session(&f.pool, ACCOUNT)
            .await
            .unwrap()
            .unwrap();
        assert!(saved.contains("from-file"), "{saved}");

        // The database's jar wins over the export from then on: it holds the rotations.
        store::save_session(&f.pool, ACCOUNT, &jar("from-db"))
            .await
            .unwrap();
        let loaded = runner::session_jar(&f.pool, &settings)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.get("sessionid"), Some("from-db"));
        std::fs::remove_file(&path).unwrap();
    }
);

db_test!(no_jar_anywhere_is_none_rather_than_an_error, |f| {
    assert!(
        runner::session_jar(&f.pool, &settings(&[]))
            .await
            .unwrap()
            .is_none()
    );
    let missing = std::env::temp_dir().join("instagram-cookies-absent.json");
    let settings = settings(&[("INSTAGRAM_COOKIES_FROM", missing.to_str().unwrap())]);
    assert!(
        runner::session_jar(&f.pool, &settings)
            .await
            .unwrap()
            .is_none()
    );
});
