//! Characterization pins: a flight sampled at fixed ticks and compared,
//! value by value, with the recording in `tests/pins/<name>.pin` within a
//! tolerance. They hold the trajectories the pilot flies (goals, gotos,
//! holds, landings) while the code that shapes them is restructured.
//!
//! `PILOT_PINS_WRITE=1` records the pins again. Only for a deliberate
//! change of how the pilot flies, and the diff of the `.pin` files belongs
//! in the same change.

use std::fmt::Write as _;
use std::path::PathBuf;

/// One pinned flight: its columns and a row of values per sampled tick.
pub struct Pin {
    name: String,
    cols: &'static [&'static str],
    rows: Vec<(u64, Vec<f32>)>,
}

impl Pin {
    pub fn new(name: impl Into<String>, cols: &'static [&'static str]) -> Self {
        Self {
            name: name.into(),
            cols,
            rows: Vec::new(),
        }
    }

    /// The values at `t_ms`, one per column.
    pub fn row(&mut self, t_ms: u64, vals: &[f32]) {
        assert_eq!(
            vals.len(),
            self.cols.len(),
            "{}: one value per column",
            self.name
        );
        self.rows.push((t_ms, vals.to_vec()));
    }

    fn path(&self) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/pins")
            .join(format!("{}.pin", self.name))
    }

    fn text(&self) -> String {
        let mut s = format!("t_ms {}\n", self.cols.join(" "));
        for (t, vals) in &self.rows {
            let _ = write!(s, "{t}");
            for v in vals {
                let _ = write!(s, " {v:.6}");
            }
            s.push('\n');
        }
        s
    }

    /// Every sampled tick and value as recorded, each value within `tol`;
    /// the largest difference seen, for the test to report.
    pub fn check(&self, tol: f32) -> f32 {
        assert!(!self.rows.is_empty(), "{}: nothing sampled", self.name);
        let path = self.path();
        if std::env::var_os("PILOT_PINS_WRITE").is_some() {
            std::fs::write(&path, self.text()).expect("pin written");
            return 0.0;
        }
        let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (record it with PILOT_PINS_WRITE=1)",
                path.display()
            )
        });
        let mut lines = want.lines();
        let header = lines.next().unwrap_or("");
        assert_eq!(
            header,
            format!("t_ms {}", self.cols.join(" ")),
            "{}: columns",
            self.name
        );
        let want: Vec<&str> = lines.collect();
        assert_eq!(want.len(), self.rows.len(), "{}: sampled ticks", self.name);
        let mut worst = 0.0f32;
        for (line, (t, vals)) in want.iter().zip(&self.rows) {
            let mut f = line.split_whitespace();
            let wt: u64 = f.next().and_then(|x| x.parse().ok()).expect("a tick");
            assert_eq!(wt, *t, "{}: tick", self.name);
            for (col, v) in self.cols.iter().zip(vals) {
                let w: f32 = f.next().and_then(|x| x.parse().ok()).expect("a value");
                let d = (v - w).abs();
                assert!(
                    d <= tol,
                    "{}: {col} at {t} ms is {v:.6}, pinned {w:.6} (off by {d:.6}, tolerance {tol})",
                    self.name
                );
                worst = worst.max(d);
            }
        }
        worst
    }
}
