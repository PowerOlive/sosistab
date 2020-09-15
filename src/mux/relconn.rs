use crate::*;
use async_channel::{Receiver, Sender};
use async_dup::Arc as DArc;
use async_dup::Mutex as DMutex;
use bipe::{BipeReader, BipeWriter};
use bytes::{Bytes, BytesMut};
use mux::structs::{Message, RelKind, Reorderer, Seqno};
use nonzero_ext::nonzero;
use rand::Rng;
use smol::prelude::*;
use std::{
    collections::BTreeSet,
    collections::VecDeque,
    pin::Pin,
    task::Context,
    task::Poll,
    time::{Duration, Instant},
};
mod bipe;
mod inflight;

const MSS: usize = 1024;
const MAX_WAIT_SECS: u64 = 60;

#[derive(Clone)]
pub struct RelConn {
    send_write: DArc<DMutex<BipeWriter>>,
    recv_read: DArc<DMutex<BipeReader>>,
}

impl RelConn {
    pub(crate) fn new(state: RelConnState, output: Sender<Message>) -> (Self, RelConnBack) {
        let (send_write, recv_write) = bipe::bipe(65536);
        let (send_read, recv_read) = bipe::bipe(10 * 1024 * 1024);
        let (send_wire_read, recv_wire_read) = async_channel::bounded(100);
        runtime::spawn(relconn_actor(
            state,
            recv_write,
            send_read,
            recv_wire_read,
            output,
        ))
        .detach();
        (
            RelConn {
                send_write: DArc::new(DMutex::new(send_write)),
                recv_read: DArc::new(DMutex::new(recv_read)),
            },
            RelConnBack { send_wire_read },
        )
    }

    // pub fn to_async_reader(&self) -> impl AsyncRead {
    //     self.recv_read.clone()
    // }

    // pub fn to_async_writer(&self) -> impl AsyncWrite {
    //     self.send_write.clone()
    // }

    // pub async fn send_raw_bytes(&self, bts: Bytes) -> std::io::Result<()> {
    //     if !bts.is_empty() {
    //         self.send_write
    //             .lock()
    //             .write(&bts)
    //             .await
    //             .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;
    //     }
    //     Ok(())
    // }

    // pub async fn recv_raw_bytes(&self) -> std::io::Result<Bytes> {
    //     let mut bts = BytesMut;
    //     self.recv_read
    //         .recv()
    //         .await
    //         .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))
    // }

    pub fn force_close(&mut self) {
        drop(self.send_write.close())
    }
}

impl AsyncRead for RelConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let recv_read = &mut self.recv_read;
        smol::pin!(recv_read);
        recv_read.poll_read(cx, buf)
    }
}

impl AsyncWrite for RelConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let send_write = &mut self.send_write;
        smol::pin!(send_write);
        send_write.poll_write(cx, buf)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let send_write = &mut self.send_write;
        smol::pin!(send_write);
        send_write.poll_close(cx)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let send_write = &mut self.send_write;
        smol::pin!(send_write);
        send_write.poll_flush(cx)
    }
}

pub(crate) enum RelConnState {
    SynReceived {
        stream_id: u16,
    },
    SynSent {
        stream_id: u16,
        tries: usize,
        result: Sender<()>,
    },
    SteadyState {
        stream_id: u16,
        conn_vars: Box<ConnVars>,
    },
    Reset {
        stream_id: u16,
        death: smol::Timer,
    },
}
use inflight::Inflight;
use RelConnState::*;

