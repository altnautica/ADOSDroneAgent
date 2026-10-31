//! Catalog consistency checks run before emitting.

use crate::Catalog;

/// Referential checks the generated tables rely on, run before anything is
/// emitted: ids unique within each section, every `[[method]]` `required_cap`
/// naming a declared agent capability, and every method naming exactly one of
/// `required_cap`, `inline` or `open`. A violation would otherwise generate a
/// dispatch row no plugin can ever be granted, or one that is silently
/// ungated, with `--check` reporting clean.
pub(crate) fn validate_catalog(cat: &Catalog) -> Result<(), Vec<String>> {
    use std::collections::HashSet;
    let mut problems = Vec::new();
    let mut dupes = |section: &str, ids: &mut dyn Iterator<Item = &str>| {
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id) {
                problems.push(format!("duplicate {section} id `{id}`"));
            }
        }
    };
    dupes("agent", &mut cat.agent.iter().map(|c| c.id.as_str()));
    dupes("gcs", &mut cat.gcs.iter().map(|c| c.id.as_str()));
    dupes("method", &mut cat.method.iter().map(|m| m.id.as_str()));
    let agent: HashSet<&str> = cat.agent.iter().map(|c| c.id.as_str()).collect();
    for m in &cat.method {
        if let Some(cap) = &m.required_cap {
            if !agent.contains(cap.as_str()) {
                problems.push(format!(
                    "method `{}` requires unknown capability `{cap}`",
                    m.id
                ));
            }
        }
        let gates =
            usize::from(m.required_cap.is_some()) + usize::from(m.inline) + usize::from(m.open);
        if gates != 1 {
            problems.push(format!(
                "method `{}` must name exactly one of required_cap, inline = true or \
                 open = true",
                m.id
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}
