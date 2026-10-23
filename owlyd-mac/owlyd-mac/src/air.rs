//! The itlawdl kext as an [`Air`]: frames tapped off its cell, injected
//! back through `debug.itlawdl_tx`.
//!
//! RX records, `debug.itlawdl_rx`: a 16-byte header then the raw 802.11
//! frame without FCS — u16 len (LE), u8 rate (500 kbit/s, or 0x80|mcs),
//! i8 rssi, u8 flags, u8 chan, u16 rsv, u64 tsft — in runs of at most
//! 64 KiB.
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
//! TX requests, `debug.itlawdl_tx`: an 8-byte header then the frame —
//! u8 ver=2, u8 flags (bit0 expect ack, bit1 ht mcs), u8 rate (same
//! coding as RX), u8 pad, u16 frame_len (LE), u16 rsv. A write the kext
//! takes always succeeds: one it refuses is counted in its stats, the
//! radio's to log, not to retry — there is no queue-full signal to hand
//! the frame back for. (The field run answered this one every time; if
//! it ever stalls, the same medicine — a writer thread — is ten lines
//! away.)

use std::io;
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
/// How long the reader waits between reads that held nothing.
const EMPTY_POLL: Duration = Duration::from_millis(2);
/// How often the reader says it is alive.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// How many frames may wait for the engine at once.
const CHANNEL: usize = 1024;

/// The kext's sysctls as a radio: a reader thread for the frames coming
/// in, an inline write for the frames going out.
pub struct KextAir {
    rx: mpsc::Receiver<Received>,
}

impl KextAir {
    /// Spawns the reader thread; its channel is the engine's end.
    pub fn new() -> io::Result<KextAir> {
        let (tx, rx) = mpsc::channel(CHANNEL);
        thread::Builder::new()
            .name("itlawdl-rx".into())
            .spawn(move || read_ring(tx))
            .map_err(|e| io::Error::other(format!("le fil de lecture du kext : {e}")))?;
        Ok(KextAir { rx })
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
            if len >= 24 {
                // a whole MAC header: the driver parses from there on
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
            }
            off += RXREC_HDR + len;
        }
        if parsed == 0 {
            // the ring is empty for now, or holds nothing we can read
            thread::sleep(EMPTY_POLL);
        }
        if beat.elapsed() >= HEARTBEAT {
            tracing::info!("lecteur : {frames} trames ({bytes} octets) en {reads} lectures");
            beat = Instant::now();
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
        if let Err(e) = sysctl::write_all(TX_SYSCTL, &req) {
            // The write itself failing means the session is gone; say so
            // the way a radio would.
            tracing::debug!("the kext refused a frame: {e}");
        }
        Ok(())
    }
}
