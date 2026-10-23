//
//  AwdlProbe.hpp
//  itlwm
//
//  AWDL listen-only probe (step 1 of the AirDrop-on-Intel work).
//
//  With Wi-Fi turned OFF in macOS, `sudo sysctl debug.itlawdl=1` restarts the
//  firmware as a listener (monitor MAC context, the code path inherited from
//  OpenBSD) on channel 6 and counts the AWDL frames it hears: action frames
//  (category 127, OUI 00:17:f2, type 8) and data frames on AWDL's BSSID
//  00:25:00:ff:94:73. Nothing is transmitted. `sysctl debug.itlawdl=0`, or
//  turning Wi-Fi back on, restores station mode.
//
//  Read the counters with `sysctl debug.itlawdl_stats`.
//
//  Included once, from ItlIwx.cpp.
//

#ifndef AwdlProbe_hpp
#define AwdlProbe_hpp

#include <sys/sysctl.h>
#include <kern/thread_call.h>
#include <kern/clock.h>

#define AWDL_PROBE_CHANNEL      6
#define AWDL_PROBE_MAX_PEERS    8

struct awdl_probe_peer {
    uint8_t  mac[6];
    uint32_t frames;
    uint32_t psf;
    uint32_t mif;
    uint32_t data;
    int8_t   last_rssi;
    int8_t   best_rssi;
};

struct awdl_probe_state {
    ItlIwx *iwx;                /* the attached card, or NULL */
    volatile int active;        /* 1 while the firmware listens for AWDL */
    volatile int entering;      /* set while awdlProbeSet() brings it up */
    int state;                  /* what `sysctl debug.itlawdl` reads */
    int want;                   /* what the last write asked for */
    thread_call_t call;
    uint64_t since_ns;

    /* counters, written from the RX path */
    uint64_t rx_all;            /* every frame the listener passed up */
    uint64_t rx_mgmt, rx_ctrl, rx_data;
    uint64_t awdl_action;       /* AWDL action frames, any subtype */
    uint64_t awdl_psf;          /* subtype 0: periodic synchronization */
    uint64_t awdl_mif;          /* subtype 3: master indication */
    uint64_t awdl_other;        /* other AWDL subtypes */
    uint64_t awdl_data;         /* data frames on AWDL's BSSID */
    uint64_t awdl_data_bytes;
    uint32_t chan_seen[15];     /* frames per 2.4 GHz channel index */
    uint32_t chan_other;
    struct awdl_probe_peer peers[AWDL_PROBE_MAX_PEERS];
    uint32_t peers_dropped;
};

static struct awdl_probe_state gAwdl;

static const uint8_t kAwdlBssid[6] = { 0x00, 0x25, 0x00, 0xff, 0x94, 0x73 };
static const uint8_t kAppleOui[3]  = { 0x00, 0x17, 0xf2 };

static struct awdl_probe_peer *
awdl_probe_peer(const uint8_t *mac)
{
    for (int i = 0; i < AWDL_PROBE_MAX_PEERS; i++) {
        struct awdl_probe_peer *p = &gAwdl.peers[i];
        if (p->frames && memcmp(p->mac, mac, 6) == 0)
            return p;
    }
    for (int i = 0; i < AWDL_PROBE_MAX_PEERS; i++) {
        struct awdl_probe_peer *p = &gAwdl.peers[i];
        if (!p->frames) {
            memcpy(p->mac, mac, 6);
            p->best_rssi = -128;
            return p;
        }
    }
    gAwdl.peers_dropped++;
    return NULL;
}

