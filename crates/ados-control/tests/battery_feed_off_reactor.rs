//! The battery feed keeps its blocking I/O off the reactor.
//!
//! `battery/task.rs` runs an async loop over a sync step that reads the config
//! file, the board dict and the INA226 gauge's I2C registers. Every one of
//! those calls blocks, so an `async fn` body there may reach them only inside a
//! `spawn_blocking` closure. Like `no_blocking_spawn_in_routes.rs`, this is a
//! boundary on the file's shape; the behavioural proof (a slow gauge never
//! stalls a single-threaded runtime) is in the task's own tests.

use std::path::Path;

const BATTERY_TASK: &str = "src/battery/task.rs";

/// The blocking entry points of the battery feed.
const BLOCKING_CALLS: &[&str] = &[
    "std::fs",
    ".step(",
    "step_gauge(",
    "resolve_gauge(",
    "bring_up(",
    "open_bus(",
    "read_reg(",
    "write_reg(",
    "device.read(",
];

/// The non-test code lines of `text`: everything before the first
/// `#[cfg(test)]`, without `//` comments.
fn code_lines(text: &str) -> Vec<&str> {
    let code = text.split("#[cfg(test)]").next().unwrap_or("");
    code.lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .filter(|line| !line.trim().is_empty())
        .collect()
}

/// The code of every `async fn` body in `text`, with each `spawn_blocking(…)`
/// call cut out, so what remains runs on the reactor.
fn async_code_outside_spawn_blocking(text: &str) -> String {
    let mut bodies = String::new();
    let mut inside = false;
    let mut opened = false;
    let mut depth: i64 = 0;
    for line in code_lines(text) {
        if !inside && line.contains("async fn ") {
            inside = true;
            opened = false;
            depth = 0;
        }
        if !inside {
            continue;
        }
        bodies.push_str(line);
        bodies.push('\n');
        depth += line.matches('{').count() as i64 - line.matches('}').count() as i64;
        opened |= line.contains('{');
        if opened && depth <= 0 {
            inside = false;
        }
    }
    let mut out = String::new();
    let mut rest = bodies.as_str();
    while let Some(at) = rest.find("spawn_blocking(") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "spawn_blocking(".len()..];
        let mut parens = 1;
        let mut end = after.len();
        for (i, c) in after.char_indices() {
            match c {
                '(' => parens += 1,
                ')' => {
                    parens -= 1;
                    if parens == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[test]
fn the_battery_feed_keeps_its_gauge_and_file_io_off_the_reactor() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(BATTERY_TASK);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{BATTERY_TASK} must exist to be scanned: {e}"));
    let reactor_code = async_code_outside_spawn_blocking(&text);
    assert!(
        reactor_code.contains("async fn run("),
        "{BATTERY_TASK}: the feed loop moved; point this scan at it"
    );
    let violations: Vec<&str> = reactor_code
        .lines()
        .filter(|line| BLOCKING_CALLS.iter().any(|c| line.contains(c)))
        .collect();
    assert!(
        violations.is_empty(),
        "{BATTERY_TASK}: blocking battery feed I/O (config file, board dict, \
         INA226 I2C) on the reactor. Run it inside spawn_blocking.\n  {}",
        violations.join("\n  ")
    );
}

/// The extractor is what this boundary rests on, so it is tested.
#[test]
fn the_async_body_scan_sees_direct_calls_and_skips_spawn_blocking() {
    let direct = "fn sync() { feed.step(); }\nasync fn run() {\n    let t = feed.step();\n}\n";
    let code = async_code_outside_spawn_blocking(direct);
    assert!(code.contains("feed.step()"), "{code}");
    assert!(!code.contains("fn sync"), "{code}");
    let wrapped = "async fn run() {\n    let s = tokio::task::spawn_blocking(move || {\n        let t = feed.step();\n        (feed, t)\n    })\n    .await;\n}\n";
    let code = async_code_outside_spawn_blocking(wrapped);
    assert!(!code.contains(".step("), "{code}");
    assert!(code.contains(".await"), "{code}");
    // Test modules and comments are not code.
    let noise = "// feed.step()\nasync fn run() {}\n#[cfg(test)]\nasync fn t() { feed.step(); }\n";
    assert!(!async_code_outside_spawn_blocking(noise).contains(".step("));
}
