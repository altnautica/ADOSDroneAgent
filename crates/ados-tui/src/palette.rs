//! The command palette: one fuzzy-searchable list of every tab, settings page,
//! config key, loaded parameter, quick action and command.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use tui_input::Input;

use crate::app::TabId;

/// How many ranked results the palette shows.
pub const TOP_N: usize = 12;

/// What running a palette entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    Tab(TabId),
    /// Open a settings page by id (a registry id or `plugin:<plugin id>`).
    SettingsPage(String),
    /// Open the All settings row for a dotted config key.
    ConfigKey(String),
    /// Jump to a parameter in the Params tab.
    Param(String),
    /// Run quick action `i`.
    QuickAction(usize),
    Cmd(PaletteCmd),
}

/// Palette-only commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCmd {
    CycleTheme,
    CycleMotion,
    Notifications,
    CopyUrls,
}

/// One palette entry.
#[derive(Debug, Clone)]
pub struct PaletteItem {
    pub label: String,
    /// Dim right-hand text (the group: tab, setting, param, command).
    pub kind: &'static str,
    pub action: PaletteAction,
}

impl PaletteItem {
    pub fn new(label: impl Into<String>, kind: &'static str, action: PaletteAction) -> Self {
        Self {
            label: label.into(),
            kind,
            action,
        }
    }
}

/// The open palette.
#[derive(Debug, Clone)]
pub struct PaletteState {
    pub input: Input,
    pub items: Vec<PaletteItem>,
    /// Indices into `items`, best first, at most [`TOP_N`].
    pub ranked: Vec<usize>,
    pub selected: usize,
}

impl PaletteState {
    pub fn new(items: Vec<PaletteItem>, query: &str) -> Self {
        let mut s = Self {
            input: Input::new(query.to_string()),
            items,
            ranked: Vec::new(),
            selected: 0,
        };
        s.rerank();
        s
    }

    /// Re-rank after the query changed.
    pub fn rerank(&mut self) {
        self.ranked = rank(self.input.value(), &self.items);
        self.selected = 0;
    }

    /// The highlighted entry's action.
    pub fn chosen(&self) -> Option<PaletteAction> {
        self.ranked
            .get(self.selected)
            .map(|i| self.items[*i].action.clone())
    }
}

/// Rank `items` against `query`: an empty query keeps list order; otherwise
/// fuzzy score (higher first), ties in list order. At most [`TOP_N`].
pub fn rank(query: &str, items: &[PaletteItem]) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..items.len().min(TOP_N)).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            pattern
                .score(Utf32Str::new(&item.label, &mut buf), &mut matcher)
                .map(|s| (s, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(TOP_N).map(|(_, i)| i).collect()
}

/// Fuzzy-score one haystack against a parsed pattern (shared with the params
/// search so both rank the same way).
pub fn score(
    pattern: &Pattern,
    matcher: &mut Matcher,
    haystack: &str,
    buf: &mut Vec<char>,
) -> Option<u32> {
    pattern.score(Utf32Str::new(haystack, buf), matcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(labels: &[&str]) -> Vec<PaletteItem> {
        labels
            .iter()
            .map(|l| PaletteItem::new(*l, "tab", PaletteAction::Tab(TabId::Overview)))
            .collect()
    }

    #[test]
    fn fuzzy_ranking_prefers_the_closer_match() {
        let list = items(&["Overview", "Settings: Wi-Fi", "Params", "Settings: Pairing"]);
        let r = rank("wifi", &list);
        assert_eq!(r.first(), Some(&1));
        assert!(!r.contains(&2));
        assert_eq!(rank("", &list), vec![0, 1, 2, 3]);
    }
}
