//! The itlawdl kext as an [`Air`]: frames tapped off its cell, injected
//! back through `debug.itlawdl_tx`.
//!
//! RX records, `debug.itlawdl_rx`: a 16-byte header then the raw 802.11
//! frame without FCS — u16 len (LE), u8 rate (500 kbit/s, or 0x80|mcs),
//! i8 rssi, u8 flags, u8 chan, u16 rsv, u64 tsft (us) — in runs of at
//! most 64 KiB.
//!
//! A sysctl has no readiness an async runtime could wait on, so v0.1.1
//! polled the drain between the engine's own timers, on the engine's
//! thread — where a syscall the kernel takes its time with freezes the
//! timers, the peers, everything. The first field run went exactly like
//! that: one action frame out, then silence, and Ctrl-C never got the
//! thread back. The drain now belongs to a thread of its own, which
//! parses the records and hands the frames to the engine over a
//! channel: a stalled drain can no longer take the engine down, and the
//! reader's heartbeat says which side is stuck. The channel holds what
//! a busy minute of AWDL brings; a full one parks the reader, and the
//! kext's ring counts the rest in its `dropped`.
//!
//! v0.1.3, two more lessons from the second field run:
//! - The reader takes only records of a whole frame's size (24 to 2384
//!   bytes). A kext v2.2 drain can hand out a tail cut mid-record; the
//!   reader then walks the ring's bytes out of step and "parses"
//!   lengths off frame data — "frames" tens of kilobytes long that no
//!   radio ever sent. They never reach the engine now; the heartbeat
//!   counts them as rejects.
//! - The inject write left the engine's thread too (the same medicine,
//!   ten lines later): the engine hands its frames to a writer thread
//!   over a channel and never waits on the kernel. A full queue drops
//!   the frame — the kext would have counted it in `inject_err`
//!   anyway, and the engine has no queue-full signal to act on.
//!
//! v0.1.6: the v05/v06 field runs both passed the 12 s listening check
//! and then watched rx stand still from the engine's first frames out —
//! 24 a second, from zero, with no way to tell which of the two killed
//! the radio. Every frame now passes a ramp ([`Gate`]): 1, 4, 12 a
//! second, ten seconds each, the kext's rx counter read at every
//! boundary, the ceiling that survived kept for the rest of the session.
//!
//! TX requests, `debug.itlawdl_tx`: an 8-byte header then the frame —
//! u8 ver=2, u8 flags (bit0 expect ack, bit1 ht mcs), u8 rate (same
//! coding as RX), u8 pad, u16 frame_len (LE), u16 rsv. A write the kext
//! takes always succeeds: one it refuses is counted in its stats, the
//! radio's to log, not to retry.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use owly_awdl::driver::{Air, Received};
use owly_awdl::frame::Frame;
use owly_awdl::TxRate;

use tokio::sync::mpsc;

use crate::sysctl;

const RX_SYSCTL: &str = "debug.itlawdl_rx";
const TX_SYSCTL: &str = "debug.itlawdl_tx";

/// The kext's drain buffer size; one read takes at most this much.
const DRAIN: usize = 64 * 1024;
/// The header on every RX record.
const RXREC_HDR: usize = 16;
/// The largest frame the ring holds (the kext's AWDL_SESSION_MAX_FRAME):
/// anything longer is a desynced length, not a frame.
const MAX_RECORD: usize = 2304 + 64;
/// How long the reader waits between reads that held nothing.
const EMPTY_POLL: Duration = Duration::from_millis(2);
/// How often the reader says it is alive.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// How many frames may wait for the engine at once.
const CHANNEL: usize = 1024;
/// How many injection requests may wait for the writer at once.
const TX_CHANNEL: usize = 256;

/// The injection ladder: (frames a second the gate lets by, seconds the
/// step lasts). The last one stays forever. The v06 field run froze rx
/// at the very moment the engine's first frames went out — 24 a second
/// from zero — and only a staged climb can say which of the two it was:
/// one frame is poison (the kext's inject path itself, its queue to
/// change), or twenty-four a second choke the firmware (a pace to hold).
const RAMP: [(u64, u64); 4] = [(1, 10), (4, 10), (12, 10), (u64::MAX, 0)];
/// A step's rx must grow by at least this to count as hearing: beacons
/// alone bring ~25 in ten seconds on channel 6, a deaf radio exactly 0.
const STILL_ALIVE: u64 = 2;

/// The gate every frame out has to pass: one ceiling at a time, each
/// step judged by the kext's own rx counter before the next opens. A
/// step that leaves rx standing still closes the ladder at the last pace
/// the radio survived — and says so — so the session stays a useful one
/// (peers, mDNS in) at whatever the firmware tolerates.
pub struct Gate {
    ladder: Mutex<Ladder>,
    /// Frames let through and eaten, since the engine started.
    allowed: AtomicU64,
    eaten: AtomicU64,
}

