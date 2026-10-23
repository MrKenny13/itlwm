//
//  AwdlSession.hpp
//  itlwm
//
//  itlawdl v2 -- the AWDL session (cell + injection + RX ring), step 2
//  of the AirDrop-on-Intel work, ported onto this driver by the v1
//  probe's pattern (AwdlProbe.hpp).
//
//  With Wi-Fi turned OFF in macOS, `sysctl -w debug.itlawdl=2` restarts
//  the firmware as a "cell" on channel 6: a PIBSS MAC context (Intel's
//  pseudo-IBSS; a plain IBSS one receives nothing on this firmware) on
//  AWDL's BSSID 00:25:00:ff:94:73 with the station's own address, which
//  makes the firmware ACK the peer's unicast data frames, the way owly
//  does on Linux with an ad-hoc cell. `debug.itlawdl=1` is the v1-style
//  listener (LISTENER MAC context, no ACKing); 0 stops and restores
//  station mode. While a session runs, every frame is also tapped into
//  a ring drained by `debug.itlawdl_rx`, and frames can be injected
//  through `debug.itlawdl_tx` on the monitor inject queue.
//
//  sysctls (the contract with tools/itlawdlctl.c):
//    debug.itlawdl        int rw   0 stop, 1 listen, 2 cell
//    debug.itlawdl_cell   int rw   pin fw mac type: 0 auto (PIBSS),
//                                  4 ibss, 3 pibss, 2 listener
//    debug.itlawdl_stats  string   counters
//    debug.itlawdl_rx     opaque   drains received frames: 16-byte
//                                  header (u16 len, u8 rate, i8 rssi,
//                                  u8 flags, u8 chan, u16 rsv, u64 tsft)
//                                  then the raw 802.11 frame, no FCS
//    debug.itlawdl_tx     opaque   injects: 8-byte request header
//                                  (u8 ver=2, u8 flags bit0 ack /
//                                  bit1 ht, u8 rate, u8 pad, u16 len,
//                                  u16 rsv) then the frame
//
//  Included once, from ItlIwx.cpp (after ItlIwx.hpp -- the sysctl
//  handlers call back into ItlIwx methods through gAwdlDrv).
//

#ifndef AwdlSession_hpp
#define AwdlSession_hpp

#include <sys/sysctl.h>
#include <kern/thread_call.h>
#include <kern/clock.h>

#define AWDL_SESSION_CHANNEL    6       /* 2 GHz, owly's Intel channel */
#define AWDL_SESSION_RING_SIZE  (256 * 1024)
#define AWDL_SESSION_MAX_FRAME  (2304 + 64)

/* sysctl-visible states, as debug.itlawdl reads them back */
#define AWDL_S_OFF          0
#define AWDL_S_LISTENING    1
#define AWDL_S_CELL         2
#define AWDL_S_BUSY_WIFI_ON (-1)    /* refused: turn Wi-Fi off first */
#define AWDL_S_NO_CARD      (-2)
#define AWDL_S_FAILED       (-3)

struct awdl_session_state {
    /* bookkeeping */
    int session;            /* 0 off, 1 listen, 2 cell (mirrors sc_awdl_session) */
    int cell;               /* fw mac type in use: 2 LISTENER, 3 PIBSS, 4 IBSS */
    int cell_wanted;        /* 0 auto (PIBSS), else pinned 2/3/4 */
    int channel;            /* ieee channel, 6 */
    volatile int entering;  /* set while awdlSessionSetGated brings it up */
    int state;              /* what debug.itlawdl reads */
    int want;               /* what the last write asked for */
    thread_call_t call;
    uint64_t since_ns;

    /* rx accounting */
    uint64_t rx, rx_bytes, rx_dropped, dup;
    uint64_t action, psf, mif, other_action, data, data_bytes;
    uint64_t last_seq;      /* last data seq from the peer (dup detection) */
    uint8_t peer[6];        /* last AWDL peer address seen */
    int peer_rssi;

    /* inject accounting */
    uint64_t inject_ok, inject_err, inject_bytes;

    /* rx ring: length-prefixed records, drained by debug.itlawdl_rx */
    uint8_t *ring;
    size_t ring_head, ring_tail, ring_used;
};

