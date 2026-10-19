//! The byte-level glob matcher behind topic subscriptions.

/// Minimal fnmatch supporting `*` (any run, including across `.`) and `?` (one
/// byte), which is all the topic taxonomy uses. Topics and patterns are
/// printable ASCII (enforced at publish and subscribe), so matching bytes is
/// matching characters, and no per-event allocation is needed. Iterative
/// single-star backtracking: O(pattern x topic) worst case, which the
/// [`super::EVENT_TOPIC_MAX_BYTES`] cap keeps small.
pub(super) fn fnmatch(p: &[u8], t: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}
