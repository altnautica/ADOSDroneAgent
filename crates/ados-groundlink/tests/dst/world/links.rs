//! The world's channels: the LAN with its losses, duplicates and the
//! eavesdropper's replays, and the ground radio link the beacons cross.

use std::sync::atomic::Ordering;

use ados_radio::hop::{build_presence_beacon, parse_presence_beacon};
use tokio::sync::oneshot;

use super::{World, BEACON_MS};
use crate::boxside::BOX_ID;
use crate::net::{Event, Gid, Signed, Via};
use crate::phone::{Act, Phase};

impl World {
    pub(super) fn hear(&mut self, from: Gid, to: Gid, frame: &[u8; 68]) {
        let now = self.now();
        match to {
            Gid::Box => {
                if self.bx.hear(frame, &self.pair_key) == Some(true) {
                    self.ledger.holder_beacon(now);
                }
            }
            Gid::Phone(i) => {
                let Some(peer) = parse_presence_beacon(frame, &self.pair_key) else {
                    return;
                };
                if self.phones[i].phase != Phase::Dead && peer.role == "gs" {
                    let was = self.phones[i].tx_up();
                    self.phones[i].on_beacon(&peer.device_id, now, &mut self.rng);
                    if was && matches!(self.phones[i].phase, Phase::Stopping { .. }) {
                        self.note(&format!("phone{i} heard {from} and yields"));
                    }
                }
            }
        }
    }

    pub(super) fn phone_acts(&mut self, phone: usize, acts: Vec<Act>, boot: u32) {
        for a in acts {
            match a {
                Act::Request(req) => self.lan_request(phone, req),
                Act::Answer { ok, answer } => self.lan_answer(ok, answer, boot),
            }
        }
    }

    pub(super) fn step_phones(&mut self) {
        let (now, wall) = (self.now(), self.wall());
        for i in 0..self.phones.len() {
            let acts = self.phones[i].step(now, wall, &mut self.next_msg);
            self.phone_acts(i, acts, self.boot);
        }
    }

    /// Every ground on air beacons every 10 s, each on its own phase.
    pub(super) fn beacons(&mut self) {
        let now = self.now();
        if now >= self.box_beacon {
            self.box_beacon += BEACON_MS;
            if self.bx.air.load(Ordering::SeqCst) {
                self.emit(Gid::Box, BOX_ID.to_string());
            }
        }
        for i in 0..self.phones.len() {
            if now >= self.phones[i].next_beacon {
                self.phones[i].next_beacon += BEACON_MS;
                if self.phones[i].tx_up() {
                    self.emit(Gid::Phone(i), self.phones[i].device_id.clone());
                }
            }
        }
    }

    fn emit(&mut self, from: Gid, device_id: String) {
        let frame = build_presence_beacon(
            &device_id,
            false,
            149,
            0,
            self.wall() as u64,
            &self.pair_key,
        );
        let grounds = std::iter::once(Gid::Box).chain((0..self.phones.len()).map(Gid::Phone));
        for to in grounds.collect::<Vec<_>>() {
            let faded =
                matches!((from, to), (Gid::Phone(i), Gid::Box) if self.now() < self.fade_until[i]);
            if to == from || faded || self.lost(self.faults.radio_loss) {
                continue;
            }
            let at = self.now() + self.rng.ms(2, 30);
            self.q.push(at, Event::Beacon { from, to, frame });
        }
    }

    pub(super) fn lan_delay(&mut self) -> u64 {
        self.now() + self.rng.ms(5, 300)
    }

    /// Lost, unless the tail is calm.
    pub(super) fn lost(&mut self, p: f64) -> bool {
        !self.calm && self.rng.chance(p)
    }

    pub(super) fn lan_request(&mut self, phone: usize, req: Signed) {
        self.captured.push((None, req.clone()));
        if self.lost(self.faults.lan_loss) {
            return self.note(&format!("lan lost request {}", req.msg));
        }
        if self.lost(self.faults.lan_dup) {
            let at = self.lan_delay() + self.rng.ms(50, 2_000);
            let (from, via) = (Some(phone), Via::Duplicate);
            self.q.push(
                at,
                Event::AtBox {
                    from,
                    via,
                    req: req.clone(),
                },
            );
        }
        let at = self.lan_delay();
        let (from, via) = (Some(phone), Via::Original);
        self.q.push(at, Event::AtBox { from, via, req });
    }

    pub(super) fn lan_yield(&mut self, phone: usize, req: Signed, answer: oneshot::Sender<bool>) {
        self.captured.push((Some(phone), req.clone()));
        if self.lost(self.faults.lan_loss) {
            self.note(&format!("lan lost take-back call {}", req.msg));
            return self.limbo.push(answer);
        }
        let boot = self.boot;
        if self.lost(self.faults.lan_dup) {
            let at = self.lan_delay() + self.rng.ms(50, 2_000);
            let (via, answer) = (Via::Duplicate, None);
            self.q.push(
                at,
                Event::AtPhone {
                    phone,
                    via,
                    req: req.clone(),
                    answer,
                    boot,
                },
            );
        }
        let at = self.lan_delay();
        let (via, answer) = (Via::Original, Some(answer));
        self.q.push(
            at,
            Event::AtPhone {
                phone,
                via,
                req,
                answer,
                boot,
            },
        );
    }

    fn lan_answer(&mut self, ok: bool, answer: Option<oneshot::Sender<bool>>, boot: u32) {
        let Some(answer) = answer else { return };
        if self.lost(self.faults.lan_loss) {
            self.note("lan lost a take-back answer");
            return self.limbo.push(answer);
        }
        let at = self.lan_delay();
        self.q.push(at, Event::YieldAnswer { ok, answer, boot });
    }

    /// The eavesdropper sends a captured message again: mostly where it went,
    /// sometimes to the other end.
    pub(super) fn replay(&mut self) {
        let k = self.rng.next_u64() as usize % self.captured.len();
        let (target, req) = self.captured[k].clone();
        let cross = self.rng.chance(0.3);
        let at = self.lan_delay();
        self.note(&format!("adversary replays {} cross={cross}", req.msg));
        let (via, answer, boot) = (Via::Replay, None, self.boot);
        match (target, cross) {
            (Some(phone), false) => self.q.push(
                at,
                Event::AtPhone {
                    phone,
                    via,
                    req,
                    answer,
                    boot,
                },
            ),
            (None, true) => {
                let phone = self.rng.next_u64() as usize % self.phones.len();
                self.q.push(
                    at,
                    Event::AtPhone {
                        phone,
                        via,
                        req,
                        answer,
                        boot,
                    },
                );
            }
            _ => self.q.push(
                at,
                Event::AtBox {
                    from: None,
                    via,
                    req,
                },
            ),
        }
    }
}
