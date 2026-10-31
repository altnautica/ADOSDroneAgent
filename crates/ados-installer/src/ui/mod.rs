//! The installer's pieces of the live progress UI.
//!
//! The renderers, the event channel, the terminal and the theme live in
//! [`ados_term::progress`]; this module holds what is specific to installing
//! the agent: the step→group checklists and their wording ([`groups`]), the
//! closing install card ([`summary`]), and the subprocess-line reducers that
//! feed the live-detail pane ([`activity`]).

pub mod activity;
pub mod groups;
pub mod summary;