struct Ladder {
    step: usize,
    since: Instant,
    rx_at_start: Option<u64>,
    /// The 1 s budget window of the current ceiling.
    window: Instant,
    budget: u64,
    /// Set once a step killed the radio: the ceiling to keep, 0 to stay
    /// silent.
    held: Option<u64>,
    /// One line of state for the monitor's ticker.
    state: String,
}

impl Gate {
    pub fn new() -> Gate {
        let rx = sysctl::radio_rx();
        Gate {
            ladder: Mutex::new(Ladder {
                step: 0,
                since: Instant::now(),
                rx_at_start: rx,
                window: Instant::now(),
                budget: 0,
                held: None,
                state: "palier 1 tr/s".into(),
            }),
            allowed: AtomicU64::new(0),
            eaten: AtomicU64::new(0),
        }
    }

    /// Whether this frame may go out now. Called from the engine's
    /// thread for every frame it builds — including the ones it eats.
    pub fn admit(&self, now: Instant) -> bool {
        let mut l = self.ladder.lock().unwrap();
        if now.saturating_duration_since(l.window) >= Duration::from_secs(1) {
            l.window = now;
            l.budget = 0;
        }
        if l.held.is_none() && RAMP[l.step].1 > 0
            && now.saturating_duration_since(l.since) >= Duration::from_secs(RAMP[l.step].1)
        {
            let rx = sysctl::radio_rx();
            let grew = match (rx, l.rx_at_start) {
                (Some(now), Some(was)) => now.saturating_sub(was) >= STILL_ALIVE,
                // The stats went away: the kext (or its session) is in no
                // state to judge anything — climb on, the engine's own
                // failure channel will say why.
                _ => true,
            };
            if !grew {
                let last = if l.step == 0 { 0 } else { RAMP[l.step - 1].0 };
                l.held = Some(last);
                let said = if last == 0 {
                    l.state = "émission coupée (la radio meurt à la première trame)".into();
                    "== porte TX : la radio n'a plus rien reçu dès 1 trame/s — la première injection \
                     l'assourdit. J'arrête d'émettre pour cette session ; colle-moi ce log : le \
                     prochain pas est côté kext (file TVQM)"
                        .to_string()
                } else {
                    l.state =
                        format!("tenue à {last} tr/s (la radio est morte à {})", RAMP[l.step].0);
                    format!(
                        "== porte TX : la radio n'a plus rien reçu à {} tr/s — je m'arrête à \
                         {last} tr/s pour la suite de la session (la réception et les pairs \
                         continuent)",
                        RAMP[l.step].0
                    )
                };
                println!("{said}");
            } else {
                let delta = match (rx, l.rx_at_start) {
                    (Some(now), Some(was)) => format!("+{}", now.saturating_sub(was)),
                    _ => "?".into(),
                };
                l.step += 1;
                l.since = now;
                l.rx_at_start = rx;
                let said = match RAMP[l.step].0 {
                    u64::MAX => {
                        l.state = "rythme libre".into();
                        "rythme libre".to_string()
                    }
                    n => {
                        l.state = format!("palier {n} tr/s");
                        format!("{n} tr/s")
                    }
                };
                println!(
                    "== porte TX : la radio tient (rx {delta} en {} s) — je monte à {said}",
                    RAMP[l.step - 1].1
                );
            }
        }
        let ceiling = match l.held {
            Some(c) => c,
            None => RAMP[l.step].0,
        };
        if l.budget >= ceiling {
            self.eaten.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        l.budget += 1;
        self.allowed.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// The monitor's line: where the ladder stands, what went through.
    pub fn report(&self) -> String {
        let l = self.ladder.lock().unwrap();
        format!(
            "porte TX : {} | passées {} mangées {}",
            l.state,
            self.allowed.load(Ordering::Relaxed),
            self.eaten.load(Ordering::Relaxed)
        )
    }
}

/// The kext's sysctls as a radio: a reader thread for the frames coming
/// in, a writer thread for the frames going out, and the ramp every
/// outgoing frame must pass.
pub struct KextAir {
    rx: mpsc::Receiver<Received>,
    tx: mpsc::Sender<Vec<u8>>,
    gate: Arc<Gate>,
}

impl KextAir {
    /// Spawns both threads; their channels are the engine's ends. The
    /// gate is shared with the monitor, which reports it every 10 s.
    pub fn new(gate: Arc<Gate>) -> io::Result<KextAir> {
        let (rx_tx, rx) = mpsc::channel(CHANNEL);
        let (tx_tx, tx_rx) = mpsc::channel(TX_CHANNEL);
        thread::Builder::new()
            .name("itlawdl-rx".into())
            .spawn(move || read_ring(rx_tx))
            .map_err(|e| io::Error::other(format!("le fil de lecture du kext : {e}")))?;
        thread::Builder::new()
            .name("itlawdl-tx".into())
            .spawn(move || write_wire(tx_rx))
            .map_err(|e| io::Error::other(format!("le fil d'émission du kext : {e}")))?;
        Ok(KextAir { rx, tx: tx_tx, gate })
    }
}

/// The reader thread's whole life: drain the ring, parse the records,
/// hand the frames over, say it is alive, and leave when the engine is
/// gone — or the kext under it.
fn read_ring(tx: mpsc::Sender<Received>) {
    let mut buf = vec![0u8; DRAIN];
    let mut frames: u64 = 0;
    let mut reads: u64 = 0;
    let mut bytes: u64 = 0;
    let mut rejects: u64 = 0;
    let mut beat = Instant::now();
    tracing::info!("lecteur : démarré, je vide le ring du kext");
    loop {
        if tx.is_closed() {
            return; // the engine is gone
        }
        let n = match sysctl::read_into(RX_SYSCTL, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("lecteur : le sysctl rx a rendu : {e} — j'arrête");
                return;
            }
        };
        reads += 1;
        let mut parsed = 0;
        let mut off = 0;
        while off + RXREC_HDR <= n {
            let len = u16::from_le_bytes([buf[off], buf[off + 1]]) as usize;
            if off + RXREC_HDR + len > n {
                break; // a record cut by the buffer's edge
            }
            if (24..=MAX_RECORD).contains(&len) {
                // A whole MAC header, of a size a radio really sends.
                let received = Received {
                    bytes: buf[off + RXREC_HDR..off + RXREC_HDR + len].to_vec(),
                    rssi: Some(buf[off + 3] as i8),
                };
                if tx.blocking_send(received).is_err() {
                    return; // the engine is gone
                }
                frames += 1;
                bytes += len as u64;
                parsed += 1;
            } else if len > MAX_RECORD {
                // A length no frame has: a walk gone out of step (a kext
                // v2.2 drain cut a record mid-way). Skip its bytes and
                // keep counting, so the heartbeat tells the story.
                rejects += 1;
            }
            off += RXREC_HDR + len;
        }
        if parsed == 0 {
            // the ring is empty for now, or holds nothing we can read
            thread::sleep(EMPTY_POLL);
        }
        if beat.elapsed() >= HEARTBEAT {
            tracing::info!(
                "lecteur : {frames} trames ({bytes} octets, {rejects} rejets) en {reads} lectures"
            );
            beat = Instant::now();
        }
    }
}

/// The writer thread's whole life: take the engine's requests, one
/// sysctl write each, and leave when the engine is gone. The kext
/// counts what it refuses; errors here are the radio's to log.
fn write_wire(mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(req) = rx.blocking_recv() {
        if let Err(e) = sysctl::write_all(TX_SYSCTL, &req) {
            tracing::debug!("le kext a refusé une écriture : {e}");
        }
    }
}

impl Air for KextAir {
    async fn recv(&mut self) -> io::Result<Received> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::other("le fil de lecture du kext s'est arrêté"))
    }

    fn send(&mut self, frame: Frame) -> Result<(), Frame> {
        if !self.gate.admit(Instant::now()) {
            // The ramp ate it. A stale PSF is worthless by its next try
            // anyway (the driver drops them the same way), and the mDNS
            // burst sends 64 of each — a few eaten change nothing.
            return Ok(());
        }
        let expect_ack = (!frame.no_ack) as u8;
        let (flags, rate) = match frame.rate {
            TxRate::Legacy(r) => (expect_ack, r),
            TxRate::Ht { mcs, .. } => (expect_ack | 0x02, 0x80 | mcs),
        };
        let mut req = Vec::with_capacity(8 + frame.bytes.len());
        req.extend_from_slice(&[2, flags, rate, 0]);
        req.extend_from_slice(&(frame.bytes.len() as u16).to_le_bytes());
        req.extend_from_slice(&[0, 0]);
        req.extend_from_slice(&frame.bytes);
        if let Err(e) = self.tx.try_send(req) {
            // A full queue or a closed writer: the frame is the radio's
            // to drop. The engine has nothing to retry against — there
            // is no queue-full signal to hand the frame back for.
            tracing::debug!("la file d'émission du kext : {e}");
        }
        Ok(())
    }
}
