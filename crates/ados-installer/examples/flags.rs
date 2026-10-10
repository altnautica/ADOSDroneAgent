//! Dump the installer flag table as JSON, for `scripts/emit-reference.py`.
//!
//! ```text
//! cargo run --quiet -p ados-installer --example flags
//! ```

use ados_installer::cli::args::FLAGS;
use serde_json::json;

fn main() {
    let flags: Vec<_> = FLAGS
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "short": f.short,
                "takes_value": f.takes_value,
                "value_name": f.value_name,
                "help": f.help.lines().map(str::trim).collect::<Vec<_>>().join(" "),
            })
        })
        .collect();
    println!("{}", json!({ "schema": 1, "flags": flags }));
}
