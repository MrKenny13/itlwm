//
//  AwdlSession.hpp
//  itlwm
//
//  itlawdl v2 -- the AWDL session (cell + injection + RX ring), step 2
//  of the AirDrop-on-Intel work, ported onto this driver by the v1
//  probe's pattern (AwdlProbe.hpp).
//
//  v2.4: the inject queue's slots are reclaimed when the ring comes
//  back around (its completions never arrive on this firmware -- the
//  v04 field run exhausted them at 330 TFDs and went radio-silent),
//  and an enable() that fails now fails the session instead of
//  leaving a deaf cell up.
//
//  v2.5: the inject stats grew `done=` (TX completions seen for the
//  inject queue) and `recycled=` (slots taken back the v2.4 way). The
//  v05/v06 field runs both watched rx die at the engine's first frames
//  while `ok=` climbed -- with `done=` beside them the next log says
//  whether the firmware ever touched what we queued. Also: the TX
//  request's "expect ACK" bit no longer sets IWX_TX_CMD_FLG_ACK -- that
//  bit belongs to the pre-AX210 TX command's flag word, and the gen3
//  one reserves its bit 3; the firmware ACKs unicast on its own.
//
//  v2.6: the inject queue's station binding is selectable -- sysctl
//  debug.itlawdl_txsta, read when a session comes up. 0 is v2.5's
//  monitor station (GENERAL_PURPOSE, zero address): the v07 field run
//  named its queue a black hole (ok climbed to 10 while done never
//  moved off its boot-time 243 -- the firmware never serviced it --
//  and rx died at the very first TFD). 1, the default, binds the
//  queue to sta 0 in the STA-mode LINK shape -- the binding the
//  boot-time traffic completed 243 frames through on this very card.
//  2 is iwlwifi's monitor-injection shape (an aux-activity station).
//  The inject stats grow done_err=/last= (completions that came back
//  FAILED, their status word kept) and fw_err= (firmware error dumps
//  seen), inject counters reset with the session now, and injected
//  frames owe the node reference their completion releases.
//
//  v2.7: the poison was the rate byte, not the queue. The v08 field
//  run (v2.6, binding 1) repeated v2.5's exact signature -- ok=10,
//  done=0, rx dead at the first TFD, and now fw_err=2 to name it: a
//  firmware error dump. The inject path's legacy branch never set
//  hwrate on this VER_8 TX command (the >8 version branch is for
//  other firmwares), so rate_n_flags carried an uninitialized stack
//  byte -- every action frame ever injected, on every binding. The
//  fix is the line iwx_tx_fill_cmd had all along: hwrate =
//  rinfo->plcp. The stats grow txrate= (the exact rate_n_flags the
//  firmware received -- the proof the byte is the PLCP code, e.g.
//  0x...05 for 12 Mbit/s) and fwid=/il1=/hcmd= (the last error
//  dump's assert id, interrupt link, and command word), so if
//  anything still wedges, the next log names the assert without the
//  system log.
//
//  v2.7.1: two audit fixes. (1) The stats buffer: a truncated
//  snprintf returns the would-be length (>= sizeof(buf)), which
//  sysctl_handle_string would take as the buffer's size -- reading (and
//  on some XNU paths writing) past it. The length is clamped to the
//  buffer now. (2) awdlInject demands a live session: the ring_count
//  alone said the queue was allocated once, not that a session owns it
//  now -- a tool writing between sessions injected against a dead
//  queue.
//
//  v2.7.2: the v10 run (binding 2, aux) cleared the last two suspects
//  -- the rate word was valid and the first frame finally survived --
//  but the UMAC still died within ten. What the firmware never had is
//  a station typed for broadcast: iwlwifi gives every vif an
//  IWL_STA_MULTICAST one, and itlwm never creates one. Binding 3
//  (debug.itlawdl_txsta=3) is binding 1 with only the station's type
//  changed to MULTICAST (address ff:ff:ff:ff:ff:ff) -- same sta 0,
//  same queue 2, same tid 15. And the error dump's UMAC table now
//  rides in the stats (umac id=/il=/d1=/d2=/d3=/ch=) beside the LMAC
//  half, kept from the session's FIRST dump (the assert that killed
//  it; later dumps are the corpse's), so the next field log names the
//  UMAC's own numbers without hunting kprintf output the field never
//  reads.
//
//  v2.8: the dossier review. Bindings 1 and 2 -- the two that killed
//  the UMAC -- both put OUR OWN address on a LINK/AUX station, a shape
//  iwlwifi mvm never builds (its STA-mode LINK station carries the
//  AP's address; its aux station sits on no real MAC context). mvm
//  injects from a LISTENER context through the snif_sta: a GENERAL_
//  PURPOSE station, zero address, its queue on tid IWL_MAX_TID_COUNT
//  (8) under TVQM. Binding 4 (debug.itlawdl_txsta=4) is that object;
//  run it with debug.itlawdl_cell=2 (LISTENER through the cell path:
//  promisc, no FCS, frames consumed by the ring). No ACKs for the
//  peer's unicast in LISTENER -- this proves TX, not the datapath.
//
//  v2.8.1: the v11 run killed binding 3 (mcast) the same way it
//  killed 0/1/2, and done=0 across all of them -- the firmware
//  NEVER serviced the inject queue, so nothing we injected ever
//  reached the air and the station axis was never the operative
//  variable. Two more canonical shapes verified off the sources:
//  (a) this fw advertises TLV API_STA_TYPE (parsed out of its own
//  ucode), so station_type is honored -- and Linux master's
//  fw/api/sta.h still carries this driver's enum (no
//  "STATION_TYPE_BCAST_MGMT" exists); commit ced19f26 types the vif's
//  BROADCAST station GENERAL_PURPOSE, the one mvm/tx.c hands ADHOC
//  management frames to. Binding 5 is binding 3 with only the type
//  changed again (mcast -> gp): sta 0, ff:ff:ff:ff:ff:ff, queue 2,
//  tid 15 -- the ADHOC-broadcast shape, on the PIBSS cell.
//  (b) the one thing the working STA path does that the session never
//  did: a session protection (CAPA SESSION_PROT_CMD is set in this
//  fw). Binding 6 = binding 5 + iwx_schedule_protect_session; prot=
//  in the stats says what the fw did with it (1 taken, -1 refused).
//  And the discriminator, kext-side this time: debug.itlawdl_notx=1
//  makes awdlInject hold every frame BEFORE the TFD (held= counts
//  them) -- the same experiment as the daemon's --sans-emission one
//  layer lower: the writes reach the kext, the TFDs stay unwritten.
//  If the fw still dies in a notx run, the TX path is innocent and
//  the bomb is the session's own machinery. The stats also grow the
//  first dump's LMAC data1-3 (lmac ld1=/ld2=/ld3= -- distinct keys so
//  the daemon's UMAC parse keeps its own: the 0x71 NMI is the UMAC
//  relaying another block's death, and the LMAC half usually names
//  the real fault).
//
//  v2.8.2: the v2.8 field run (binding 4, mvm's exact snif_sta) died
//  with the same assert as 1/2/3 and done=0 throughout -- the fw never
//  completes a TX on the inject queue; the frame sits in the ring
//  (queued=1) and the LMAC asserts (ADVANCED_SYSASSERT 0x103D, il
//  0x4CF022, hcmd 0x8001FC12). The --sans-emission control ran clean
//  (fw_err=0), so the context is innocent and transmitting on the
//  static queue is the trigger. The bcast_sta (binding 5/6) attacks the
//  station+protection axis; binding 7 attacks the remaining one, the
//  queue. On gen3 mvm allocates every data-carrying queue through TVQM
//  (iwl_trans_txq_alloc), never the static SCD_QUEUE_CFG the inject
//  path binds a fixed qid 2 with. Binding 7 (debug.itlawdl_txsta=7) is
//  binding 1's station (sta 0 LINK, our MAC) with its queue allocated
//  through TVQM -- the fw picks the qid, named in the stats as qid=.
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
//    debug.itlawdl_txsta  int rw   inject queue binding: 0 monitor
//                                  (v2.5), 1 sta0 LINK (default), 2
//                                  aux, 3 sta0 MULTICAST (v2.7.2,
//                                  iwlwifi's bcast shape), 4 mvm's
//                                  snif_sta (GP, zero address, tid 8,
//                                  no HT flags; v2.8), 5 sta0 GP
//                                  broadcast tid 15 (v2.8.1, mvm's
//                                  bcast_sta), 6 = 5 + session
//                                  protection -- read at session start
//    debug.itlawdl_notx   int rw   v2.8.1: 1 = hold injected frames
//                                  before the TFD (held= counts them)
//                                  -- the discriminator, kext-side;
//                                  read live
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

    /* inject accounting. done counts TX completions seen for the
     * inject queue (iwx_rx_tx_cmd / iwx_rx_tx_ba_notif) — the v05/v06
     * runs never saw one, which no counter could say. recycled counts
     * the slots awdlTxInject took back the v2.4 way. v2.6: done_err
     * counts completions that came back FAILED (the status word of
     * the last one kept in last_status — a lifetime/fifo/abort
     * verdict separates "never scheduled" from "scheduled and
     * refused"), and fw_err counts firmware error dumps
     * (iwx_nic_error). The whole block resets with the session now,
     * so done= reads "this session", not "since boot" (the v07 run
     * read 243 at daemon start: the boot-time queue, not ours). */
    uint64_t inject_ok, inject_err, inject_bytes;
    uint64_t inject_done, inject_recycled;
    /* v2.8.1: frames dropped by debug.itlawdl_notx before the TFD --
     * a held frame is neither ok (it never queued) nor err (nothing
     * refused it); the discriminator run reads it as "the session
     * ran, emission held, fw watched". */
    uint64_t inject_held;
    uint64_t inject_done_err;
    uint32_t inject_last_status;
    /* v2.7: the last rate_n_flags handed to the firmware (txrate=) --
     * proof in every log that the rate byte is the PLCP code, not
     * stack garbage like every injected frame before v2.7. */
    uint32_t inject_rate_nf;
    uint64_t fw_err;
    /* v2.7: the last error dump's identity -- which assert (fwid=),
     * where (il1=), executing which command word (hcmd=). v2.7.2: kept
     * from the session's FIRST dump (the one that killed it; later
     * ones are the corpse's -- v10's later dumps all said 0x103d while
     * the first said 0x71, the UMAC fatal). */
    uint32_t fw_err_id, fw_err_il1, fw_err_hcmd;
    /* v2.7.2: the same dump's UMAC table -- the UMAC owns the stations
     * and queues, and its data1-3 name the assert's objects. They went
     * to kprintf only before, which a field log never reads. */
    uint32_t fw_err_u_id, fw_err_u_il1, fw_err_u_d1, fw_err_u_d2,
              fw_err_u_d3, fw_err_u_ch;
    /* v2.8.1: the same first dump's LMAC data1-3. The 0x71 NMI is the
     * UMAC relaying another block's death -- the LMAC half usually
     * names the real fault -- and the v11 run's LMAC half only ever
     * fed kprintf. ld1=/ld2=/ld3= in the stats so they cannot collide
     * with the umac d1=/d2=/d3= the daemon parses. */
    uint32_t fw_err_l_d1, fw_err_l_d2, fw_err_l_d3;

    /* v2.6: the binding this session's inject queue came up on
     * (tx_sta = debug.itlawdl_txsta's value at session start;
     * tx_sta_id/tx_tid the resolved queue mapping). Written at
     * session start like cell/channel, not reset. v2.7.2: tx_sta_type
     * joins them -- the station's TYPE is the binding's whole point,
     * and the stats line names it. */
    int tx_sta_wanted;
    int tx_sta, tx_sta_id, tx_tid;
    int tx_sta_type;
    /* v2.8.2: the inject queue id this session used -- 2 for the static
     * bindings, the fw's TVQM pick for binding 7. Named in stats (qid=). */
    int tx_qid;
    /* v2.8.1 knobs, never reset with the counters: notx_wanted is the
     * kext-side discriminator flag (debug.itlawdl_notx); prot says
     * what binding 6's session protection got -- 0 none requested,
     * 1 the fw took it, -1 it refused (the session runs on
     * regardless, the stats and the log both say). */
    int notx_wanted;
    int prot;
    /* v2.8.3: binding 8's TLC rate-scaling config result: 0 none,
     * 1 sent, -1 refused. */
    int tlc;

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
           offsetof(struct awdl_session_state, tx_sta_wanted) -
           offsetof(struct awdl_session_state, rx));
    gAwdl.last_seq = (uint64_t)-1;
    uint64_t now;
    clock_get_uptime(&now);
    absolutetime_to_nanoseconds(now, &gAwdl.since_ns);
}