static struct awdl_session_state gAwdl = {};
static ItlIwx *gAwdlDrv = NULL;    /* the one card we drive */

static const uint8_t kAwdlSessBssid[6] = { 0x00, 0x25, 0x00, 0xff, 0x94, 0x73 };

/* TX request header (debug.itlawdl_tx), then the raw frame */
struct itlawdl_txreq {
    uint8_t ver, flags, rate, pad;
    uint16_t frame_len, rsv;
} __packed;

/* ---- the RX ring ----
 * Record: 16-byte header then the frame (no FCS):
 *   u16 len (LE); u8 rate (500 kbit/s, or 0x80|mcs); i8 rssi (dBm);
 *   u8 flags (bit0 short preamble); u8 chan; u16 rsv; u64 tsft (us).
 */
static IOSimpleLock *gAwdlRingLock = NULL;
static uint8_t gAwdlDrain[64 * 1024];    /* copied out to user outside the lock */

#define AWDL_SESS_RXREC_HDR 16

static void
awdl_ring_put(const uint8_t *frame, size_t len, uint8_t rate, int8_t rssi,
              uint8_t flags, uint8_t chan, uint64_t tsft)
{
    IOInterruptState st;
    size_t need = AWDL_SESS_RXREC_HDR + len;
    if (!gAwdl.ring || !gAwdlRingLock || len == 0)
        return;
    st = IOSimpleLockLockDisableInterrupt(gAwdlRingLock);
    if (!gAwdl.ring) {
        /* v2.3: a session left while this frame was arriving -- its
         * ring is gone with it (awdlSessionLeaveGated nulls the pointer
         * under this same lock before freeing). */
        IOSimpleLockUnlockEnableInterrupt(gAwdlRingLock, st);
        return;
    }
    if (gAwdl.ring_used + need > AWDL_SESSION_RING_SIZE) {
        gAwdl.rx_dropped++;
        IOSimpleLockUnlockEnableInterrupt(gAwdlRingLock, st);
        return;
    }
    uint8_t *p = gAwdl.ring + gAwdl.ring_head;
    p[0] = (uint8_t)(len & 0xff); p[1] = (uint8_t)(len >> 8);
    p[2] = rate; p[3] = (uint8_t)rssi; p[4] = flags; p[5] = chan;
    p[6] = p[7] = 0;
    memcpy(p + 8, &tsft, 8);
    if (len)
        memcpy(p + AWDL_SESS_RXREC_HDR, frame, len);
    gAwdl.ring_head += need;
    gAwdl.ring_used += need;
    IOSimpleLockUnlockEnableInterrupt(gAwdlRingLock, st);
}

static void
awdl_session_reset_counters(void)
{
    memset(&gAwdl.rx, 0,
           offsetof(struct awdl_session_state, inject_ok) -
           offsetof(struct awdl_session_state, rx));
    gAwdl.last_seq = (uint64_t)-1;
    uint64_t now;
    clock_get_uptime(&now);
    absolutetime_to_nanoseconds(now, &gAwdl.since_ns);
}

/* ---- sysctl handlers ---- */

static void
awdl_session_work(thread_call_param_t, thread_call_param_t)
{
    if (!gAwdlDrv) {
        gAwdl.state = AWDL_S_NO_CARD;
        return;
    }
    gAwdl.state = gAwdlDrv->awdlSessionSet(gAwdl.want, gAwdl.cell_wanted);
}

static int
awdl_state_sysctl SYSCTL_HANDLER_ARGS
{
    int v = gAwdl.state;
    int err = sysctl_handle_int(oidp, &v, 0, req);
    if (err || !req->newptr)
        return err;
    if (v < 0 || v > 2)
        return EINVAL;
    if (!gAwdlDrv)
        return ENODEV;
    gAwdl.want = v;
    if (!gAwdl.call)
        gAwdl.call = thread_call_allocate(awdl_session_work, nullptr);
    if (!gAwdl.call)
        return ENOMEM;
    thread_call_enter(gAwdl.call);
    return 0;
}

