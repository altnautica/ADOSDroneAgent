use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::cmd::{CmdOut, CmdRunner};

#[derive(Default)]
struct State {
    ipt_chain: bool,
    ipt_jump: bool,
    ipt_rules: Vec<String>,
    nft_rules: Vec<String>,
    qdisc: BTreeMap<String, u32>,
    calls: Vec<Vec<String>>,
}

#[derive(Default)]
pub(crate) struct FakeNet {
    state: Mutex<State>,
}

fn ok(stdout: impl Into<String>) -> CmdOut {
    CmdOut {
        rc: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

impl FakeNet {
    /// Oifs of the MASQUERADE rules in whichever backend chain holds them.
    pub(crate) fn nat_ifaces(&self) -> Vec<String> {
        let s = self.state.lock();
        let mut all = s.ipt_rules.clone();
        all.extend(s.nft_rules.iter().cloned());
        all
    }
    /// The tbf rate on `iface`, if shaped.
    pub(crate) fn tbf_rate(&self, iface: &str) -> Option<u32> {
        self.state.lock().qdisc.get(iface).copied()
    }
    /// Drop the qdisc behind the reconciler's back.
    pub(crate) fn clear_qdisc(&self, iface: &str) {
        self.state.lock().qdisc.remove(iface);
    }
    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.state.lock().calls.clone()
    }

    fn nft_listing(rules: &[String]) -> String {
        let mut body = String::from(
            "table ip ados_nat {\n\tchain postrouting {\n\t\ttype nat hook postrouting priority srcnat; policy accept;\n",
        );
        for r in rules {
            body.push_str(&format!("\t\toifname \"{r}\" masquerade\n"));
        }
        body.push_str("\t}\n}\n");
        body
    }

    fn handle(&self, argv: &[&str]) -> CmdOut {
        let mut s = self.state.lock();
        s.calls.push(argv.iter().map(|a| a.to_string()).collect());
        match argv {
            ["iptables", "-t", "nat", "-N", _] => {
                if s.ipt_chain {
                    return CmdOut::failed(1, "iptables: Chain already exists.");
                }
                s.ipt_chain = true;
                ok("")
            }
            ["iptables", "-t", "nat", "-C", "POSTROUTING", "-j", _] => {
                if s.ipt_jump {
                    ok("")
                } else {
                    CmdOut::failed(1, "")
                }
            }
            ["iptables", "-t", "nat", "-A", "POSTROUTING", "-j", _] => {
                s.ipt_jump = true;
                ok("")
            }
            ["iptables", "-t", "nat", "-S", chain] => {
                let mut out = format!("-N {chain}\n");
                for r in &s.ipt_rules {
                    out.push_str(&format!("-A {chain} -o {r} -j MASQUERADE\n"));
                }
                ok(out)
            }
            ["iptables", "-t", "nat", "-F", _] => {
                s.ipt_rules.clear();
                ok("")
            }
            ["iptables", "-t", "nat", "-A", _, "-o", iface, "-j", "MASQUERADE"] => {
                s.ipt_rules.push(iface.to_string());
                ok("")
            }
            ["iptables-save"] => ok("# rules\n"),
            ["nft", "list", "chain", ..] | ["nft", "list", "table", ..] => {
                ok(Self::nft_listing(&s.nft_rules))
            }
            ["nft", "flush", "chain", ..] => {
                s.nft_rules.clear();
                ok("")
            }
            ["nft", "add", "rule", .., "oifname", iface, "masquerade"] => {
                s.nft_rules.push(iface.to_string());
                ok("")
            }
            ["tc", "qdisc", "show", "dev", iface, "root"] => match s.qdisc.get(*iface) {
                Some(rate) => ok(format!(
                    "qdisc tbf 8001: root refcnt 2 rate {rate}Kbit burst 4Kb lat 400ms\n"
                )),
                None => ok("qdisc fq_codel 0: root refcnt 2 limit 10240p\n"),
            },
            ["tc", "qdisc", "del", "dev", iface, "root"] => {
                if s.qdisc.remove(*iface).is_some() {
                    ok("")
                } else {
                    CmdOut::failed(2, "Error: Cannot find specified qdisc on specified device.")
                }
            }
            ["tc", "qdisc", "add", "dev", iface, "root", "tbf", "rate", rate, ..] => {
                let kbps = rate.trim_end_matches("kbit").parse().unwrap();
                s.qdisc.insert(iface.to_string(), kbps);
                ok("")
            }
            _ => ok(""),
        }
    }
}

#[async_trait]
impl CmdRunner for FakeNet {
    async fn run(&self, argv: &[&str], _timeout: Duration) -> CmdOut {
        self.handle(argv)
    }
}
