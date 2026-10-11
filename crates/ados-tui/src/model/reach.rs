//! Reach links: which URLs a user can open from this node.

use super::*;

impl Dashboard {
    /// Every advertised link, grouped for the Links panel: Console (the setup
    /// webapp), Connect to GCS (the bare base address to add into Mission
    /// Control), Ground control (MAVLink tcp + ws), Video, Mission Control, and
    /// Remote. Each link is a full URL. The MAVLink and video endpoints, which
    /// the agent advertises on one best host, are projected onto every reachable
    /// LAN host so the operator sees both the mDNS name and the LAN IP. Loopback
    /// is dropped from a group once it has any routable link.
    pub fn reach_links(&self) -> Vec<LinkGroup> {
        let hosts = self.lan_hosts();
        let mut out: Vec<LinkGroup> = Vec::new();

        // Console: the setup webapp, already advertised per host.
        out.push(LinkGroup {
            title: "Console",
            desc: "Setup & dashboard in a browser, on this LAN.",
            rows: self.rows_for(&["setup"]),
        });

        // Connect to GCS: the bare base address (no /setup) to paste into
        // Mission Control's Add-a-Node card to pair this drone over the LAN.
        let gcs: Vec<LinkRow> = self
            .rows_for(&["setup"])
            .into_iter()
            .map(|r| LinkRow {
                url: base_url(&r.url),
                primary: r.primary,
                loopback: r.loopback,
            })
            .collect();
        out.push(LinkGroup {
            title: "Connect to GCS",
            desc: "Add this drone to Mission Control — paste one of these into the Add-a-Node card (LAN pairing).",
            rows: dedup_drop_loopback(gcs),
        });

        // Ground control: a tcp + ws endpoint on every reachable LAN host (both
        // are bound on 0.0.0.0), plus any advertised tunnel endpoint.
        let tcp_port = self
            .mavlink_tcp_url
            .as_deref()
            .and_then(port_of)
            .unwrap_or(5760);
        let ws_port = self
            .mavlink_ws_url
            .as_deref()
            .and_then(port_of)
            .unwrap_or(8765);
        let mut gc: Vec<LinkRow> = Vec::new();
        if self.mavlink_tcp_url.is_some() || self.mavlink_ws_url.is_some() {
            for h in &hosts {
                gc.push(LinkRow {
                    url: format!("tcp://{}:{tcp_port}", h.host),
                    primary: h.primary,
                    loopback: false,
                });
                gc.push(LinkRow {
                    url: format!("ws://{}:{ws_port}/", h.host),
                    primary: false,
                    loopback: false,
                });
            }
        }
        if let Some(pub_ws) = &self.mavlink_public_ws_url {
            gc.push(LinkRow {
                url: pub_ws.clone(),
                primary: false,
                loopback: false,
            });
        }
        gc.extend(self.rows_for(&["mavlink"])); // e.g. the tunnel MAVLink WS
        out.push(LinkGroup {
            title: "Ground control",
            desc: "Conventional MAVLink over TCP / WebSocket for a third-party GCS — tcp://…:5760 (QGC / Mission Planner desktop), ws://…:8765/ (browser GCS).",
            rows: dedup_drop_loopback(gc),
        });

        // Video: project the advertised viewer URL (or the WHEP viewer) onto
        // every LAN host, plus any advertised tunnel viewer.
        let mut vid: Vec<LinkRow> = Vec::new();
        if let Some(tmpl) = self
            .best_url(&["video"])
            .or_else(|| self.video_viewer.clone())
        {
            for h in &hosts {
                vid.push(LinkRow {
                    url: swap_host(&tmpl, &h.host),
                    primary: h.primary,
                    loopback: false,
                });
            }
        }
        vid.extend(self.rows_for(&["video"]));
        out.push(LinkGroup {
            title: "Video",
            desc: "Live camera in a browser (WHEP viewer).",
            rows: dedup_drop_loopback(vid),
        });

        // Mission Control + Remote: as advertised.
        out.push(LinkGroup {
            title: "Mission Control",
            desc: "Open Mission Control to fly this drone. Hosted at command.altnautica.com; a local or self-hosted build runs wherever you serve it (e.g. http://localhost:4000).",
            rows: self.rows_for(&["mission_control"]),
        });
        out.push(LinkGroup {
            title: "Remote",
            desc: "Reach this device from anywhere via the cloud tunnel.",
            rows: self.rows_for(&["cloud"]),
        });

        out.retain(|g| !g.rows.is_empty());
        out
    }

    /// The reachable LAN hosts (mDNS `.local` first, then LAN IPs) taken from
    /// the advertised access URLs. Loopback and non-LAN (tunnel) hosts are
    /// excluded — those ride their own advertised entries.
    pub(super) fn lan_hosts(&self) -> Vec<HostRef> {
        let mut sorted = self.access_urls.clone();
        sorted.sort_by_key(|item| reach_rank(&item.url));
        let mut out: Vec<HostRef> = Vec::new();
        for item in &sorted {
            if reach_rank(&item.url) > 1 {
                continue; // 0 = .local, 1 = LAN IP; skip tunnel/other/loopback
            }
            let host = url_host(&item.url);
            if host.is_empty() || out.iter().any(|h| h.host == host) {
                continue;
            }
            out.push(HostRef {
                host,
                primary: item.primary,
            });
        }
        out
    }

    /// The rows for the given kinds, ordered best-first, deduped, loopback
    /// dropped once a routable link exists.
    pub(super) fn rows_for(&self, kinds: &[&str]) -> Vec<LinkRow> {
        let mut sorted = self.access_urls.clone();
        sorted.sort_by_key(|item| reach_rank(&item.url));
        let rows = sorted
            .iter()
            .filter(|item| kinds.contains(&item.kind.as_str()) && !item.url.is_empty())
            .map(|item| LinkRow {
                url: item.url.clone(),
                primary: item.primary,
                loopback: is_loopback_url(&item.url),
            })
            .collect();
        dedup_drop_loopback(rows)
    }

    /// The best (most reachable, non-loopback) advertised URL of the given kinds.
    pub(super) fn best_url(&self, kinds: &[&str]) -> Option<String> {
        self.rows_for(kinds).into_iter().next().map(|r| r.url)
    }
}