static int
awdl_cell_sysctl SYSCTL_HANDLER_ARGS
{
    int v = gAwdl.cell_wanted;
    int err = sysctl_handle_int(oidp, &v, 0, req);
    if (err || !req->newptr)
        return err;
    if (v != 0 && v != IWX_FW_MAC_TYPE_LISTENER &&
        v != IWX_FW_MAC_TYPE_PIBSS && v != IWX_FW_MAC_TYPE_IBSS)
        return EINVAL;
    gAwdl.cell_wanted = v;
    return 0;
}

static int
awdl_stats_sysctl SYSCTL_HANDLER_ARGS
{
    static char buf[512];
    int n;

    n = snprintf(buf, sizeof(buf),
        "rx=%llu bytes=%llu dropped=%llu dup=%llu\n"
        "awdl action=%llu psf=%llu mif=%llu other=%llu data=%llu data_bytes=%llu\n"
        "peer %02x:%02x:%02x:%02x:%02x:%02x rssi=%d\n"
        "inject ok=%llu err=%llu bytes=%llu\n"
        "state=%d cell=%d(want %d) channel=%d\n",
        gAwdl.rx, gAwdl.rx_bytes, gAwdl.rx_dropped, gAwdl.dup,
        gAwdl.action, gAwdl.psf, gAwdl.mif, gAwdl.other_action,
        gAwdl.data, gAwdl.data_bytes,
        gAwdl.peer[0], gAwdl.peer[1], gAwdl.peer[2],
        gAwdl.peer[3], gAwdl.peer[4], gAwdl.peer[5],
        gAwdl.peer_rssi,
        gAwdl.inject_ok, gAwdl.inject_err, gAwdl.inject_bytes,
        gAwdl.session, gAwdl.cell, gAwdl.cell_wanted, gAwdl.channel);
    if (n < 0)
        return EIO;
    return sysctl_handle_string(oidp, buf, n + 1, req);
}

static int
awdl_rx_sysctl SYSCTL_HANDLER_ARGS
{
    size_t n, end, reclen;
    const uint8_t *p;
    IOInterruptState st;

    if (!gAwdlDrv || !gAwdlRingLock)
        return 0;
    if (!req->oldptr) {
        req->oldidx = (u_int)gAwdl.ring_used;
        return 0;
    }
    st = IOSimpleLockLockDisableInterrupt(gAwdlRingLock);
    if (!gAwdl.ring) {
        /* v2.3: no session, or one leaving -- nothing to drain. The
         * check runs under the lock because awdlSessionLeaveGated
         * nulls the pointer (and frees the ring) between the guard
         * above and this point. */
        IOSimpleLockUnlockEnableInterrupt(gAwdlRingLock, st);
        return 0;
    }
    n = gAwdl.ring_used;
    if (n > sizeof(gAwdlDrain))
        n = sizeof(gAwdlDrain);
    if (n > req->oldlen)
        n = req->oldlen;
    /* v2.3: clamp to the last whole record. v2 handed out a tail cut
     * mid-record ("the tool tolerates a truncated tail"), but the
     * bytes already consumed left the reader to resume in the middle
     * of a record it then read as a header -- a stream it could never
     * re-sync. The rest of the cut record stays in the ring for the
     * next read instead. */
    p = gAwdl.ring + gAwdl.ring_tail;
    end = 0;
    while (end + AWDL_SESS_RXREC_HDR <= n) {
        reclen = (size_t)((uint16_t)p[end] | ((uint16_t)p[end + 1] << 8));
        if (end + AWDL_SESS_RXREC_HDR + reclen > n)
            break;
        end += AWDL_SESS_RXREC_HDR + reclen;
    }
    n = end;
    if (n) {
        memcpy(gAwdlDrain, p, n);
        gAwdl.ring_tail += n;
        gAwdl.ring_used -= n;
        if (gAwdl.ring_tail == gAwdl.ring_head)
            gAwdl.ring_tail = gAwdl.ring_head = 0;
    }
    IOSimpleLockUnlockEnableInterrupt(gAwdlRingLock, st);
    if (n == 0)
        return 0;
    return SYSCTL_OUT(req, gAwdlDrain, n);
}