/* Called for every received frame while the probe is active. */
static void
awdl_probe_rx(const uint8_t *f, size_t len, int rssi, int chanidx)
{
    gAwdl.rx_all++;
    if (chanidx >= 0 && chanidx < 15)
        gAwdl.chan_seen[chanidx]++;
    else
        gAwdl.chan_other++;
    if (len < 2)
        return;

    uint8_t type = (f[0] >> 2) & 3;
    uint8_t subtype = f[0] >> 4;
    if (type == 1) { gAwdl.rx_ctrl++; return; }
    if (len < 24) return;
    if (type == 0) gAwdl.rx_mgmt++;
    if (type == 2) gAwdl.rx_data++;

    const uint8_t *ta = f + 10;
    struct awdl_probe_peer *p = NULL;

    if (type == 0 && subtype == 13 && len >= 31 &&
        f[24] == 127 && memcmp(f + 25, kAppleOui, 3) == 0 && f[28] == 8) {
        gAwdl.awdl_action++;
        p = awdl_probe_peer(ta);
        switch (f[30]) {
            case 0: gAwdl.awdl_psf++; if (p) p->psf++; break;
            case 3: gAwdl.awdl_mif++; if (p) p->mif++; break;
            default: gAwdl.awdl_other++; break;
        }
    } else if (type == 2 && memcmp(f + 16, kAwdlBssid, 6) == 0) {
        gAwdl.awdl_data++;
        gAwdl.awdl_data_bytes += len;
        p = awdl_probe_peer(ta);
        if (p) p->data++;
    }

    /* The RX path hands RSSI normalised as 100 + dBm (IWX_MIN_DBM); store dBm. */
    rssi += IWX_MIN_DBM;
    if (p) {
        p->frames++;
        p->last_rssi = (int8_t)rssi;
        if ((int8_t)rssi > p->best_rssi)
            p->best_rssi = (int8_t)rssi;
    }
}

static void
awdl_probe_reset_counters()
{
    gAwdl.rx_all = gAwdl.rx_mgmt = gAwdl.rx_ctrl = gAwdl.rx_data = 0;
    gAwdl.awdl_action = gAwdl.awdl_psf = gAwdl.awdl_mif = gAwdl.awdl_other = 0;
    gAwdl.awdl_data = gAwdl.awdl_data_bytes = 0;
    memset(gAwdl.chan_seen, 0, sizeof(gAwdl.chan_seen));
    gAwdl.chan_other = 0;
    memset(gAwdl.peers, 0, sizeof(gAwdl.peers));
    gAwdl.peers_dropped = 0;
    uint64_t now;
    clock_get_uptime(&now);
    absolutetime_to_nanoseconds(now, &gAwdl.since_ns);
}

/* Probe states, as `sysctl debug.itlawdl` reads them. */
#define AWDL_STATE_OFF          0
#define AWDL_STATE_LISTENING    1
#define AWDL_STATE_BUSY_WIFI_ON (-1)   /* refused: turn Wi-Fi off first */
#define AWDL_STATE_NO_CARD      (-2)
#define AWDL_STATE_FAILED       (-3)

static void
awdl_probe_work(thread_call_param_t, thread_call_param_t)
{
    ItlIwx *iwx = gAwdl.iwx;
    if (!iwx) {
        gAwdl.state = AWDL_STATE_NO_CARD;
        return;
    }
    gAwdl.state = iwx->awdlProbeSet(gAwdl.want);
}

static int
awdl_probe_sysctl SYSCTL_HANDLER_ARGS
{
    int v = gAwdl.state;
    int err = sysctl_handle_int(oidp, &v, 0, req);
    if (err || !req->newptr)
        return err;
    if (v != 0 && v != 1)
        return EINVAL;
    if (!gAwdl.iwx)
        return ENODEV;
    gAwdl.want = v;
    if (!gAwdl.call)
        gAwdl.call = thread_call_allocate(awdl_probe_work, nullptr);
    if (!gAwdl.call)
        return ENOMEM;
    thread_call_enter(gAwdl.call);
    return 0;
}

