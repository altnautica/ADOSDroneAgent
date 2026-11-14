//! The flow and rangefinder parameter rules of `cockpit-spec/sensor-health.json`
//! (`params`), parsed from the spec so every surface evaluates one table.
//!
//! Rule kinds: `eq`, `ne`, `range`, `max`, `multiple_of` (conjoined with any
//! other key of the same object), `mount` with an `else` rule for an
//! undeclared mount, `group_not_all_zero` (reported once per group) and
//! `no_forward_any_port`; a row's `when` map gates it on other parameters.
//! A row whose parameter is absent from the FC table is skipped.

use std::sync::LazyLock;

use ados_protocol::pilot::Severity;
use serde_json::Value;

use crate::config::FcSensors;
use crate::sensor_spec::spec;

#[derive(Debug, Clone, PartialEq)]
enum Clause {
    Eq(f64),
    Ne(f64),
    Range(f64, f64),
    Max(f64),
    MultipleOf(f64),
    GroupNotAllZero,
    NoForward(NoForward),
}

#[derive(Debug, Clone, PartialEq)]
struct NoForward {
    protocol_param: String,
    protocols: Vec<f64>,
    options_param: String,
    options_mask: u32,
    mav_param: String,
    mav_mask: u32,
    n: (u32, u32),
}

impl NoForward {
    fn names(&self) -> Vec<String> {
        (self.n.0..=self.n.1)
            .flat_map(|n| {
                [&self.protocol_param, &self.options_param, &self.mav_param]
                    .map(|p| p.replace("{n}", &n.to_string()))
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Rule {
    clauses: Vec<Clause>,
    /// `flow.yaw_cdeg`, `flow.pos_m.0`, `rangefinder.pos_m.2`, …
    mount: Option<String>,
    otherwise: Option<Box<Rule>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    group: Option<String>,
    rule: Rule,
    when: Vec<(String, Rule)>,
    pub severity: Severity,
}

/// One failing row (or group), worded for the readiness message.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// `FLOW_ORIENT_YAW 4500 (want multiple of 9000)`.
    pub detail: String,
    pub severity: Severity,
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn parse_rule(v: &Value) -> Rule {
    let mut r = Rule::default();
    let Some(obj) = v.as_object() else { return r };
    for (k, v) in obj {
        match k.as_str() {
            "eq" => r.clauses.extend(num(v).map(Clause::Eq)),
            "ne" => r.clauses.extend(num(v).map(Clause::Ne)),
            "max" => r.clauses.extend(num(v).map(Clause::Max)),
            "multiple_of" => r.clauses.extend(num(v).map(Clause::MultipleOf)),
            "range" => {
                if let Some([a, b]) = v.as_array().map(Vec::as_slice) {
                    if let (Some(a), Some(b)) = (num(a), num(b)) {
                        r.clauses.push(Clause::Range(a, b));
                    }
                }
            }
            "group_not_all_zero" if v.as_bool() == Some(true) => {
                r.clauses.push(Clause::GroupNotAllZero)
            }
            "no_forward_any_port" => r.clauses.extend(parse_no_forward(v).map(Clause::NoForward)),
            "mount" => r.mount = v.as_str().map(str::to_owned),
            "else" => r.otherwise = Some(Box::new(parse_rule(v))),
            other => panic!("sensor-health.json: unknown rule kind {other:?}"),
        }
    }
    r
}

fn parse_no_forward(v: &Value) -> Option<NoForward> {
    let s = v.get("serial")?;
    let m = v.get("mav")?;
    let n = v.get("n")?.as_array()?;
    let mask = |v: &Value| v.get("mask")?.as_u64().and_then(|m| u32::try_from(m).ok());
    Some(NoForward {
        protocol_param: s.get("protocol_param")?.as_str()?.to_owned(),
        protocols: s
            .get("protocols")?
            .as_array()?
            .iter()
            .filter_map(num)
            .collect(),
        options_param: s.get("options_param")?.as_str()?.to_owned(),
        options_mask: mask(s)?,
        mav_param: m.get("options_param")?.as_str()?.to_owned(),
        mav_mask: mask(m)?,
        n: (
            u32::try_from(n.first()?.as_u64()?).ok()?,
            u32::try_from(n.get(1)?.as_u64()?).ok()?,
        ),
    })
}

fn parse_row(v: &Value) -> Row {
    let name = v["name"].as_str().expect("param row name").to_owned();
    let severity = match v["severity"].as_str() {
        Some("block") => Severity::Block,
        _ => Severity::Warn,
    };
    let when = v
        .get("when")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, r)| (k.clone(), parse_rule(r))).collect())
        .unwrap_or_default();
    Row {
        name,
        group: v.get("group").and_then(Value::as_str).map(str::to_owned),
        rule: parse_rule(&v["rule"]),
        when,
        severity,
    }
}

static ROWS: LazyLock<Vec<Row>> = LazyLock::new(|| spec().params.iter().map(parse_row).collect());

/// The parameter table.
pub fn rows() -> &'static [Row] {
    &ROWS
}

/// The EKF source parameters that say whether it fuses flow.
pub const EKF_VELXY: [&str; 3] = ["EK3_SRC1_VELXY", "EK3_SRC2_VELXY", "EK3_SRC3_VELXY"];

/// Every parameter the rules (and the readiness EKF check) read.
pub fn param_names() -> &'static [String] {
    &NAMES
}

static NAMES: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut out: Vec<String> = EKF_VELXY.iter().map(|s| (*s).to_owned()).collect();
    for row in rows() {
        let nf = row.rule.clauses.iter().find_map(|c| match c {
            Clause::NoForward(nf) => Some(nf),
            _ => None,
        });
        match nf {
            Some(nf) => out.extend(nf.names()),
            None => out.push(row.name.clone()),
        }
        out.extend(row.when.iter().map(|(n, _)| n.clone()));
    }
    out.sort();
    out.dedup();
    out
});

