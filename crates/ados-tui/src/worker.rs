//! Write requests off the render thread.
//!
//! Every write (and every read that is not a timed poll) is a [`Request`]
//! handed to a small pool of worker threads. At most [`MAX_IN_FLIGHT`] run at
//! once; the rest queue. Replies come back over the app's message channel
//! tagged with the [`Route`] that asked, so the screen that started a request
//! is the one that handles its answer.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ados_protocol::rest::{RestClient, RestError};
use serde_json::Value;

use crate::app::TabId;

/// Requests that run concurrently.
pub const MAX_IN_FLIGHT: usize = 4;
/// Default request timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);
/// A Wi-Fi scan can take 15 s on the agent.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(20);
/// A plugin download + verify + install.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(180);
/// A service restart: the agent bounds `systemctl restart` at 30 s and then
/// confirms the unit for up to 5 s more before it answers.
pub const RESTART_TIMEOUT: Duration = Duration::from_secs(40);

/// Which screen handles a reply (and an overlay result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub tab: TabId,
    /// A settings page id, for replies a settings page handles.
    pub page: Option<&'static str>,
}

impl Route {
    pub const fn tab(tab: TabId) -> Self {
        Self { tab, page: None }
    }
    pub const fn page(page: &'static str) -> Self {
        Self {
            tab: TabId::Settings,
            page: Some(page),
        }
    }
}

/// How the app handles a reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Purpose {
    /// `PUT /api/config` of one key; `row` names the row to flash.
    ConfigSet { key: String, row: String },
    /// `POST /api/v1/setup/apply` of one section.
    SetupApply { section: &'static str },
    /// Sequential parameter writes, one `Progress` message per item.
    ParamBatch(Vec<(String, f64)>),
    WifiScan,
    WifiJoin { ssid: String, forced: bool },
    McpMint,
    PluginParse,
    PluginInstall,
    /// A plain action: success toasts `ok_toast`, failure toasts the error.
    Generic { ok_toast: String },
    /// Anything else a screen tracks itself: `tag` names it, `data` carries
    /// whatever the screen needs back.
    Page { tag: &'static str, data: Value },
}

/// One request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: &'static str,
    pub path: String,
    pub body: Option<Value>,
    pub timeout: Duration,
    pub purpose: Purpose,
    pub route: Route,
}

impl Request {
    /// A sequential batch of parameter writes. The worker posts each item to
    /// its own `/api/params/{name}` route, so the batch itself names no path.
    pub fn param_batch(route: Route, items: Vec<(String, f64)>) -> Self {
        Self::new("POST", String::new(), route, Purpose::ParamBatch(items))
    }

    pub fn new(method: &'static str, path: impl Into<String>, route: Route, purpose: Purpose) -> Self {
        Self {
            method,
            path: path.into(),
            body: None,
            timeout: DEFAULT_TIMEOUT,
            purpose,
            route,
        }
    }
    pub fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// A finished request.
#[derive(Debug)]
pub struct Reply {
    pub route: Route,
    pub purpose: Purpose,
    pub result: Result<Value, RestError>,
}

/// What the worker pool sends back.
#[derive(Debug)]
pub enum WorkerMsg {
    /// One item of a parameter batch finished.
    Progress {
        route: Route,
        name: String,
        value: f64,
        result: Result<Value, RestError>,
    },
    Done(Reply),
}

/// The worker pool. Dropping it lets the threads exit after their current
/// request.
pub struct Worker {
    jobs: Sender<Request>,
    replies: Receiver<WorkerMsg>,
}

impl Worker {
    pub fn spawn(client: RestClient) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Request>();
        let (reply_tx, replies) = mpsc::channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..MAX_IN_FLIGHT {
            let job_rx = Arc::clone(&job_rx);
            let reply_tx = reply_tx.clone();
            let client = client.clone();
            thread::spawn(move || loop {
                let job = {
                    let Ok(rx) = job_rx.lock() else { return };
                    match rx.recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    }
                };
                if run(&client, job, &reply_tx).is_err() {
                    return;
                }
            });
        }
        Self { jobs, replies }
    }

    /// Queue a request.
    pub fn submit(&self, req: Request) {
        let _ = self.jobs.send(req);
    }

    /// Every message that landed since the last call. Never blocks.
    pub fn drain(&self) -> impl Iterator<Item = WorkerMsg> + '_ {
        self.replies.try_iter()
    }
}

/// Run one job; `Err` when the app has gone away.
fn run(client: &RestClient, job: Request, tx: &Sender<WorkerMsg>) -> Result<(), ()> {
    if let Purpose::ParamBatch(items) = &job.purpose {
        // Sequential: each write is acknowledged (or not) before the next goes
        // out, so the FC never sees a burst of PARAM_SETs.
        for (name, value) in items {
            let path = format!("/api/params/{}", crate::api_path::seg(name));
            let body = serde_json::json!({ "value": value });
            let result = client.send_json("POST", &path, Some(&body), job.timeout);
            tx.send(WorkerMsg::Progress {
                route: job.route,
                name: name.clone(),
                value: *value,
                result,
            })
            .map_err(|_| ())?;
        }
        return tx
            .send(WorkerMsg::Done(Reply {
                route: job.route,
                purpose: job.purpose,
                result: Ok(Value::Null),
            }))
            .map_err(|_| ());
    }
    let result = client.send_json(job.method, &job.path, job.body.as_ref(), job.timeout);
    tx.send(WorkerMsg::Done(Reply {
        route: job.route,
        purpose: job.purpose,
        result,
    }))
    .map_err(|_| ())
}
