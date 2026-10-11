use super::*;
use crate::db;
use crate::query::params::QueryParams;

fn seed_metrics(path: &std::path::Path) {
    let conn = db::open(path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind) VALUES (0, 'boot')",
        [],
    )
    .unwrap();
    let s = conn.last_insert_rowid();
    // 10 cpu.load samples one second apart, values 0..9.
    for i in 0..10i64 {
        conn.execute(
            "INSERT INTO metrics (ts_us, session, metric, value) VALUES (?1, ?2, 'cpu.load', ?3)",
            rusqlite::params![i * 1_000_000, s, i as f64],
        )
        .unwrap();
    }
    // Maintained 1m rollup for the same window: one bucket, sum=45, count=10,
    // min=0, max=9, last=9.
    conn.execute(
            "INSERT INTO metrics_1m (bucket_us, metric, tags_key, count, sum, min, max, last, last_us) \
             VALUES (0, 'cpu.load', '', 10, 45.0, 0.0, 9.0, 9.0, 9000000)",
            [],
        )
        .unwrap();
}

fn params(q: &str) -> AggregateParams {
    AggregateParams::parse(&QueryParams::parse(q), 0).unwrap()
}

#[test]
fn p99_reaches_deeper_into_the_tail_than_p95() {
    // The whole reason p99 exists here. A loop-rate measurement is judged on
    // its tail: at 100 Hz, p95 forgives five late frames every second, and
    // whether those five are barely late or catastrophically late is the
    // question being asked. A p99 that merely aliased p95 would answer the
    // wrong question while looking like it answered the right one.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    let conn = db::open(&path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind) VALUES (0, 'boot')",
        [],
    )
    .unwrap();
    let s = conn.last_insert_rowid();
    // A hundred samples in ONE bucket: ninety-five on time, then a tail that
    // gets progressively worse. Selection is nearest-rank
    // (`index = ceil(q*n) - 1`), so p95 lands on index 94 and p99 on index
    // 98 -- one inside the punctual body, one well into the tail. A single
    // lone outlier would NOT distinguish them: with 99 samples flat and one
    // extreme, p99 is still the flat value, because 99 percent of the data
    // genuinely is flat. The tail has to have shape for the deeper
    // percentile to mean anything.
    let mut values = vec![1.0f64; 95];
    values.extend_from_slice(&[10.0, 20.0, 30.0, 40.0, 50.0]);
    for (i, v) in values.iter().enumerate() {
        conn.execute(
                "INSERT INTO metrics (ts_us, session, metric, value) VALUES (?1, ?2, 'loop.period_ms', ?3)",
                rusqlite::params![(i as i64) * 1_000, s, v],
            )
            .unwrap();
    }
    drop(conn);

    let ro = db::open_readonly(&path).unwrap();
    let p95 = aggregate(&ro, &params("metric=loop.period_ms&bucket=1s&agg=p95")).unwrap();
    let p99 = aggregate(&ro, &params("metric=loop.period_ms&bucket=1s&agg=p99")).unwrap();

    assert_eq!(p95.len(), 1);
    assert_eq!(p99.len(), 1);
    assert!(
        p99[0].value > p95[0].value,
        "p99 ({}) must see the stall p95 ({}) steps over",
        p99[0].value,
        p95[0].value
    );
    assert_eq!(p95[0].value, 1.0, "p95 sits inside the punctual body");
    assert_eq!(
        p99[0].value, 40.0,
        "and p99 reports a real sample from the tail, not an interpolation"
    );
}

#[test]
fn p99_is_refused_at_a_rollup_grain_rather_than_silently_approximated() {
    // A percentile needs the individual samples. Serving one from a rollup
    // would return a mean wearing a percentile's name.
    assert!(!Agg::P99.rollup_capable());
}

#[test]
fn raw_avg_min_max_last_count_over_one_second_buckets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed_metrics(&path);
    let ro = db::open_readonly(&path).unwrap();
    // 1s buckets → 10 buckets, each a single sample.
    let avg = aggregate(&ro, &params("metric=cpu.load&bucket=1s&agg=avg")).unwrap();
    assert_eq!(avg.len(), 10);
    assert_eq!(avg[0].value, 0.0);
    assert_eq!(avg[9].value, 9.0);
    // count agg reports one sample per 1s bucket.
    let cnt = aggregate(&ro, &params("metric=cpu.load&bucket=1s&agg=count")).unwrap();
    assert!(cnt.iter().all(|b| b.value == 1.0));
}

#[test]
fn one_minute_grain_reads_the_rollup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed_metrics(&path);
    let ro = db::open_readonly(&path).unwrap();
    // 1m grain → the maintained rollup: one bucket, avg = 45/10 = 4.5.
    let avg = aggregate(&ro, &params("metric=cpu.load&bucket=1m&agg=avg")).unwrap();
    assert_eq!(avg.len(), 1);
    assert!((avg[0].value - 4.5).abs() < 1e-9);
    assert_eq!(avg[0].count, 10);
    let max = aggregate(&ro, &params("metric=cpu.load&bucket=1m&agg=max")).unwrap();
    assert_eq!(max[0].value, 9.0);
    let last = aggregate(&ro, &params("metric=cpu.load&bucket=1m&agg=last")).unwrap();
    assert_eq!(last[0].value, 9.0);
}

#[test]
fn percentile_falls_back_to_raw_even_at_a_rollup_grain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed_metrics(&path);
    let ro = db::open_readonly(&path).unwrap();
    // p50 at 1m grain ignores the rollup and computes over raw samples.
    let p50 = aggregate(&ro, &params("metric=cpu.load&bucket=1m&agg=p50")).unwrap();
    assert_eq!(p50.len(), 1);
    // Nearest-rank median of 0..9 (10 samples) → index ceil(0.5*10)-1 = 4 → 4.0.
    assert_eq!(p50[0].value, 4.0);
    let p95 = aggregate(&ro, &params("metric=cpu.load&bucket=1m&agg=p95")).unwrap();
    // ceil(0.95*10)-1 = 9 → 9.0.
    assert_eq!(p95[0].value, 9.0);
}

#[test]
fn auto_grain_picks_seconds_for_a_short_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed_metrics(&path);
    let ro = db::open_readonly(&path).unwrap();
    // A 10s window resolves to 1s buckets (raw scan, 10 buckets).
    let res = aggregate(
        &ro,
        &params("metric=cpu.load&from=0&to=10000000&bucket=auto&agg=avg"),
    )
    .unwrap();
    assert_eq!(res.len(), 10);
}

#[test]
fn missing_metric_is_rejected() {
    assert!(AggregateParams::parse(&QueryParams::parse("bucket=1m"), 0).is_err());
}
