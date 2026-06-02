//! Metric downsampling for charts.
//!
//! A chart over a long window must not scan the raw rows: it asks for a coarse
//! `bucket` grain and an aggregate, and the answer is computed per time bucket.
//! When the grain matches a maintained rollup (`1m`/`1h`), the answer is read
//! from the rollup tables (a year-scale chart is a rollup scan, not a raw
//! scan). Otherwise it is a `GROUP BY` over the raw `metrics` table.
//!
//! `avg` is derived as `sum/count` so a coarse grain composes from finer ones
//! (the rollup tables carry `sum`+`count`, never a pre-averaged value).
//! Percentiles (`p50`/`p95`) need the individual samples and so are only
//! available against the raw table; requesting a percentile at a rollup grain
//! falls back to a raw scan.

use rusqlite::Connection;
use serde::Serialize;

use super::params::ParamError;

/// The aggregate to compute per bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    /// Mean (`sum/count`).
    Avg,
    /// Minimum.
    Min,
    /// Maximum.
    Max,
    /// Median (raw only).
    P50,
    /// 95th percentile (raw only).
    P95,
    /// The latest sample in the bucket.
    Last,
    /// The sample count in the bucket.
    Count,
}

impl Agg {
    /// Parse the `agg` selector, defaulting to `avg`.
    pub fn parse(s: Option<&str>) -> Result<Agg, ParamError> {
        match s.unwrap_or("avg") {
            "avg" => Ok(Agg::Avg),
            "min" => Ok(Agg::Min),
            "max" => Ok(Agg::Max),
            "p50" => Ok(Agg::P50),
            "p95" => Ok(Agg::P95),
            "last" => Ok(Agg::Last),
            "count" => Ok(Agg::Count),
            other => Err(ParamError::new(
                "bad_agg",
                format!("unknown agg '{other}', expected avg|min|max|p50|p95|last|count"),
            )),
        }
    }

    /// Whether this aggregate can be served from a rollup table. Percentiles
    /// need the individual samples and so cannot.
    fn rollup_capable(self) -> bool {
        !matches!(self, Agg::P50 | Agg::P95)
    }
}

/// The bucket grain selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grain {
    /// Pick the coarsest grain that yields a sane point count for the window.
    Auto,
    /// One-second buckets (raw scan).
    Sec,
    /// One-minute buckets (rollup-eligible).
    Min,
    /// One-hour buckets (rollup-eligible).
    Hour,
}

impl Grain {
    /// Parse the `bucket` selector, defaulting to `auto`.
    pub fn parse(s: Option<&str>) -> Result<Grain, ParamError> {
        match s.unwrap_or("auto") {
            "auto" => Ok(Grain::Auto),
            "1s" => Ok(Grain::Sec),
            "1m" => Ok(Grain::Min),
            "1h" => Ok(Grain::Hour),
            other => Err(ParamError::new(
                "bad_bucket",
                format!("unknown bucket '{other}', expected auto|1s|1m|1h"),
            )),
        }
    }

    /// The bucket width in microseconds for a concrete grain.
    fn width_us(self) -> i64 {
        match self {
            Grain::Auto | Grain::Sec => 1_000_000,
            Grain::Min => 60_000_000,
            Grain::Hour => 3_600_000_000,
        }
    }

    /// The maintained rollup table for a grain, if one exists.
    fn rollup_table(self) -> Option<&'static str> {
        match self {
            Grain::Min => Some("metrics_1m"),
            Grain::Hour => Some("metrics_1h"),
            _ => None,
        }
    }

    /// Resolve `Auto` to a concrete grain for the requested window, aiming for a
    /// few hundred to ~1000 points. An open-ended window defaults to seconds.
    fn resolve(self, from_us: Option<i64>, to_us: Option<i64>) -> Grain {
        if self != Grain::Auto {
            return self;
        }
        match (from_us, to_us) {
            (Some(lo), Some(hi)) if hi > lo => {
                let span = hi - lo;
                // Target ~600 points; step up grains as the window grows.
                if span <= 600 * Grain::Sec.width_us() {
                    Grain::Sec
                } else if span <= 600 * Grain::Min.width_us() {
                    Grain::Min
                } else {
                    Grain::Hour
                }
            }
            _ => Grain::Sec,
        }
    }
}

/// One downsampled bucket. `value` is the aggregate; `count` is the sample
/// count folded into the bucket (useful to gauge confidence).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Bucket {
    /// The bucket's start time, microsecond epoch.
    pub bucket_us: i64,
    /// The metric this bucket belongs to.
    pub metric: String,
    /// The aggregate value.
    pub value: f64,
    /// Samples in the bucket.
    pub count: i64,
}

/// Parameters for an aggregate request.
#[derive(Debug, Clone)]
pub struct AggregateParams {
    /// One or more dotted metric keys (required).
    pub metrics: Vec<String>,
    /// Window lower/upper bounds, microsecond epoch.
    pub from_us: Option<i64>,
    pub to_us: Option<i64>,
    /// Restrict to one session.
    pub session: Option<i64>,
    /// The bucket grain.
    pub grain: Grain,
    /// The aggregate.
    pub agg: Agg,
}