/* ---- sysctl handlers ---- */

/* v2.7.2: the station's type, by name, for the stats line -- "link",
 * "gp", "mcast", "tdls", "aux": the binding's whole point, visible. */
static const char *
awdl_sta_type_name(int t)
{
    switch (t) {
    case IWX_STA_LINK: return "link";
    case IWX_STA_GENERAL_PURPOSE: return "gp";
    case IWX_STA_MULTICAST: return "mcast";
    case IWX_STA_TDLS_LINK: return "tdls";
    case IWX_STA_AUX_ACTIVITY: return "aux";
    default: return "?";
    }
}

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
awdl_txsta_sysctl SYSCTL_HANDLER_ARGS
{
    int v = gAwdl.tx_sta_wanted;
    int err = sysctl_handle_int(oidp, &v, 0, req);
    if (err || !req->newptr)
        return err;
    if (v < 0 || v > 9)
        return EINVAL;
    /* takes effect at the next session start; the stats line says
     * which binding is live (txsta=). */
    gAwdl.tx_sta_wanted = v;
    return 0;
}

static int
awdl_notx_sysctl SYSCTL_HANDLER_ARGS
{
    int v = gAwdl.notx_wanted;
    int err = sysctl_handle_int(oidp, &v, 0, req);
    if (err || !req->newptr)
        return err;
    if (v != 0 && v != 1)
        return EINVAL;
    /* v2.8.1: read live by awdlInject -- set it before a run and the
     * session builds every context, station and queue as usual, only
     * no TFD is ever written. held= in the stats counts the drops. */
    gAwdl.notx_wanted = v;
    return 0;
}

