//! Const-evaluable views of the ground-station role ownership in [`UNITS`].

use super::{UnitSpec, UNITS};

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn role_owns(unit: &UnitSpec, role: Option<&str>) -> bool {
    let Some(role) = role else {
        return !unit.roles.is_empty();
    };
    let mut i = 0;
    while i < unit.roles.len() {
        if str_eq(unit.roles[i], role) {
            return true;
        }
        i += 1;
    }
    false
}

/// How many units `role` owns; `None` counts every role-owned unit.
pub const fn role_unit_count(role: Option<&str>) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < UNITS.len() {
        if role_owns(&UNITS[i], role) {
            n += 1;
        }
        i += 1;
    }
    n
}

/// The unit file names `role` owns, in table order (its start order); `None`
/// lists every role-owned unit. `N` must be [`role_unit_count`] of the same
/// argument, so the table and the array cannot disagree.
pub const fn role_unit_names<const N: usize>(role: Option<&str>) -> [&'static str; N] {
    assert!(N == role_unit_count(role), "role unit array size mismatch");
    let mut out = [""; N];
    let mut n = 0;
    let mut i = 0;
    while i < UNITS.len() {
        if role_owns(&UNITS[i], role) {
            out[n] = UNITS[i].name;
            n += 1;
        }
        i += 1;
    }
    out
}
