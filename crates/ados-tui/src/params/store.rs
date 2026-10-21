//! The parameter table's model: live values, staged edits, favourites, the
//! sidebar filter, and fuzzy search over a per-parameter haystack.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

use super::metadata::{Catalog, ParamMeta};
use super::{category_from_name, same, IDENTITY_PARAMS};

/// Which parameters the table lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Staged,
    NonDefault,
    Favourites,
    Group(String),
}

/// A staged edit: the new value and the live value it was staged against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Staged {
    pub value: f64,
    pub base: Option<f64>,
}

/// What staging a value did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    Staged,
    /// The value equals the live one, so any staged edit was dropped.
    Unchanged,
    /// The agent would refuse it.
    ReadOnly,
}

/// Counts for the fixed sidebar entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub all: usize,
    pub staged: usize,
    pub non_default: usize,
    pub favourites: usize,
}

#[derive(Default)]
pub struct Store {
    live: BTreeMap<String, f64>,
    staged: BTreeMap<String, Staged>,
    catalog: Option<Arc<Catalog>>,
    favourites: BTreeSet<String>,
    haystacks: HashMap<String, String>,
    filter: Filter,
    query: String,
    visible: Vec<String>,
    groups: Vec<(String, usize)>,
    counts: Counts,
}

impl Store {
    // --- data ----------------------------------------------------------------

    /// Replace the live values. Staged edits survive.
    pub fn set_live(&mut self, live: BTreeMap<String, f64>) {
        self.live = live;
        self.rebuild();
    }

    /// Record one value the FC confirmed.
    pub fn set_one(&mut self, name: &str, value: f64) {
        self.live.insert(name.to_string(), value);
        self.rebuild();
    }

    pub fn live(&self) -> &BTreeMap<String, f64> {
        &self.live
    }

    pub fn value(&self, name: &str) -> Option<f64> {
        self.live.get(name).copied()
    }

    pub fn set_catalog(&mut self, catalog: Option<Arc<Catalog>>) {
        self.catalog = catalog;
        self.haystacks.clear();
        self.rebuild();
    }