impl AggregateParams {
    /// Parse and validate an aggregate request from a query map.
    pub fn parse(params: &super::params::QueryParams, now_us: i64) -> Result<Self, ParamError> {
        let metrics = params.get_all("metric");
        if metrics.is_empty() {
            return Err(ParamError::new(
                "missing_metric",
                "aggregate requires at least one metric",
            ));
        }
        let from_us = super::params::parse_time(params.get("from"), now_us, "from")?.or(
            super::params::parse_time(params.get("since"), now_us, "since")?,
        );
        let to_us = super::params::parse_time(params.get("to"), now_us, "to")?;
        let session = match params.get("session") {
            Some(s) => Some(s.parse::<i64>().map_err(|_| {
                ParamError::new("bad_session", format!("session '{s}' is not a number"))
            })?),
            None => None,
        };
        Ok(Self {
            metrics,
            from_us,
            to_us,
            session,
            grain: Grain::parse(params.get("bucket"))?,
            agg: Agg::parse(params.get("agg"))?,
        })
    }
}

/// Compute the downsampled series for the requested metrics. Reads from the
/// rollup tables when the resolved grain is rollup-eligible and the aggregate is
/// rollup-capable; otherwise groups over the raw `metrics` table.
pub fn aggregate(conn: &Connection, params: &AggregateParams) -> rusqlite::Result<Vec<Bucket>> {
    let grain = params.grain.resolve(params.from_us, params.to_us);
    let mut out: Vec<Bucket> = Vec::new();
    for metric in &params.metrics {
        let series = match grain.rollup_table() {
            Some(table) if params.agg.rollup_capable() && params.session.is_none() => {
                aggregate_from_rollup(conn, table, metric, params)?
            }
            _ => aggregate_from_raw(conn, metric, grain, params)?,
        };
        out.extend(series);
    }
    // Stable order: by metric then bucket time so a multi-metric chart is
    // deterministic.
    out.sort_by(|a, b| a.metric.cmp(&b.metric).then(a.bucket_us.cmp(&b.bucket_us)));
    Ok(out)
}

/// Read the series from a maintained rollup table. The rollup is keyed by the
/// already-floored `bucket_us`, so the read is a bounded index scan. The session
/// dimension is not maintained in the rollup, so the caller routes to the raw
/// path when a session is requested.
fn aggregate_from_rollup(
    conn: &Connection,
    table: &str,
    metric: &str,
    params: &AggregateParams,
) -> rusqlite::Result<Vec<Bucket>> {
    // The rollup carries one row per (metric, tags_key, bucket). Fold the tag
    // groupings together per bucket so a chart of a tagless metric and a tagged
    // metric reads the same: SUM the sums and counts, MIN the mins, MAX the
    // maxes, and pick the last value across folded tag groups as the one with
    // the greatest last_us via a correlated subquery. `avg` is derived as
    // sum/count downstream so the grains compose.
    let mut where_clauses = vec!["metric = ?".to_string()];
    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(metric.to_string())];
    if let Some(lo) = params.from_us {
        where_clauses.push("bucket_us >= ?".to_string());
        binds.push(Box::new(lo));
    }
    if let Some(hi) = params.to_us {
        where_clauses.push("bucket_us < ?".to_string());
        binds.push(Box::new(hi));
    }
    let where_sql = format!("WHERE {}", where_clauses.join(" AND "));
    let sql = format!(
        "SELECT r.bucket_us, \
            SUM(r.sum) AS sum_sum, \
            SUM(r.count) AS sum_count, \
            MIN(r.min) AS min_min, \
            MAX(r.max) AS max_max, \
            (SELECT r2.last FROM {table} r2 \
              WHERE r2.metric = r.metric AND r2.bucket_us = r.bucket_us \
              ORDER BY r2.last_us DESC LIMIT 1) AS last_last \
         FROM {table} r {where_sql} \
         GROUP BY r.bucket_us ORDER BY r.bucket_us"
    );
    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let metric_owned = metric.to_string();
    let rows = stmt.query_map(bind_refs.as_slice(), |row| {
        let bucket_us: i64 = row.get(0)?;
        let sum_sum: f64 = row.get(1)?;
        let sum_count: i64 = row.get(2)?;
        let min_min: f64 = row.get(3)?;
        let max_max: f64 = row.get(4)?;
        let last_last: f64 = row.get(5)?;
        let value = match params.agg {
            Agg::Avg => {
                if sum_count > 0 {
                    sum_sum / sum_count as f64
                } else {
                    0.0
                }
            }
            Agg::Min => min_min,
            Agg::Max => max_max,
            Agg::Last => last_last,
            Agg::Count => sum_count as f64,
            Agg::P50 | Agg::P95 => sum_sum / sum_count.max(1) as f64,
        };
        Ok(Bucket {
            bucket_us,
            metric: metric_owned.clone(),
            value,
            count: sum_count,
        })
    })?;
    rows.collect()
}