/// The declared mount value for a `mount` key, with its tolerance.
fn mount_value(m: &FcSensors, key: &str) -> Option<(f64, f64)> {
    let tol = spec().mount.pos_tolerance_m;
    let mut parts = key.split('.');
    let pos = |p: Option<[f64; 3]>, i: Option<&str>| {
        let i: usize = i?.parse().ok()?;
        p?.get(i).map(|v| (*v, tol))
    };
    match (parts.next()?, parts.next()?) {
        ("flow", "yaw_cdeg") => m
            .flow
            .as_ref()?
            .orient_yaw_cdeg
            .map(|y| (f64::from(y), 0.5)),
        ("flow", "pos_m") => pos(m.flow.as_ref()?.pos_m, parts.next()),
        ("rangefinder", "pos_m") => pos(m.rangefinder.as_ref()?.pos_m, parts.next()),
        _ => None,
    }
}

/// The first failing clause's `want` text, `None` when all pass.
fn first_fail(clauses: &[Clause], v: f64) -> Option<String> {
    clauses.iter().find_map(|c| match *c {
        Clause::Eq(x) if (v - x).abs() > 1e-3 => Some(format!("want {x}")),
        Clause::Ne(x) if (v - x).abs() <= 1e-3 => Some(format!("want not {x}")),
        Clause::Range(a, b) if v < a || v > b => Some(format!("want {a} to {b}")),
        Clause::Max(m) if v > m => Some(format!("want at most {m}")),
        Clause::MultipleOf(m) if m != 0.0 && ((v / m).round() * m - v).abs() > 0.5 => {
            Some(format!("want multiple of {m}"))
        }
        _ => None,
    })
}

fn passes(rule: &Rule, v: f64) -> bool {
    first_fail(&rule.clauses, v).is_none()
}

fn no_forward_ok(nf: &NoForward, param: &dyn Fn(&str) -> Option<f64>) -> Option<bool> {
    let bit = |v: Option<f64>, mask: u32| v.is_some_and(|v| (v as i64 as u32) & mask != 0);
    let mut any = false;
    for n in nf.n.0..=nf.n.1 {
        let name = |p: &str| p.replace("{n}", &n.to_string());
        let proto = param(&name(&nf.protocol_param));
        let opts = param(&name(&nf.options_param));
        let mav = param(&name(&nf.mav_param));
        any |= proto.is_some() || opts.is_some() || mav.is_some();
        let mavlink = proto.is_some_and(|p| nf.protocols.iter().any(|x| (p - x).abs() < 1e-3));
        if mavlink && bit(opts, nf.options_mask) || bit(mav, nf.mav_mask) {
            return Some(true);
        }
    }
    any.then_some(false)
}

/// Every failing row, in table order, groups once at their first row.
pub fn check(param: impl Fn(&str) -> Option<f32>, mounts: &FcSensors) -> Vec<Failure> {
    let param = |n: &str| param(n).map(f64::from);
    let mut out = Vec::new();
    // Groups in table order: (name, all zero so far, severity, position in `out`).
    let mut groups: Vec<(&str, bool, Severity, usize)> = Vec::new();
    for row in rows() {
        let gated = row
            .when
            .iter()
            .all(|(n, r)| param(n).is_some_and(|v| passes(r, v)));
        if !gated {
            continue;
        }
        if let Some(nf) = row.rule.clauses.iter().find_map(|c| match c {
            Clause::NoForward(nf) => Some(nf),
            _ => None,
        }) {
            if no_forward_ok(nf, &param) == Some(false) {
                out.push(Failure {
                    detail: format!(
                        "{} no port with {} set, nor {} {} (want the flow port's forwarding off)",
                        nf.options_param.replace("{n}", "n"),
                        nf.options_mask,
                        nf.mav_param.replace("{n}", "n"),
                        nf.mav_mask
                    ),
                    severity: row.severity,
                });
            }
            continue;
        }
        let Some(v) = param(&row.name) else { continue };
        let declared = row
            .rule
            .mount
            .as_deref()
            .and_then(|k| mount_value(mounts, k));
        let rule = match (declared, &row.rule.otherwise) {
            (Some((want, tol)), _) => {
                if (v - want).abs() > tol {
                    out.push(Failure {
                        detail: format!("{} {} (want {want})", row.name, v as f32),
                        severity: row.severity,
                    });
                }
                continue;
            }
            (None, Some(e)) if row.rule.mount.is_some() => e.as_ref(),
            _ => &row.rule,
        };
        if rule.clauses.contains(&Clause::GroupNotAllZero) {
            if let Some(g) = row.group.as_deref() {
                let zero = v.abs() < 1e-6;
                match groups.iter_mut().find(|e| e.0 == g) {
                    Some(e) => e.1 &= zero,
                    None => groups.push((g, zero, row.severity, out.len())),
                }
            }
        }
        if let Some(want) = first_fail(&rule.clauses, v) {
            out.push(Failure {
                detail: format!("{} {} ({want})", row.name, v as f32),
                severity: row.severity,
            });
        }
    }
    // Each insert shifts the later positions by one.
    let zero = groups.into_iter().filter(|e| e.1);
    for (i, (g, _, severity, at)) in zero.enumerate() {
        let detail = format!("{g} all 0 (want the lever arm from the CG)");
        out.insert(at + i, Failure { detail, severity });
    }
    out
}

#[cfg(test)]
mod tests;
