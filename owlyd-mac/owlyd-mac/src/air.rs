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
//! second, ten seconds each, the kext's counters read at every
//! boundary, the ceiling that survived kept for the rest of the session.
//!
//! v0.2.0, the audit's lesson: a quiet channel is not a dead radio.
//! The v08 run judged on rx alone, and the field channel runs quiet
//! (5 frames in 12 s of listening — a busy one brings 25) — a radio
//! hearing nothing for one window might just have nothing to hear.
//! The verdict now needs the firmware's own word: its error dumps
//! (`fw_err=`, v2.6+) cut at once, and a silent window only holds the
//! step for another — two in a row at pace, with the firmware silent,
//! is the only rx-only death. A cut that sees rx move again re-opens
//! the ladder (up to thrice), and once the ramp is open the firmware
//! keeps being watched: an error dump in free run falls back to the
//! last pace that was proven safe.
//!
//! TX requests, `debug.itlawdl_tx`: an 8-byte header then the frame —
//! u8 ver=2, u8 flags (bit0 unused -- a pre-AX210 ACK bit the gen3 TX
//! command reserves; v2.5+ kexts ignore it, and v0.2.0 stops setting
//! it, bit1 ht mcs), u8 rate (same coding as RX), u8 pad, u16 frame_len
//! (LE), u16 rsv. A write the kext takes always succeeds: one it
//! refuses is counted in its stats, the radio's to log, not to retry.

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
/// step lasts). The last one stays for the rest of the session, watched
/// by the free-run guard. The v06 field run froze rx at the very moment
/// the engine's first frames went out — 24 a second from zero — and only
/// a staged climb can say which of the two it was: one frame is poison
/// (the kext's inject path itself, its queue to change), or twenty-four
/// a second choke the firmware (a pace to hold).
const RAMP: [(u64, u64); 4] = [(1, 10), (4, 10), (12, 10), (u64::MAX, 0)];
/// A step's rx must grow by at least this to count as hearing. One
/// frame is enough: a healthy radio on a quiet channel still catches
/// the odd beacon or a peer's action frame (the field channel ran at
/// 5 frames in 12 s), while a deaf one catches exactly none, ever.
const STILL_ALIVE: u64 = 1;
/// How long the free-run guard waits between firmware checks.
const WATCH: Duration = Duration::from_secs(10);
/// How often a cut gate probes the rx counter for signs of life.
const REOPEN_PROBE: Duration = Duration::from_secs(2);
/// How many times a cut gate may re-open on signs of life.
const REOPEN_MAX: u32 = 3;

/// The gate every frame out has to pass: one ceiling at a time, each
/// step judged by the kext's own counters before the next opens — rx
/// for hearing, `fw_err` for the firmware's own word on our emission
/// (v0.2.0: the v08 run's cut was read off rx alone, on a channel quiet
/// enough that a healthy radio can look dead for a window; the
/// firmware's error dump is the honest death sentence, and rx standing
/// still only holds the step — twice in a row is the rx-only death).
/// A step that survives opens the next; a cut that hears rx again
/// re-opens the ladder, thrice at most. In free run the firmware stays
/// watched: an error dump falls back to the last proven pace.
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
    fw_at_start: Option<u64>,
    /// Judged windows in a row in which rx stood still and the firmware
    /// said nothing: a quiet channel is not a dead radio, but two at
    /// pace is as dead as rx alone can say.
    quiet: u32,
    /// The 1 s budget window of the current ceiling.
    window: Instant,
    budget: u64,
    /// Set once a verdict closed the ladder: the ceiling to keep (0
    /// cuts emission), with the reason it did.
    held: Option<(u64, String)>,
    /// How many times a cut gate re-opened on signs of life.
    reopens: u32,
    /// When a cut gate last probed the rx counter.
    probed: Instant,
    /// The last boundary's counters, for the monitor's line.
    verdict: String,
    /// One line of state for the monitor's ticker.
    state: String,
}

impl Gate {
    pub fn new() -> Gate {
        let rx = sysctl::radio_rx();
        let fw = sysctl::radio_fw_err();
        Gate {
            ladder: Mutex::new(Ladder {
                step: 0,
                since: Instant::now(),
                rx_at_start: rx,
                fw_at_start: fw,
                quiet: 0,
                window: Instant::now(),
                budget: 0,
                held: None,
                reopens: 0,
                probed: Instant::now(),
                verdict: "première fenêtre".into(),
                state: "palier 1 tr/s".into(),
            }),
            allowed: AtomicU64::new(0),
            eaten: AtomicU64::new(0),
        }
    }

