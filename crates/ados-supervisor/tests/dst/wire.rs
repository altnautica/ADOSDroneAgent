//! The bind tunnel and the upstream wire protocol, as the two socat processes
//! see them: the drone's `TCP4-LISTEN` handing its one connection to
//! `wfb_bind_server.sh`, the ground station's retrying `TCP4` connect handing
//! its connection to `wfb_bind_client.sh`. TCP is modelled per segment: lost
//! transmissions are resent after a doubling timeout, segments arrive in order,
//! and nothing crosses while either tunnel is down or the air is out. Every
//! segment that crosses counts on the receiving tunnel's packet counter.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ados_supervisor::bind::{BindRole, KEY_TRANSFER_TIMEOUT, UPSTREAM_DRONE_KEY};

use crate::world::{lock, Conn, File, Msg, Shared, World, DRONE, GS};

/// How often a blocked socket re-checks the world.
const POLL: Duration = Duration::from_millis(10);
/// The first TCP resend timeout.
const RTO: Duration = Duration::from_millis(200);
/// socat's connect interval on the ground station (`interval=1`).
const CONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// Queue `msg` from `from` on connection `id`, if `from` still has its socket.
pub fn send(w: &mut World, from: usize, id: u32, msg: Msg) {
    let to = 1 - from;
    let lat = w.faults.latency_s;
    let mut delay = w.draw(lat);
    let mut rto = RTO;
    for _ in 0..8 {
        if w.rng.unit() >= w.faults.loss {
            break;
        }
        delay += rto;
        rto *= 2;
    }
    let frames = if w.rng.unit() < w.faults.dup { 2 } else { 1 };
    let now = tokio::time::Instant::now();
    let name = w.rigs[from].name;
    let Some(conn) = w.conn.as_mut().filter(|c| c.id == id && c.open[from]) else {
        return;
    };
    let at = (now + delay).max(conn.last[to]);
    conn.last[to] = at;
    conn.queues[to].push_back((at, msg.clone(), frames));
    w.log(format!("{name} sends {}", label(&msg)));
}

fn label(msg: &Msg) -> &'static str {
    match msg {
        Msg::Version => "VERSION",
        Msg::VersionOk => "OK version",
        Msg::Bind(_) => "BIND",
        Msg::BindOk => "OK bind",
        Msg::Fin => "FIN",
    }
}

