//! Scheduler-pressure counters: page faults from `/proc/vmstat`.
//!
//! Context switches (`ctxt`) and forks (`processes`) come from `/proc/stat`
//! (parsed in [`super::cpu`]); the page-fault counters come from `/proc/vmstat`:
//! `pgfault` (total minor + major faults) and `pgmajfault` (major faults that
//! hit storage). All three are cumulative; their rates are derived at read time
//! from successive snapshots, so the raw values are recorded.

use std::path::Path;

use super::reader::under;

/// Cumulative page-fault counters from `/proc/vmstat`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmStat {
    /// `pgfault`: cumulative total page faults.
    pub pgfault: Option<u64>,
    /// `pgmajfault`: cumulative major page faults.
    pub pgmajfault: Option<u64>,
}

/// Read and parse `/proc/vmstat`. Returns a default (all-`None`) [`VmStat`] when
/// the file is absent.
pub fn read_vmstat(root: &Path) -> VmStat {
    let path = under(root, "/proc/vmstat");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_vmstat(&text),
        Err(_) => VmStat::default(),
    }
}

/// Parse the text of `/proc/vmstat`. Each line is `<key> <value>`; only the
/// page-fault keys are extracted.
pub fn parse_vmstat(text: &str) -> VmStat {
    let mut out = VmStat::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let value = parts.next().and_then(|v| v.parse::<u64>().ok());
        match key {
            "pgfault" => out.pgfault = value,
            "pgmajfault" => out.pgmajfault = value,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_page_fault_counters() {
        let text = "\
nr_free_pages 123456
pgfault 9876543
pgmajfault 1234
pgsteal_kswapd 0
";
        let vm = parse_vmstat(text);
        assert_eq!(vm.pgfault, Some(9_876_543));
        assert_eq!(vm.pgmajfault, Some(1234));
    }

    #[test]
    fn read_vmstat_from_a_fixture_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("proc")).unwrap();
        fs::write(
            dir.path().join("proc/vmstat"),
            "pgfault 100\npgmajfault 5\n",
        )
        .unwrap();
        let vm = read_vmstat(dir.path());
        assert_eq!(vm.pgfault, Some(100));
        assert_eq!(vm.pgmajfault, Some(5));
    }

    #[test]
    fn read_vmstat_is_default_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_vmstat(dir.path()), VmStat::default());
    }

    #[test]
    fn missing_keys_stay_none() {
        let vm = parse_vmstat("nr_free_pages 1\npgfault 2\n");
        assert_eq!(vm.pgfault, Some(2));
        assert_eq!(vm.pgmajfault, None);
    }
}
