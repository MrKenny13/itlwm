//! The itlawdl kext as an [`Air`]: frames tapped off its cell, injected
//! back through `debug.itlawdl_tx`.
//!
//! RX records, `debug.itlawdl_rx`: a 16-byte header then the raw 802.11
//! frame without FCS — u16 len (LE), u8 rate (500 kbit/s, or 0x80|mcs),
//! i8 rssi, u8 flags, u8 chan, u16 rsv, u64 tsft — in runs of at most
//! 64 KiB, so `recv` drains until a short read.
//!
//! TX requests, `debug.itlawdl_tx`: an 8-byte header then the frame —
//! u8 ver=2, u8 flags (bit0 expect ack, bit1 ht mcs), u8 rate (same
//! coding as RX), u8 pad, u16 frame_len (LE), u16 rsv. A write the kext
//! takes always succeeds: one it refuses is counted in its stats, the
//! radio's to log, not to retry — there is no queue-full signal to hand
//! the frame back for.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use owly_awdl::driver::{Air, Received};
use owly_awdl::frame::Frame;
use owly_awdl::TxRate;

use crate::sysctl;

const RX_SYSCTL: &str = "debug.itlawdl_rx";
const TX_SYSCTL: &str = "debug.itlawdl_tx";

/// The kext's drain buffer size; one read takes at most this much.
const DRAIN: usize = 64 * 1024;
/// The largest frame the ring holds, to tell a short read by.
const MAX_FRAME: usize = 2368;
/// The header on every RX record.
const RXREC_HDR: usize = 16;
/// How long to wait between polls of an empty ring.
const EMPTY_POLL: Duration = Duration::from_millis(2);

/// The kext's sysctls as a radio.
pub struct KextAir {
    buf: Vec<u8>,
    queue: VecDeque<Received>,
}

impl KextAir {
    pub fn new() -> KextAir {
        KextAir {
            buf: vec![0; DRAIN],
            queue: VecDeque::new(),
        }
    }

    /// Drains the ring into the queue; returns how many bytes one read
    /// held (so `recv` can tell "the ring is empty for now" from "there
    /// was more than a buffer's worth").
    fn drain(&mut self) -> io::Result<usize> {
        let n = sysctl::read_into(RX_SYSCTL, &mut self.buf)?;
        let mut off = 0;
        while off + RXREC_HDR <= n {
            let len = u16::from_le_bytes([self.buf[off], self.buf[off + 1]]) as usize;
            if off + RXREC_HDR + len > n {
                break; // a record truncated by the buffer's edge
            }
            let rssi = self.buf[off + 3] as i8;
            let frame = &self.buf[off + RXREC_HDR..off + RXREC_HDR + len];
            if len >= 24 {
                // a whole MAC header: the driver parses from there on
                self.queue.push_back(Received {
                    bytes: frame.to_vec(),
                    rssi: Some(rssi),
                });
            }
            off += RXREC_HDR + len;
        }
        Ok(n)
    }
}

impl Air for KextAir {
    async fn recv(&mut self) -> io::Result<Received> {
        loop {
            if let Some(received) = self.queue.pop_front() {
                return Ok(received);
            }
            let n = self.drain()?;
            if n < DRAIN - MAX_FRAME && self.queue.is_empty() {
                // a short read: the ring is empty for now
                tokio::time::sleep(EMPTY_POLL).await;
            }
        }
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