async fn relconn_actor(
    mut state: RelConnState,
    mut recv_write: BipeReader,
    mut send_read: BipeWriter,
    recv_wire_read: Receiver<Message>,
    send_wire_write: Sender<Message>,
) -> anyhow::Result<()> {
    let limiter = governor::RateLimiter::direct(
        governor::Quota::per_second(nonzero!(20000u32)).allow_burst(nonzero!(10u32)),
    );
    // match on our current state repeatedly
    #[derive(Debug, Clone)]
    enum Evt {
        Rto((Seqno, bool)),
        AckTimer,
        NewWrite(Bytes),
        NewPkt(Message),
    }

    let transmit = |msg| async {
        if send_wire_write.try_send(msg).is_err() {
            eprintln!("SEND WIRE OVERFLO!")
        }
    };
    let mut fragments: VecDeque<Bytes> = VecDeque::new();
    loop {
        state = match state {
            SynReceived { stream_id } => {
                log::trace!("C={} SynReceived, sending SYN-ACK", stream_id);
                // send a synack
                transmit(Message::Rel {
                    kind: RelKind::SynAck,
                    stream_id,
                    seqno: 0,
                    payload: Bytes::new(),
                })
                .await;
                SteadyState {
                    stream_id,
                    conn_vars: Box::new(ConnVars::default()),
                }
            }
            SynSent {
                stream_id,
                tries,
                result,
            } => {
                let wait_interval = 2u64.saturating_pow(tries as u32) / 2;
                log::trace!("C={} SynSent, tried {} times", stream_id, tries);
                if wait_interval > MAX_WAIT_SECS {
                    anyhow::bail!("timeout in SynSent");
                }
                let synack_evt = async {
                    loop {
                        match recv_wire_read.recv().await? {
                            Message::Rel { .. } => return Ok::<_, anyhow::Error>(true),
                            _ => continue,
                        }
                    }
                };
                let success = synack_evt
                    .or(async {
                        smol::Timer::after(Duration::from_secs(wait_interval as u64)).await;
                        Ok(false)
                    })
                    .await?;
                if success {
                    log::trace!("C={} SynSent got SYN-ACK", stream_id);
                    SteadyState {
                        stream_id,
                        conn_vars: Box::new(ConnVars::default()),
                    }
                } else {
                    log::trace!("C={} SynSent timed out", stream_id);
                    transmit(Message::Rel {
                        kind: RelKind::Syn,
                        stream_id,
                        seqno: 0,
                        payload: Bytes::new(),
                    })
                    .await;
                    SynSent {
                        stream_id,
                        tries: tries + 1,
                        result,
                    }
                }
            }
            SteadyState {
                stream_id,
                mut conn_vars,
            } => {
                let event = {
                    let writeable = conn_vars.inflight.inflight() <= conn_vars.cwnd as usize
                        && conn_vars.inflight.len() < 10000;
                    let force_ack = conn_vars.ack_seqnos.len() >= 128;

                    let ack_timer = conn_vars.delayed_ack_timer;
                    let ack_timer = async {
                        if force_ack {
                            return Ok(Evt::AckTimer);
                        }
                        if let Some(time) = ack_timer {
                            smol::Timer::at(time).await;
                            Ok::<Evt, anyhow::Error>(Evt::AckTimer)
                        } else {
                            smol::future::pending().await
                        }
                    };
                    let rto_timer = conn_vars.inflight.wait_first();
                    let rto_timeout = async { Ok::<Evt, anyhow::Error>(Evt::Rto(rto_timer.await)) };
                    let new_write = async {
                        if writeable {
                            if fragments.is_empty() {
                                let mut to_write = {
                                    let mut bts = BytesMut::with_capacity(1024);
                                    bts.extend_from_slice(&[0; 1024]);
                                    let n = recv_write.read(&mut bts).await?;
                                    let bts = bts.freeze();
                                    bts.slice(0..n)
                                };
                                while to_write.len() > MSS {
                                    fragments.push_back(to_write.slice(0..MSS));
                                    to_write = to_write.slice(MSS..);
                                }
                                fragments.push_back(to_write);
                            }
                            // limiter.until_ready().await;
                            Ok::<Evt, anyhow::Error>(Evt::NewWrite(fragments.pop_front().unwrap()))
                        } else {
                            Ok(smol::future::pending().await)
                        }
                    };
                    let new_pkt = async {
                        Ok::<Evt, anyhow::Error>(Evt::NewPkt(recv_wire_read.recv().await?))
                    };
                    ack_timer.or(rto_timeout.or(new_write.or(new_pkt))).await
                };
                match event {
                    Ok(Evt::Rto((seqno, is_timeout))) => {
                        // retransmit packet
                        // assert!(!conn_vars.inflight.len() == 0);
                        if conn_vars.inflight.len() > 0 {
                            if let Some(v) = conn_vars.inflight.get_seqno(seqno) {
                                let payload = v.payload.clone();
                                if v.retrans == 1 {
                                    if is_timeout {
                                        conn_vars.congestion_rto()
                                    } else {
                                        conn_vars.congestion_fast();
                                    }
                                }
                                conn_vars.retrans_count += 1;
                                // eprintln!(
                                //     "{}/{} retrans",
                                //     conn_vars.retrans_count, conn_vars.next_free_seqno
                                // );
                                transmit(payload).await;
                            }
                        }
                        // new state
                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    Ok(Evt::NewPkt(Message::Rel {
                        kind: RelKind::Rst,
                        stream_id,
                        ..
                    })) => Reset {
                        stream_id,
                        death: smol::Timer::after(Duration::from_secs(60)),
                    },
                    Ok(Evt::NewPkt(Message::Rel {
                        kind: RelKind::DataAck,
                        payload,
                        seqno,
                        ..
                    })) => {
                        log::trace!("new ACK pkt with {} seqnos", payload.len() / 2);
                        for seqno in
                            bincode::deserialize::<BTreeSet<Seqno>>(&payload).unwrap_or_default()
                        {
                            if conn_vars.inflight.mark_acked(seqno) {
                                conn_vars.congestion_ack();
                            }
                        }
                        conn_vars.inflight.mark_acked_lt(seqno);
                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    Ok(Evt::NewPkt(Message::Rel {
                        kind: RelKind::Data,
                        seqno,
                        payload,
                        stream_id,
                    })) => {
                        log::trace!("new data pkt with seqno={}", seqno);
                        conn_vars.ack_seqnos.insert(seqno);
                        if conn_vars.delayed_ack_timer.is_none() {
                            conn_vars.delayed_ack_timer =
                                Instant::now().checked_add(Duration::from_millis(5));
                        }
                        conn_vars.reorderer.insert(seqno, payload);
                        let times = conn_vars.reorderer.take();
                        conn_vars.lowest_unseen += times.len() as u64;
                        for pkt in times {
                            send_read.write(&pkt).await?;
                        }
                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    Ok(Evt::NewWrite(bts)) => {
                        assert!(bts.len() <= MSS);
                        let seqno = conn_vars.next_free_seqno;
                        conn_vars.next_free_seqno += 1;
                        let msg = Message::Rel {
                            kind: RelKind::Data,
                            stream_id,
                            seqno,
                            payload: bts,
                        };
                        // put msg into inflight
                        conn_vars.inflight.insert(seqno, msg.clone());

                        transmit(msg).await;

                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    Ok(Evt::AckTimer) => {
                        // eprintln!("acking {} seqnos", conn_vars.ack_seqnos.len());
                        let encoded_acks = bincode::serialize(&conn_vars.ack_seqnos).unwrap();
                        transmit(Message::Rel {
                            kind: RelKind::DataAck,
                            stream_id,
                            seqno: conn_vars.lowest_unseen,
                            payload: Bytes::copy_from_slice(&encoded_acks),
                        })
                        .await;
                        conn_vars.ack_seqnos.clear();
                        conn_vars.delayed_ack_timer = None;
                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    err => {
                        log::trace!("forced to RESET due to {:?}", err);
                        Reset {
                            stream_id,
                            death: smol::Timer::after(Duration::from_secs(MAX_WAIT_SECS)),
                        }
                    }
                }
            }
            Reset {
                stream_id,
                mut death,
            } => {
                send_read.close().await;
                log::trace!("C={} RESET", stream_id);
                transmit(Message::Rel {
                    kind: RelKind::Rst,
                    stream_id,
                    seqno: 0,
                    payload: Bytes::new(),
                })
                .await;
                let die = smol::future::race(
                    async {
                        (&mut death).await;
                        true
                    },
                    async {
                        if recv_wire_read.recv().await.is_ok() {
                            true
                        } else {
                            smol::future::pending().await
                        }
                    },
                )
                .await;
                if die {
                    anyhow::bail!("60 seconds in reset up")
                }
                Reset { stream_id, death }
            }
        }
    }
}

pub(crate) struct RelConnBack {
    send_wire_read: Sender<Message>,
}

impl RelConnBack {
    pub async fn process(&self, input: Message) {
        drop(self.send_wire_read.send(input).await)
    }
}

pub(crate) struct ConnVars {
    inflight: Inflight,
    next_free_seqno: Seqno,
    retrans_count: u64,

    delayed_ack_timer: Option<Instant>,
    ack_seqnos: BTreeSet<Seqno>,

    reorderer: Reorderer<Bytes>,
    lowest_unseen: Seqno,
    // read_buffer: VecDeque<Bytes>,
    slow_start: bool,
    ssthresh: f64,
    cwnd: f64,
    cwnd_max: f64,
    last_loss: Instant,

    cubic_secs: f64,
    last_cubic: Instant,
}

impl Default for ConnVars {
    fn default() -> Self {
        ConnVars {
            inflight: Inflight::default(),
            next_free_seqno: 0,
            retrans_count: 0,

            delayed_ack_timer: None,
            ack_seqnos: BTreeSet::new(),

            reorderer: Reorderer::default(),
            lowest_unseen: 0,

            slow_start: true,
            ssthresh: 10000.0,
            cwnd: 16.0,
            cwnd_max: 100.0,
            last_loss: Instant::now(),
            cubic_secs: 0.0,
            last_cubic: Instant::now(),
        }
    }
}

impl ConnVars {
    fn congestion_ack(&mut self) {
        if self.slow_start {
            self.cwnd += 1.0;
            if self.cwnd > self.ssthresh {
                self.slow_start = false;
                // self.cwnd_max = self.cwnd / 0.8;
                // let now = Instant::now();
                // self.last_loss = now;
                // self.cubic_secs = 0.0;
                // self.cubic_update(now);
            }
        } else {
            // self.cubic_update(Instant::now());
            let n = (0.23 * self.cwnd.powf(0.4)).max(1.0) * 4.0;
            // dbg!(n);
            self.cwnd = (self.cwnd + n / self.cwnd).min(10000.0);
        }
        // eprintln!("ACK CWND => {}", self.cwnd);
    }

    fn congestion_fast(&mut self) {
        if Instant::now().saturating_duration_since(self.last_loss) > self.inflight.rto() * 2 {
            self.slow_start = false;
            self.cwnd = self.inflight.bdp().max(self.cwnd * 0.9);
            self.last_loss = Instant::now();
            // self.cwnd_max = self.cwnd;
            // let now = Instant::now();
            // self.last_loss = now;
            // self.cubic_secs = 0.0;
            // self.cubic_update(now);
            eprintln!(
                "LOSS CWND => {} (ssthresh {}) bdp {} rto {}ms",
                self.cwnd,
                self.ssthresh,
                self.inflight.bdp(),
                self.inflight.rto().as_millis()
            );
        }
    }

    fn congestion_rto(&mut self) {
        if Instant::now().saturating_duration_since(self.last_loss) > self.inflight.rto() * 2 {
            let now = Instant::now();
            self.last_loss = now;
            // self.slow_start = false;
            // self.cwnd_max = self.cwnd;
            // self.last_cubic = now;
            // self.cubic_secs = 0.0;
            // self.cubic_update(now);
            self.ssthresh = self.cwnd * 0.9;
            self.cwnd = 1.0;
            self.slow_start = true;
            eprintln!(
                "RTO CWND => {} (ssthresh {}) bdp {} rto {}ms",
                self.cwnd,
                self.ssthresh,
                self.inflight.bdp(),
                self.inflight.rto().as_millis()
            );
        }
    }

    fn cubic_update(&mut self, now: Instant) {
        let delta_t = now
            .saturating_duration_since(self.last_cubic)
            .as_secs_f64()
            .min(0.1);
        self.last_cubic = now;
        self.cubic_secs += delta_t;
        let t = self.cubic_secs;
        let k = (self.cwnd_max / 2.0).powf(0.333);
        let wt = 0.4 * (t - k).powf(3.0) + self.cwnd_max;
        let new_cwnd = wt.min(10000.0);
        self.cwnd = new_cwnd;
        // self.cwnd = 300.0;
    }
}
