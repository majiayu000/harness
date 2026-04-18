use criterion::{criterion_group, criterion_main, Criterion};
use harness_core::types::{Decision, Event, EventFilters, SessionId};
use harness_observe::event_store::EventStore;

const EVENT_COUNT: usize = 10_000;
// Watermark placed at the 9900th event, leaving ~100 events in the incremental window.
const WATERMARK_INDEX: usize = 9_900;

fn bench_event_scan(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();

    let store = rt.block_on(async {
        let store = EventStore::new(dir.path()).await.unwrap();

        // Seed events with monotonically increasing timestamps (oldest first).
        for i in 0..EVENT_COUNT {
            let mut event = Event::new(SessionId::new(), "pre_tool_use", "Edit", Decision::Pass);
            event.ts = chrono::Utc::now() - chrono::Duration::seconds((EVENT_COUNT - i) as i64);

            if i + 1 == WATERMARK_INDEX {
                store
                    .set_scan_watermark("bench_project", "gc", event.ts)
                    .await
                    .unwrap();
            }
            store.log(&event).await.unwrap();
        }
        store
    });

    let mut group = c.benchmark_group("event_scan");

    group.bench_function("full_scan_10k", |b| {
        b.iter(|| rt.block_on(store.query(&EventFilters::default())).unwrap());
    });

    group.bench_function("watermarked_scan_100", |b| {
        b.iter(|| {
            rt.block_on(async {
                let ts = store
                    .get_scan_watermark("bench_project", "gc")
                    .await
                    .unwrap();
                store
                    .query(&EventFilters {
                        since: ts,
                        ..Default::default()
                    })
                    .await
                    .unwrap()
            })
        });
    });

    group.finish();
    // Keep `dir` alive until after the benchmark group completes.
    drop(dir);
}

criterion_group!(benches, bench_event_scan);
criterion_main!(benches);
