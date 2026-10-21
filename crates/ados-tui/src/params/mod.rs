//! FC parameter metadata, staging, and file import/export.

pub mod file_io;
pub mod metadata;
pub mod store;

/// The parameter group a name falls in when the catalog names none: the
/// first underscore-separated segment (`BATT_LOW_VOLT` → `BATT`), else `OTHER`.
pub fn category_from_name(name: &str) -> String {
    match name.find('_') {
        Some(idx) if idx > 0 => name[..idx].to_uppercase(),
        _ => "OTHER".to_string(),
    }
}

/// Whether two parameter values are the same. Parameters travel as
/// single-precision floats, so a typed `0.135` equals the FC's
/// `0.13500000536…`.
pub fn same(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    let scale = a.abs().max(b.abs()).max(1e-6);
    (a - b).abs() <= scale * 1e-6
}

/// Identity parameters the agent refuses to write.
pub const IDENTITY_PARAMS: [&str; 3] = ["SYSID_THISMAV", "MAV_SYSID", "MAV_SYS_ID"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_is_the_first_segment() {
        assert_eq!(category_from_name("BATT_LOW_VOLT"), "BATT");
        assert_eq!(category_from_name("FORMAT_VERSION"), "FORMAT");
        assert_eq!(category_from_name("SYSID"), "OTHER");
        assert_eq!(category_from_name("_X"), "OTHER");
    }

    #[test]
    fn single_precision_values_compare_equal() {
        assert!(same(0.135, 0.135_000_005_364_418_03));
        assert!(!same(0.135, 0.136));
        assert!(same(500.0, 500.0));
        assert!(!same(0.0, 0.001));
    }
}