    pub fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_deref()
    }

    pub fn meta(&self, name: &str) -> Option<&ParamMeta> {
        self.catalog.as_ref().and_then(|c| c.get(name))
    }

    pub fn set_favourites<I: IntoIterator<Item = String>>(&mut self, names: I) {
        self.favourites = names.into_iter().collect();
        self.rebuild();
    }

    pub fn is_favourite(&self, name: &str) -> bool {
        self.favourites.contains(name)
    }

    /// Toggle a favourite; returns whether it is now one.
    pub fn toggle_favourite(&mut self, name: &str) -> bool {
        let now = if self.favourites.remove(name) {
            false
        } else {
            self.favourites.insert(name.to_string());
            true
        };
        self.rebuild();
        now
    }

    pub fn favourites(&self) -> Vec<String> {
        self.favourites.iter().cloned().collect()
    }

    // --- staging ---------------------------------------------------------------

    /// Stage `value`; staging the live value removes the entry.
    pub fn stage(&mut self, name: &str, value: f64) -> StageOutcome {
        let outcome = self.stage_quiet(name, value);
        self.rebuild();
        outcome
    }

    pub fn unstage(&mut self, name: &str) -> bool {
        let had = self.staged.remove(name).is_some();
        self.rebuild();
        had
    }

    /// Run several staging edits, recomputing the view once at the end.
    pub fn batch<R>(&mut self, edit: impl FnOnce(&mut StagingBatch<'_>) -> R) -> R {
        let out = edit(&mut StagingBatch { store: self });
        self.rebuild();
        out
    }

    fn stage_quiet(&mut self, name: &str, value: f64) -> StageOutcome {
        if self.read_only(name) {
            return StageOutcome::ReadOnly;
        }
        let live = self.value(name);
        if live.is_some_and(|l| same(l, value)) {
            self.staged.remove(name);
            StageOutcome::Unchanged
        } else {
            self.staged
                .insert(name.to_string(), Staged { value, base: live });
            StageOutcome::Staged
        }
    }

    /// Drop every staged edit; returns how many there were.
    pub fn discard_all(&mut self) -> usize {
        let n = self.staged.len();
        self.staged.clear();
        self.rebuild();
        n
    }

    pub fn staged(&self) -> &BTreeMap<String, Staged> {
        &self.staged
    }

    pub fn staged_value(&self, name: &str) -> Option<f64> {
        self.staged.get(name).map(|s| s.value)
    }

    /// The live value moved after the edit was staged.
    pub fn drift(&self, name: &str) -> bool {
        match (self.staged.get(name), self.value(name)) {
            (Some(s), Some(live)) => s.base.is_none_or(|b| !same(b, live)),
            _ => false,
        }
    }

    pub fn read_only(&self, name: &str) -> bool {
        IDENTITY_PARAMS.contains(&name) || self.meta(name).is_some_and(|m| m.read_only)
    }

    /// The live value differs from the catalog default.
    pub fn non_default(&self, name: &str) -> bool {
        match (
            self.value(name),
            self.meta(name).and_then(|m| m.default_value),
        ) {
            (Some(v), Some(d)) => !same(v, d),
            _ => false,
        }
    }

    pub fn group_of(&self, name: &str) -> String {
        self.meta(name)
            .and_then(|m| m.group.clone())
            .filter(|g| !g.is_empty())
            .unwrap_or_else(|| category_from_name(name))
    }

    // --- view ------------------------------------------------------------------

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.rebuild();
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn set_query(&mut self, query: &str) {
        if self.query != query {
            self.query = query.to_string();
            self.rebuild();
        }
    }

    /// Names the table lists, in order.
    pub fn visible(&self) -> &[String] {
        &self.visible
    }

    /// Groups with their parameter counts, sorted by name.
    pub fn groups(&self) -> &[(String, usize)] {
        &self.groups
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// Every name known: live values plus staged edits whose parameter left
    /// the live set.
    fn names(&self) -> BTreeSet<String> {
        self.live
            .keys()
            .chain(self.staged.keys())
            .cloned()
            .collect()
    }

    fn haystack(&mut self, name: &str) -> &str {
        if !self.haystacks.contains_key(name) {
            let mut text = name.to_string();
            if let Some(m) = self.catalog.as_ref().and_then(|c| c.get(name)) {
                for part in [&m.human_name, &m.description] {
                    text.push(' ');
                    text.push_str(part);
                }
                for (_, label) in m.values.iter().flatten() {
                    text.push(' ');
                    text.push_str(label);
                }
                for (_, label) in m.bitmask.iter().flatten() {
                    text.push(' ');
                    text.push_str(label);
                }
            }
            self.haystacks.insert(name.to_string(), text);
        }
        &self.haystacks[name]
    }

    fn passes(&self, name: &str) -> bool {
        match &self.filter {
            Filter::All => true,
            Filter::Staged => self.staged.contains_key(name),
            Filter::NonDefault => self.non_default(name),
            Filter::Favourites => self.favourites.contains(name),
            Filter::Group(g) => self.group_of(name) == *g,
        }
    }

    /// Recompute counts, groups, and the visible list.
    fn rebuild(&mut self) {
        let names = self.names();
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        let mut counts = Counts {
            all: names.len(),
            staged: self.staged.len(),
            ..Counts::default()
        };
        for name in &names {
            *groups.entry(self.group_of(name)).or_default() += 1;
            counts.non_default += usize::from(self.non_default(name));
            counts.favourites += usize::from(self.favourites.contains(name));
        }
        self.groups = groups.into_iter().collect();
        self.counts = counts;

        let candidates: Vec<String> = names.into_iter().filter(|n| self.passes(n)).collect();
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.visible = candidates;
            return;
        }
        let pattern = Pattern::parse(&query, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, String)> = Vec::new();
        for name in candidates {
            let hay = self.haystack(&name).to_string();
            if let Some(s) = crate::palette::score(&pattern, &mut matcher, &hay, &mut buf) {
                scored.push((s, name));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        self.visible = scored.into_iter().map(|(_, n)| n).collect();
    }
}

/// Staging edits inside [`Store::batch`]; the view is recomputed once when the
/// batch ends instead of after every edit.
pub struct StagingBatch<'a> {
    store: &'a mut Store,
}

impl StagingBatch<'_> {
    pub fn value(&self, name: &str) -> Option<f64> {
        self.store.value(name)
    }

    /// Same rules as [`Store::stage`].
    pub fn stage(&mut self, name: &str, value: f64) -> StageOutcome {
        self.store.stage_quiet(name, value)
    }

    pub fn unstage(&mut self, name: &str) -> bool {
        self.store.staged.remove(name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(values: &[(&str, f64)]) -> Store {
        let mut s = Store::default();
        s.set_live(values.iter().map(|(n, v)| (n.to_string(), *v)).collect());
        s
    }

    #[test]
    fn staging_the_original_value_removes_the_entry() {
        let mut s = store(&[("ATC_RAT_RLL_P", 0.135)]);
        assert_eq!(s.stage("ATC_RAT_RLL_P", 0.2), StageOutcome::Staged);
        assert_eq!(s.staged().len(), 1);
        assert_eq!(
            s.stage("ATC_RAT_RLL_P", 0.135_000_005_364_418_03),
            StageOutcome::Unchanged
        );
        assert!(s.staged().is_empty());
    }

    #[test]
    fn drift_is_flagged_when_the_live_value_moves() {
        let mut s = store(&[("WPNAV_SPEED", 500.0)]);
        s.stage("WPNAV_SPEED", 750.0);
        assert!(!s.drift("WPNAV_SPEED"));
        s.set_live([("WPNAV_SPEED".to_string(), 600.0)].into_iter().collect());
        assert!(s.drift("WPNAV_SPEED"));
        assert_eq!(
            s.staged_value("WPNAV_SPEED"),
            Some(750.0),
            "staged edits survive a refresh"
        );
    }

    #[test]
    fn identity_params_are_read_only() {
        let mut s = store(&[("SYSID_THISMAV", 1.0)]);
        assert_eq!(s.stage("SYSID_THISMAV", 2.0), StageOutcome::ReadOnly);
        assert!(s.staged().is_empty());
    }

    #[test]
    fn filters_and_search_narrow_the_list() {
        let mut s = store(&[
            ("BATT_LOW_VOLT", 10.5),
            ("BATT_CAPACITY", 3300.0),
            ("GPS_TYPE", 1.0),
        ]);
        assert_eq!(
            s.groups(),
            &[("BATT".to_string(), 2), ("GPS".to_string(), 1)]
        );
        s.set_filter(Filter::Group("GPS".into()));
        assert_eq!(s.visible(), &["GPS_TYPE".to_string()]);
        s.set_filter(Filter::All);
        s.set_query("capacity");
        assert_eq!(s.visible(), &["BATT_CAPACITY".to_string()]);
        s.set_query("");
        s.stage("GPS_TYPE", 2.0);
        s.set_filter(Filter::Staged);
        assert_eq!(s.visible(), &["GPS_TYPE".to_string()]);
        assert_eq!(s.counts().staged, 1);
        s.toggle_favourite("BATT_LOW_VOLT");
        s.set_filter(Filter::Favourites);
        assert_eq!(s.visible(), &["BATT_LOW_VOLT".to_string()]);
    }
}