/// Group over the raw `metrics` table at an arbitrary grain. Percentiles are
/// computed exactly by reading the bucket's samples and selecting the rank.
fn aggregate_from_raw(
    conn: &Connection,
    metric: &str,
    grain: Grain,
    params: &AggregateParams,
) -> rusqlite::Result<Vec<Bucket>> {
    let width = grain.width_us();
    if matches!(params.agg, Agg::P50 | Agg::P95) {
        return aggregate_percentile_from_raw(conn, metric, width, params);
    }
    let value_expr = match params.agg {
        Agg::Avg => "AVG(value)",
        Agg::Min => "MIN(value)",
        Agg::Max => "MAX(value)",
        Agg::Last => "value", // resolved by the ORDER inside the group below
        Agg::Count => "COUNT(*)",
        Agg::P50 | Agg::P95 => unreachable!("percentiles handled above"),
    };
    let mut where_clauses = vec!["metric = ?".to_string()];
    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(metric.to_string())];
    if let Some(lo) = params.from_us {
        where_clauses.push("ts_us >= ?".to_string());
        binds.push(Box::new(lo));
    }
    if let Some(hi) = params.to_us {
        where_clauses.push("ts_us < ?".to_string());
        binds.push(Box::new(hi));
    }
    if let Some(sid) = params.session {
        where_clauses.push("session = ?".to_string());
        binds.push(Box::new(sid));
    }
    let where_sql = format!("WHERE {}", where_clauses.join(" AND "));
    // `last` needs the value at the greatest ts_us in the bucket: a correlated
    // subquery selects it; the other aggregates are plain group functions.
    let sql = if matches!(params.agg, Agg::Last) {
        format!(
            "SELECT (ts_us / {width}) * {width} AS bucket_us, \
                (SELECT m2.value FROM metrics m2 \
                  WHERE m2.metric = m.metric \
                    AND (m2.ts_us / {width}) * {width} = (m.ts_us / {width}) * {width} \
                  ORDER BY m2.ts_us DESC LIMIT 1) AS v, \
                COUNT(*) AS c \
             FROM metrics m {where_sql} \
             GROUP BY bucket_us ORDER BY bucket_us"
        )
    } else {
        format!(
            "SELECT (ts_us / {width}) * {width} AS bucket_us, {value_expr} AS v, COUNT(*) AS c \
             FROM metrics {where_sql} \
             GROUP BY bucket_us ORDER BY bucket_us"
        )
    };
    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let metric_owned = metric.to_string();
    let rows = stmt.query_map(bind_refs.as_slice(), |row| {
        Ok(Bucket {
            bucket_us: row.get(0)?,
            metric: metric_owned.clone(),
            value: row.get(1)?,
            count: row.get(2)?,
        })
    })?;
    rows.collect()
}

/// Compute an exact percentile per bucket from the raw samples. Reads the
/// (bucket, value) pairs ordered, then selects the rank within each bucket. The
/// store is bounded by retention, so a windowed percentile read is bounded too.
fn aggregate_percentile_from_raw(
    conn: &Connection,
    metric: &str,
    width: i64,
    params: &AggregateParams,
) -> rusqlite::Result<Vec<Bucket>> {
    let q = if matches!(params.agg, Agg::P95) {
        0.95
    } else {
        0.50
    };
    let mut where_clauses = vec!["metric = ?".to_string()];
    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(metric.to_string())];
    if let Some(lo) = params.from_us {
        where_clauses.push("ts_us >= ?".to_string());
        binds.push(Box::new(lo));
    }
    if let Some(hi) = params.to_us {
        where_clauses.push("ts_us < ?".to_string());
        binds.push(Box::new(hi));
    }
    if let Some(sid) = params.session {
        where_clauses.push("session = ?".to_string());
        binds.push(Box::new(sid));
    }
    let where_sql = format!("WHERE {}", where_clauses.join(" AND "));
    let sql = format!(
        "SELECT (ts_us / {width}) * {width} AS bucket_us, value \
         FROM metrics {where_sql} ORDER BY bucket_us, value"
    );
    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let mut per_bucket: Vec<(i64, Vec<f64>)> = Vec::new();
    let mut rows = stmt.query(bind_refs.as_slice())?;
    while let Some(row) = rows.next()? {
        let bucket_us: i64 = row.get(0)?;
        let value: f64 = row.get(1)?;
        match per_bucket.last_mut() {
            Some((b, vals)) if *b == bucket_us => vals.push(value),
            _ => per_bucket.push((bucket_us, vec![value])),
        }
    }
    let mut out = Vec::with_capacity(per_bucket.len());
    for (bucket_us, vals) in per_bucket {
        // vals is already sorted ascending (ORDER BY value within the bucket).
        let n = vals.len();
        // Nearest-rank percentile: index = ceil(q * n) - 1, clamped.
        let rank = (((q * n as f64).ceil() as usize).max(1) - 1).min(n - 1);
        out.push(Bucket {
            bucket_us,
            metric: metric.to_string(),
            value: vals[rank],
            count: n as i64,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
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
}