/// Wait for the next segment to `me` on connection `id`. Segments wait out a
/// down link (TCP keeps resending); a socket whose connection is gone waits
/// forever, as a `read` on a silent peer does.
async fn recv(w: &Shared, me: usize, id: u32) -> Msg {
    loop {
        {
            let mut g = lock(w);
            let up = g.link_up();
            let now = tokio::time::Instant::now();
            let due = g
                .conn
                .as_ref()
                .filter(|c| c.id == id)
                .and_then(|c| c.queues[me].front())
                .is_some_and(|(at, _, _)| *at <= now);
            if due && up {
                let (_, msg, frames) = g
                    .conn
                    .as_mut()
                    .and_then(|c| c.queues[me].pop_front())
                    .expect("a due segment");
                g.rigs[me].rx_packets += frames;
                let name = g.rigs[me].name;
                g.log(format!("{name} reads {}", label(&msg)));
                return msg;
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// One frame (a SYN, its answer) crossing to `to`, if the air allows.
fn frame_to(w: &mut World, to: usize) -> bool {
    if w.link_up() {
        w.rigs[to].rx_packets += 1;
        true
    } else {
        false
    }
}

/// Closes this end's socket when the socat process ends, however it ends: a
/// clean exit, a kill when its future is dropped, or the whole supervisor
/// dying. The kernel sends the peer a FIN on close.
struct Socket {
    w: Shared,
    me: usize,
    /// The connection this socket carries (0: none yet).
    conn: AtomicU32,
}

impl Drop for Socket {
    fn drop(&mut self) {
        let mut g = lock(&self.w);
        let me = self.me;
        let id = self.conn.load(Ordering::SeqCst);
        g.rigs[me].socats -= 1;
        g.rigs[me].listening = false;
        if g.conn.as_ref().is_some_and(|c| c.id == id && c.open[me]) {
            send(&mut g, me, id, Msg::Fin);
            if let Some(c) = g.conn.as_mut() {
                c.open[me] = false;
            }
        }
        let name = g.rigs[me].name;
        g.log(format!("{name} socat exits"));
    }
}

/// Run this rig's socat to exit: `(exit status, stderr)`.
pub async fn run_socat(w: &Shared, me: usize) -> (i32, Vec<u8>) {
    let role = {
        let mut g = lock(w);
        g.rigs[me].socats += 1;
        let name = g.rigs[me].name;
        g.log(format!("{name} socat starts"));
        g.rigs[me].role
    };
    let socket = Socket {
        w: w.clone(),
        me,
        conn: AtomicU32::new(0),
    };
    if role == BindRole::Drone {
        drone_server(w, &socket).await
    } else {
        gs_client(w, &socket).await
    }
}

/// `socat TCP4-LISTEN:5555 EXEC:wfb_bind_server.sh`: accept one connection,
/// answer VERSION and BIND, end at the client's FIN (`while read` ends).
async fn drone_server(w: &Shared, socket: &Socket) -> (i32, Vec<u8>) {
    lock(w).rigs[DRONE].listening = true;
    let id = loop {
        {
            let g = lock(w);
            if !g.rigs[DRONE].listening {
                if let Some(c) = g.conn.as_ref().filter(|c| c.open[DRONE]) {
                    break c.id;
                }
            }
        }
        tokio::time::sleep(POLL).await;
    };
    socket.conn.store(id, Ordering::SeqCst);
    loop {
        match recv(w, DRONE, id).await {
            Msg::Version => {
                let d = {
                    let mut g = lock(w);
                    let r = g.faults.version_s;
                    g.draw(r)
                };
                tokio::time::sleep(d).await;
                send(&mut lock(w), DRONE, id, Msg::VersionOk);
            }
            Msg::Bind(blob) => {
                let mut g = lock(w);
                let mtime = g.wall_now();
                g.rigs[DRONE]
                    .files
                    .insert(UPSTREAM_DRONE_KEY.to_string(), File { data: blob, mtime });
                g.log("drone server deposits /etc/drone.key");
                send(&mut g, DRONE, id, Msg::BindOk);
            }
            Msg::Fin => return (0, Vec::new()),
            Msg::VersionOk | Msg::BindOk => {}
        }
    }
}

/// `socat TCP4:<drone>:5555,retry=N,interval=1 EXEC:wfb_bind_client.sh`.
async fn gs_client(w: &Shared, socket: &Socket) -> (i32, Vec<u8>) {
    let retries = KEY_TRANSFER_TIMEOUT.as_secs() + 5;
    let mut id = None;
    for _ in 0..=retries {
        id = connect(w).await;
        if id.is_some() {
            break;
        }
        tokio::time::sleep(CONNECT_INTERVAL).await;
    }
    let Some(id) = id else {
        return (1, b"E connect: Connection refused".to_vec());
    };
    socket.conn.store(id, Ordering::SeqCst);
    send(&mut lock(w), GS, id, Msg::Version);
    if !matches!(recv(w, GS, id).await, Msg::VersionOk) {
        return (1, b"Unable to fetch version".to_vec());
    }
    {
        let mut g = lock(w);
        let blob = g.rigs[GS]
            .files
            .get(UPSTREAM_DRONE_KEY)
            .map(|f| f.data.clone())
            .unwrap_or_default();
        send(&mut g, GS, id, Msg::Bind(blob));
    }
    if !matches!(recv(w, GS, id).await, Msg::BindOk) {
        return (1, b"Bind failed".to_vec());
    }
    let mut g = lock(w);
    send(&mut g, GS, id, Msg::Fin);
    if let Some(c) = g.conn.as_mut().filter(|c| c.id == id) {
        c.open[GS] = false;
    }
    (0, Vec::new())
}

/// One connect attempt: a SYN out, a SYN-ACK or RST back. Accepting takes the
/// drone's one listener. A SYN into a dead link times the attempt out.
async fn connect(w: &Shared) -> Option<u32> {
    let d = {
        let mut g = lock(w);
        let r = g.faults.latency_s;
        g.draw(r)
    };
    tokio::time::sleep(d).await;
    let accepted = {
        let mut g = lock(w);
        if !frame_to(&mut g, DRONE) {
            Err(())
        } else if g.rigs[DRONE].listening {
            g.rigs[DRONE].listening = false;
            g.conns += 1;
            let id = g.conns;
            let now = tokio::time::Instant::now();
            g.conn = Some(Conn {
                id,
                queues: Default::default(),
                last: [now, now],
                open: [true, true],
            });
            g.log("drone accepts the connection");
            Ok(Some(id))
        } else {
            Ok(None)
        }
    };
    let Ok(accepted) = accepted else {
        tokio::time::sleep(CONNECT_INTERVAL).await;
        return None;
    };
    tokio::time::sleep(d).await;
    frame_to(&mut lock(w), GS);
    accepted
}