    /// v0.2.2: the control experiment's gate -- shut from the start and
    /// for good (`--sans-emission`): the engine runs, receives and builds
    /// its frames, and not one reaches the kext. A firmware that still
    /// dumps (fw_err climbing) with nothing sent dies of the context,
    /// not of our TX.
    pub fn closed() -> Gate {
        let g = Gate::new();
        {
            let mut l = g.ladder.lock().unwrap();
            l.held = Some((0, "contrôle sans émission".into()));
            l.reopens = REOPEN_MAX;
            l.state = "émission coupée (contrôle sans émission, voulu)".into();
        }
        g
    }

    /// Whether this frame may go out now. Called from the engine's
    /// thread for every frame it builds — including the ones it eats.
    pub fn admit(&self, now: Instant) -> bool {
        let mut l = self.ladder.lock().unwrap();
        if now.saturating_duration_since(l.window) >= Duration::from_secs(1) {
            l.window = now;
            l.budget = 0;
        }
        if l.held.is_some() {
            // A cut gate listens for signs of life (a dead radio's rx
            // never moves again; a quiet channel's does, eventually)
            // and re-opens the ladder when one comes -- thrice at most,
            // so a flapping radio cannot loop us forever.
            if l.reopens < REOPEN_MAX
                && now.saturating_duration_since(l.probed) >= REOPEN_PROBE
            {
                l.probed = now;
                if let (Some(rx), Some(was)) = (sysctl::radio_rx(), l.rx_at_start) {
                    if rx > was {
                        l.reopens += 1;
                        l.held = None;
                        l.quiet = 0;
                        l.step = 0;
                        l.since = now;
                        l.rx_at_start = sysctl::radio_rx();
                        l.fw_at_start = sysctl::radio_fw_err();
                        l.state = format!(
                            "reprise à 1 tr/s (la radio reçoit à nouveau, essai {}/{REOPEN_MAX})",
                            l.reopens
                        );
                        println!(
                            "== porte TX : la radio capte à nouveau — je reprends la rampe à 1 tr/s \
                             (essai {}/{REOPEN_MAX})",
                            l.reopens
                        );
                    } else {
                        l.rx_at_start = Some(rx);
                    }
                }
            }
        } else {
            self.boundary(&mut l, now);
        }
        let ceiling = match l.held {
            Some((c, _)) => c,
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

    /// The step boundary, when a step's time is up (and the free-run
    /// guard, every [`WATCH`] once the ramp is open).
    fn boundary(&self, l: &mut Ladder, now: Instant) {
        let (ceiling, secs) = RAMP[l.step];
        if secs > 0 {
            if now.saturating_duration_since(l.since) < Duration::from_secs(secs) {
                return;
            }
        } else if now.saturating_duration_since(l.since) < WATCH {
            return;
        }
        let rx = sysctl::radio_rx();
        let fw = sysctl::radio_fw_err();
        let rx_delta = match (rx, l.rx_at_start) {
            (Some(a), Some(b)) => a.saturating_sub(b),
            _ => 0,
        };
        let fw_delta = match (fw, l.fw_at_start) {
            (Some(a), Some(b)) => a.saturating_sub(b),
            _ => 0,
        };
        l.verdict = format!(
            "rx +{rx_delta}{}",
            match fw {
                Some(f) => format!(" | fw_err {f}"),
                None => String::new(),
            }
        );
        // The firmware's own word: an error dump during our emission is
        // the honest death sentence, at any pace, in the ramp and in
        // free run alike (v08: fw_err=2 while ok climbed and rx died).
        if fw_delta > 0 {
            let back = if secs > 0 && l.step > 0 {
                RAMP[l.step - 1].0
            } else if secs == 0 {
                RAMP[RAMP.len() - 2].0
            } else {
                0
            };
            // v0.2.1: the values, not the placeholders — the message
            // has said "fwid=/il1=/hcmd=" since v0.2.0 shipped it as a
            // literal. The UMAC half joins in (v2.7.2 kexts): it owns
            // the stations and queues, and its data1-3 name the
            // assert's objects.
            let dump = sysctl::radio_fw_dump();
            let why = match &dump {
                Some(d) => format!(
                    "le firmware a planté (dump d'erreur : fwid=0x{:x} il1=0x{:x} \
                     hcmd=0x{:x} | umac id=0x{:x} il=0x{:x} d1=0x{:x} d2=0x{:x} \
                     d3=0x{:x} ch=0x{:x} — dans les stats)",
                    d.fwid, d.il1, d.hcmd, d.u_id, d.u_il1, d.u_d1, d.u_d2, d.u_d3, d.u_ch
                ),
                None => "le firmware a planté (dump d'erreur : voir les stats)".to_string(),
            };
            l.held = Some((back, why.clone()));
            if back == 0 {
                l.state = "émission coupée (plantage firmware)".into();
                let pace = ceiling_limit(l);
                println!(
                    "== porte TX : {why} dès {pace} — j'arrête d'émettre pour cette \
                     session ; colle-moi ce log : les stats nomment l'assert \
                     (fwid=, umac id=, d1-d3=)"
                );
            } else {
                l.state = format!("tenue à {back} tr/s (plantage firmware en rythme libre)");
                println!(
                    "== porte TX : {why} en rythme libre — je tiens à {back} tr/s pour la suite \
                     de la session (la réception et les pairs continuent)"
                );
            }
            return;
        }
        // The stats went away: the kext (or its session) is in no state
        // to judge anything — climb on, the engine's own failure
        // channel will say why. (A pre-2.6 kext never says fw_err;
        // rx alone carries the verdict there, two windows as below.)
        let heard = rx_delta >= STILL_ALIVE || rx.is_none();
        if secs == 0 {
            // Free run: nothing cuts on a quiet channel — the verdict
            // line says what the window saw, and that is all.
            l.since = now;
            l.rx_at_start = rx;
            l.fw_at_start = fw;
            l.quiet = 0;
            return;
        }
        if heard {
            l.quiet = 0;
            self.climb(l, now, rx, fw, rx_delta);
        } else {
            l.quiet += 1;
            if l.quiet >= 2 {
                // Two silent windows at pace, the firmware silent: as
                // dead as rx alone can say. Hold the last proven pace
                // (at step 0 nothing was ever proven -- cut).
                let last = if l.step == 0 { 0 } else { RAMP[l.step - 1].0 };
                l.held = Some((last, "rx muet".into()));
                if last == 0 {
                    l.state =
                        "émission coupée (la radio meurt à la première trame)".into();
                    println!(
                        "== porte TX : la radio n'a plus rien reçu dès 1 trame/s (deux fenêtres, \
                         le firmware muet) — la première injection l'assourdit. J'arrête d'émettre \
                         pour cette session ; colle-moi ce log : le prochain pas est côté kext \
                         (file TVQM)"
                    );
                } else {
                    l.state = format!(
                        "tenue à {last} tr/s (la radio est morte à {ceiling})"
                    );
                    println!(
                        "== porte TX : la radio n'a plus rien reçu à {ceiling} tr/s — je m'arrête à \
                         {last} tr/s pour la suite de la session (la réception et les pairs \
                         continuent)"
                    );
                }
            } else {
                // One silent window: hold the step and watch another --
                // a quiet channel is not a dead radio.
                l.since = now;
                l.rx_at_start = rx;
                l.fw_at_start = fw;
                l.state = format!(
                    "palier {ceiling} tr/s (canal calme, je regarde encore)"
                );
            }
        }
    }

    /// One window survived: open the next step (or the ramp's end).
    fn climb(&self, l: &mut Ladder, now: Instant, rx: Option<u64>, fw: Option<u64>, rx_delta: u64) {
        l.step += 1;
        l.since = now;
        l.rx_at_start = rx;
        l.fw_at_start = fw;
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
            "== porte TX : la radio tient (rx +{rx_delta} en {} s) — je monte à {said}",
            RAMP[l.step - 1].1
        );
    }

    /// The monitor's line: where the ladder stands, what went through,
    /// and what the last window saw.
    pub fn report(&self) -> String {
        let l = self.ladder.lock().unwrap();
        let why = l
            .held
            .as_ref()
            .map(|(_, why)| format!(" — {why}"))
            .unwrap_or_default();
        format!(
            "porte TX : {}{why} | passées {} mangées {} | {}",
            l.state,
            self.allowed.load(Ordering::Relaxed),
            self.eaten.load(Ordering::Relaxed),
            l.verdict
        )
    }
}

/// The pace a step was at, in the log's words (the free-run step has
/// no number of its own).
fn ceiling_limit(l: &Ladder) -> String {
    match RAMP[l.step].0 {
        u64::MAX => "un rythme libre".into(),
        n => format!("{n} tr/s"),
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
        // v0.2.0: no ACK bit -- IWX_TX_CMD_FLG_ACK belongs to the
        // pre-AX210 TX command; the gen3 one reserves the bit, v2.5+
        // kexts ignore it, and the firmware ACKs unicast on its own.
        let (flags, rate) = match frame.rate {
            TxRate::Legacy(r) => (0, r),
            TxRate::Ht { mcs, .. } => (0x02, 0x80 | mcs),
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