static int
awdl_probe_stats_sysctl SYSCTL_HANDLER_ARGS
{
    char buf[1536];
    int n = 0;
    uint64_t now, now_ns;
    clock_get_uptime(&now);
    absolutetime_to_nanoseconds(now, &now_ns);
    uint64_t secs = gAwdl.active ? (now_ns - gAwdl.since_ns) / 1000000000ULL : 0;

#define AP_OUT(...) do { if (n < (int)sizeof(buf)) n += snprintf(buf + n, sizeof(buf) - n, __VA_ARGS__); } while (0)
    AP_OUT("state=%d listening=%d seconds=%llu channel=%d\n",
           gAwdl.state, gAwdl.active, secs, AWDL_PROBE_CHANNEL);
    AP_OUT("rx all=%llu mgmt=%llu ctrl=%llu data=%llu\n",
           gAwdl.rx_all, gAwdl.rx_mgmt, gAwdl.rx_ctrl, gAwdl.rx_data);
    AP_OUT("awdl action=%llu psf=%llu mif=%llu other=%llu data=%llu data_bytes=%llu\n",
           gAwdl.awdl_action, gAwdl.awdl_psf, gAwdl.awdl_mif, gAwdl.awdl_other,
           gAwdl.awdl_data, gAwdl.awdl_data_bytes);
    AP_OUT("chan");
    for (int i = 1; i < 15; i++)
        if (gAwdl.chan_seen[i])
            AP_OUT(" %d:%u", i, gAwdl.chan_seen[i]);
    AP_OUT(" other:%u\n", gAwdl.chan_other);
    for (int i = 0; i < AWDL_PROBE_MAX_PEERS; i++) {
        struct awdl_probe_peer *p = &gAwdl.peers[i];
        if (!p->frames)
            continue;
        AP_OUT("peer %02x:%02x:%02x:%02x:%02x:%02x frames=%u psf=%u mif=%u data=%u rssi=%ddBm best=%ddBm\n",
               p->mac[0], p->mac[1], p->mac[2], p->mac[3], p->mac[4], p->mac[5],
               p->frames, p->psf, p->mif, p->data, p->last_rssi, p->best_rssi);
    }
    if (gAwdl.peers_dropped)
        AP_OUT("peers_dropped=%u\n", gAwdl.peers_dropped);
#undef AP_OUT
    if (n >= (int)sizeof(buf))
        n = sizeof(buf) - 1;
    buf[n] = 0;
    return sysctl_handle_string(oidp, buf, n + 1, req);
}

SYSCTL_PROC(_debug, OID_AUTO, itlawdl, CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_LOCKED | CTLFLAG_KERN,
            NULL, 0, awdl_probe_sysctl, "I",
            "itlwm AWDL listen probe: write 1 (Wi-Fi off) to listen on channel 6, 0 to stop; reads the state");
SYSCTL_PROC(_debug, OID_AUTO, itlawdl_stats, CTLTYPE_STRING | CTLFLAG_RD | CTLFLAG_LOCKED,
            NULL, 0, awdl_probe_stats_sysctl, "A",
            "itlwm AWDL listen probe counters");

static bool gAwdlSysctlRegistered;

static void
awdl_probe_attach(ItlIwx *iwx)
{
    gAwdl.iwx = iwx;
    gAwdl.state = AWDL_STATE_OFF;
    if (!gAwdlSysctlRegistered) {
        sysctl_register_oid(&sysctl__debug_itlawdl);
        sysctl_register_oid(&sysctl__debug_itlawdl_stats);
        gAwdlSysctlRegistered = true;
    }
}

static void
awdl_probe_detach(ItlIwx *iwx)
{
    if (gAwdl.iwx != iwx)
        return;
    if (gAwdlSysctlRegistered) {
        sysctl_unregister_oid(&sysctl__debug_itlawdl);
        sysctl_unregister_oid(&sysctl__debug_itlawdl_stats);
        gAwdlSysctlRegistered = false;
    }
    if (gAwdl.call) {
        thread_call_cancel_wait(gAwdl.call);
        thread_call_free(gAwdl.call);
        gAwdl.call = NULL;
    }
    gAwdl.active = 0;
    gAwdl.iwx = NULL;
}

#endif /* AwdlProbe_hpp */