static int
awdl_stats_sysctl SYSCTL_HANDLER_ARGS
{
    static char buf[896];
    int n;

    n = snprintf(buf, sizeof(buf),
        "rx=%llu bytes=%llu dropped=%llu dup=%llu\n"
        "awdl action=%llu psf=%llu mif=%llu other=%llu data=%llu data_bytes=%llu\n"
        "peer %02x:%02x:%02x:%02x:%02x:%02x rssi=%d\n"
        "inject ok=%llu err=%llu held=%llu bytes=%llu done=%llu recycled=%llu done_err=%llu last=0x%x txrate=0x%x\n"
        "fw_err=%llu fwid=0x%x il1=0x%x hcmd=0x%x txsta=%d(sta %d type %s tid %d qid %d want %d%s)\n"
        "umac id=0x%x il=0x%x d1=0x%x d2=0x%x d3=0x%x ch=0x%x\n"
        "lmac ld1=0x%x ld2=0x%x ld3=0x%x\n"
        "state=%d cell=%d(want %d) channel=%d prot=%d tlc=%d notx=%d\n",
        gAwdl.rx, gAwdl.rx_bytes, gAwdl.rx_dropped, gAwdl.dup,
        gAwdl.action, gAwdl.psf, gAwdl.mif, gAwdl.other_action,
        gAwdl.data, gAwdl.data_bytes,
        gAwdl.peer[0], gAwdl.peer[1], gAwdl.peer[2],
        gAwdl.peer[3], gAwdl.peer[4], gAwdl.peer[5],
        gAwdl.peer_rssi,
        gAwdl.inject_ok, gAwdl.inject_err, gAwdl.inject_held,
        gAwdl.inject_bytes,
        gAwdl.inject_done, gAwdl.inject_recycled,
        gAwdl.inject_done_err, (unsigned)gAwdl.inject_last_status,
        (unsigned)gAwdl.inject_rate_nf,
        gAwdl.fw_err, (unsigned)gAwdl.fw_err_id,
        (unsigned)gAwdl.fw_err_il1, (unsigned)gAwdl.fw_err_hcmd,
        gAwdl.tx_sta, gAwdl.tx_sta_id,
        awdl_sta_type_name(gAwdl.tx_sta_type),
        gAwdl.tx_tid, gAwdl.tx_qid, gAwdl.tx_sta_wanted,
        gAwdl.prot > 0 ? " prot" : "",
        (unsigned)gAwdl.fw_err_u_id, (unsigned)gAwdl.fw_err_u_il1,
        (unsigned)gAwdl.fw_err_u_d1, (unsigned)gAwdl.fw_err_u_d2,
        (unsigned)gAwdl.fw_err_u_d3, (unsigned)gAwdl.fw_err_u_ch,
        (unsigned)gAwdl.fw_err_l_d1, (unsigned)gAwdl.fw_err_l_d2,
        (unsigned)gAwdl.fw_err_l_d3,
        gAwdl.session, gAwdl.cell, gAwdl.cell_wanted, gAwdl.channel,
        gAwdl.prot, gAwdl.tlc, gAwdl.notx_wanted);
    if (n < 0)
        return EIO;
    /* v2.7.1: a truncated snprintf returns the would-be length, which
     * would walk sysctl_handle_string past buf (XNU reads -- and on
     * some paths writes -- arg1[arg2-1]). The string is NUL-terminated
     * at buf[sizeof(buf)-1] either way; hand it no more than that. */
    if (n >= (int)sizeof(buf))
        n = (int)sizeof(buf) - 1;
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
SYSCTL_PROC(_debug, OID_AUTO, itlawdl_txsta, CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_LOCKED | CTLFLAG_KERN,
            NULL, 0, awdl_txsta_sysctl, "I",
            "inject queue station binding: 0 monitor (v2.5), 1 sta0 LINK (default), 2 aux, 3 sta0 mcast (v2.7.2), 4 snif_sta tid 8 (v2.8), 5 sta0 gp bcast tid 15 (v2.8.1), 6 = 5 + session protection, 7 sta0 LINK via TVQM (v2.8.2), 8 = 7 + TLC rate-scaling (v2.8.3), 9 = new-API queue config SCD_QUEUE_CONFIG_CMD (v2.8.4) -- read at session start");
SYSCTL_PROC(_debug, OID_AUTO, itlawdl_notx, CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_LOCKED | CTLFLAG_KERN,
            NULL, 0, awdl_notx_sysctl, "I",
            "v2.8.1: 1 = hold injected frames before the TFD (the session without emission, kext-side; held= counts) -- read live");
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
        gAwdl.tx_sta_wanted = 1;    /* v2.6 default: the STA-mode shape */
        gAwdl.tx_qid = 2;           /* v2.8.2: default before any session */
        sysctl_register_oid(&sysctl__debug_itlawdl);
        sysctl_register_oid(&sysctl__debug_itlawdl_cell);
        sysctl_register_oid(&sysctl__debug_itlawdl_txsta);
        sysctl_register_oid(&sysctl__debug_itlawdl_notx);
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
        sysctl_unregister_oid(&sysctl__debug_itlawdl_txsta);
        sysctl_unregister_oid(&sysctl__debug_itlawdl_notx);
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