static int
awdl_tx_sysctl SYSCTL_HANDLER_ARGS
{
    static uint8_t reqbuf[AWDL_SESSION_MAX_FRAME + 16];
    int error;
    size_t len;

    if (!req->newptr)
        return 0;
    len = req->newlen;
    if (len < 9 || len > sizeof(reqbuf))
        return EINVAL;
    error = SYSCTL_IN(req, reqbuf, len);
    if (error)
        return error;
    if (!gAwdlDrv)
        return ENXIO;
    /* errors are reported in the inject counters; the write itself
     * succeeds so that sysctl -w does not fail mid-experiment. */
    (void)gAwdlDrv->awdlInject(reqbuf, len);
    return 0;
}

SYSCTL_PROC(_debug, OID_AUTO, itlawdl, CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_LOCKED | CTLFLAG_KERN,
            NULL, 0, awdl_state_sysctl, "I",
            "itlwm AWDL session: 0 stop, 1 listen (v1), 2 cell (PIBSS on AWDL's BSSID, ACKs + injection)");
SYSCTL_PROC(_debug, OID_AUTO, itlawdl_cell, CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_LOCKED | CTLFLAG_KERN,
            NULL, 0, awdl_cell_sysctl, "I",
            "pin fw mac type for the next session: 0 auto (pibss), 4 ibss, 3 pibss, 2 listener");
SYSCTL_PROC(_debug, OID_AUTO, itlawdl_stats, CTLTYPE_STRING | CTLFLAG_RD | CTLFLAG_LOCKED | CTLFLAG_KERN,
            NULL, 0, awdl_stats_sysctl, "A",
            "itlwm AWDL session counters");
SYSCTL_PROC(_debug, OID_AUTO, itlawdl_rx, CTLTYPE_INT | CTLFLAG_RD | CTLFLAG_LOCKED | CTLFLAG_KERN,
            NULL, 0, awdl_rx_sysctl, "I",
            "drain received frames (16-byte header + raw 802.11, no FCS)");
SYSCTL_PROC(_debug, OID_AUTO, itlawdl_tx, CTLTYPE_INT | CTLFLAG_WR | CTLFLAG_LOCKED | CTLFLAG_KERN,
            NULL, 0, awdl_tx_sysctl, "I",
            "inject a frame (8-byte request header + raw 802.11)");

static bool gAwdlSysctlRegistered;

/* Called from ItlIwx::attach (the one card we drive). */
static void
awdl_session_attach(void)
{
    gAwdl.state = AWDL_S_OFF;
    if (!gAwdlRingLock)
        gAwdlRingLock = IOSimpleLockAlloc();
    if (!gAwdlSysctlRegistered) {
        sysctl_register_oid(&sysctl__debug_itlawdl);
        sysctl_register_oid(&sysctl__debug_itlawdl_cell);
        sysctl_register_oid(&sysctl__debug_itlawdl_stats);
        sysctl_register_oid(&sysctl__debug_itlawdl_rx);
        sysctl_register_oid(&sysctl__debug_itlawdl_tx);
        gAwdlSysctlRegistered = true;
    }
}

/* Called from ItlIwx::detach: stop any session before the firmware
 * goes, then let the sysctls go. */
static void
awdl_session_detach(ItlIwx *iwx)
{
    if (gAwdlDrv != iwx)
        return;
    if (gAwdl.session)
        iwx->awdlSessionLeaveGated();
    if (gAwdlSysctlRegistered) {
        sysctl_unregister_oid(&sysctl__debug_itlawdl);
        sysctl_unregister_oid(&sysctl__debug_itlawdl_cell);
        sysctl_unregister_oid(&sysctl__debug_itlawdl_stats);
        sysctl_unregister_oid(&sysctl__debug_itlawdl_rx);
        sysctl_unregister_oid(&sysctl__debug_itlawdl_tx);
        gAwdlSysctlRegistered = false;
    }
    if (gAwdl.call) {
        thread_call_cancel(gAwdl.call);   /* _wait is private KPI: unresolved for a third-party kext */
        thread_call_free(gAwdl.call);
        gAwdl.call = NULL;
    }
    gAwdl.session = 0;
    gAwdl.state = AWDL_S_OFF;
    gAwdlDrv = NULL;
}

#endif /* AwdlSession_hpp */
