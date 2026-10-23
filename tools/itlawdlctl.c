/*
 * itlawdlctl -- userspace side of the itlawdl v2 session (AirportItlwm).
 *
 * Talks to the kext through sysctls:
 *   debug.itlawdl        0 stop / 1 listen / 2 cell     (int rw)
 *   debug.itlawdl_cell   pin fw mac type 0/2/3/4        (int rw)
 *   debug.itlawdl_stats  counters                       (string ro)
 *   debug.itlawdl_rx     drain frames                   (opaque ro)
 *   debug.itlawdl_tx     inject a frame                 (opaque wo)
 *
 * RX record (16-byte header, little endian, then the frame, no FCS):
 *   u16 len; u8 rate (500 kb/s units, or 0x80|mcs);
 *   i8 rssi; u8 flags (bit0 short preamble); u8 chan; u16 rsv; u64 tsft.
 *
 * TX request (8 bytes, then the frame):
 *   u8 ver=2; u8 flags (bit0 expect ack, bit1 ht mcs); u8 rate; u8 pad;
 *   u16 frame_len; u16 rsv.
 *
 * Build: clang -O2 -Wall -o itlawdlctl itlawdlctl.c
 * Usage: itlawdlctl status | up [listen|cell] [ibss|pibss|listener]
 *        | down | cell [0|2|3|4] | rx [n] [--hex] [--out FILE]
 *        | tx --hex <frame> [--rate R] [--ht] [--ack]
 *        | tx-probe [--ssid NAME] [--rate R] [--ack]
 *        | tx-awdl [--src MAC] [--rate R] [--ack]
 */

#include <errno.h>
#include <ifaddrs.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/sysctl.h>
#include <net/if_dl.h>
#include <net/if_types.h>
#include <unistd.h>

#define AWDL_BSSID "\x00\x25\x00\xff\x94\x73"
#define AWDL_OUI "\x00\x17\xf2"
#define RXREC_HDR 16
#define MAX_FRAME 2368

static int hexval(int c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

static int parse_hex(const char *s, uint8_t *out, int max)
{
    int n = 0, hi = -1;
    for (; *s; s++) {
        if (*s == ' ' || *s == ':' || *s == '-' || *s == '\n' || *s == '\t')
            continue;
        int v = hexval((unsigned char)*s);
        if (v < 0) return -1;
        if (hi < 0) { hi = v; continue; }
        if (n >= max) return -1;
        out[n++] = (uint8_t)((hi << 4) | v);
        hi = -1;
    }
    return (hi < 0) ? n : -1;
}

static void print_hex(const uint8_t *p, int n)
{
    for (int i = 0; i < n; i++)
        printf("%02x", p[i]);
}

/* reads a sysctl into a malloc'd buffer */
static void *sysctl_read(const char *name, size_t *len)
{
    void *buf = NULL;
    size_t l = 0;
    if (sysctlbyname(name, NULL, &l, NULL, 0) == 0 && l) {
        buf = malloc(l);
        if (buf && sysctlbyname(name, buf, &l, NULL, 0) == 0) {
            *len = l;
            return buf;
        }
        free(buf);
    }
    /* some sysctls need a guess */
    l = 64 * 1024;
    buf = malloc(l);
    if (buf && sysctlbyname(name, buf, &l, NULL, 0) == 0) {
        *len = l;
        return buf;
    }
    free(buf);
    return NULL;
}

static int get_state(void)
{
    int v = -1;
    size_t l = sizeof(v);
    if (sysctlbyname("debug.itlawdl", &v, &l, NULL, 0) != 0)
        return -2;    /* kext not loaded */
    return v;
}

static int wifi_mac(const char *ifname, uint8_t *mac)
{
    struct ifaddrs *ifa, *p;
    if (getifaddrs(&ifa) != 0)
        return -1;
    for (p = ifa; p; p = p->ifa_next) {
        if (p->ifa_addr && p->ifa_addr->sa_family == AF_LINK &&
            strcmp(p->ifa_name, ifname) == 0) {
            struct sockaddr_dl *sdl = (struct sockaddr_dl *)(void *)p->ifa_addr;
            if (sdl->sdl_type == IFT_ETHER && sdl->sdl_alen == 6) {
                memcpy(mac, LLADDR(sdl), 6);
                freeifaddrs(ifa);
                return 0;
            }
        }
    }
    freeifaddrs(ifa);
    return -1;
}

static void cmd_status(void)
{
    size_t len = 0;
    int st = get_state();
    if (st == -2) {
        printf("itlawdl: kext not loaded (no debug.itlawdl sysctl)\n");
        exit(1);
    }
    printf("state=%d (%s)\n", st,
           st == 0 ? "off" : st == 1 ? "listen" : st == 2 ? "cell" : "?");
    void *buf = sysctl_read("debug.itlawdl_stats", &len);
    if (buf) {
        fwrite(buf, 1, len > 0 ? len - 1 : 0, stdout);
        printf("\n");
        free(buf);
    }
}

static void cmd_up(const char *mode, const char *pin)
{
    int cell = 0, target;
    size_t l;
    if (!mode) mode = "cell";
    if (!strcmp(mode, "listen")) target = 1;
    else if (!strcmp(mode, "cell")) target = 2;
    else { fprintf(stderr, "mode: listen|cell\n"); exit(2); }
    if (pin) {
        if (!strcmp(pin, "ibss")) cell = 4;
        else if (!strcmp(pin, "pibss")) cell = 3;
        else if (!strcmp(pin, "listener")) cell = 2;
        else { fprintf(stderr, "pin: ibss|pibss|listener\n"); exit(2); }
        l = sizeof(cell);
        if (sysctlbyname("debug.itlawdl_cell", NULL, NULL, &cell, l) != 0)
            perror("debug.itlawdl_cell");
    }
    l = sizeof(target);
    if (sysctlbyname("debug.itlawdl", NULL, NULL, &target, l) != 0) {
        perror("debug.itlawdl");
        exit(1);
    }
    /* the driver applies it on its own thread; poll for the outcome
     * (up to 5 s: the first enable can load firmware) */
    for (int i = 0; i < 50; i++) {
        int st = get_state();
        if (st == target || st < 0)
            break;
        usleep(100 * 1000);
    }
    printf("-> state %d\n", get_state());
}

static void cmd_down(void)
{
    int v = 0;
    if (sysctlbyname("debug.itlawdl", NULL, NULL, &v, sizeof(v)) != 0) {
        perror("debug.itlawdl");
        exit(1);
    }
    for (int i = 0; i < 50; i++) {
        int st = get_state();
        if (st == 0 || st < 0)
            break;
        usleep(100 * 1000);
    }
    printf("-> state %d\n", get_state());
}

static void cmd_cell(const char *arg)
{
    int v = arg ? atoi(arg) : 0;
    if (sysctlbyname("debug.itlawdl_cell", NULL, NULL, &v, sizeof(v)) != 0) {
        perror("debug.itlawdl_cell");
        exit(1);
    }
    size_t len = 0;
    void *buf = sysctl_read("debug.itlawdl_stats", &len);
    if (buf) { fwrite(buf, 1, len ? len - 1 : 0, stdout); free(buf); }
}

static void cmd_rx(int count, int hex, const char *out)
{
    FILE *f = out ? fopen(out, "wb") : NULL;
    if (out && !f) { perror(out); exit(1); }
    int shown = 0;
    while (count < 0 || shown < count) {
        size_t len = 0;
        void *buf = sysctl_read("debug.itlawdl_rx", &len);
        if (!buf) { fprintf(stderr, "debug.itlawdl_rx: %s\n", strerror(errno)); exit(1); }
        if (len == 0) { free(buf); break; }
        size_t off = 0;
        while (off + RXREC_HDR <= len) {
            uint8_t *r = (uint8_t *)buf + off;
            uint16_t flen = (uint16_t)(r[0] | (r[1] << 8));
            if (off + RXREC_HDR + flen > len)
                break;    /* truncated tail: stop here */
            uint8_t *frame = r + RXREC_HDR;
            if (f) fwrite(r, 1, RXREC_HDR + flen, f);
            uint64_t tsft;
            memcpy(&tsft, r + 8, 8);
            printf("[%d] len=%-4u rate=%-3u rssi=%-4d chan=%-2u flags=%x tsft=%llu ",
                   shown, flen, r[2], (int8_t)r[3], r[5], r[4],
                   (unsigned long long)tsft);
            if (flen >= 24) {
                printf("fc=%02x%02x a1=", frame[0], frame[1]);
                print_hex(frame + 4, 6);
                printf(" a2=");
                print_hex(frame + 10, 6);
                printf(" a3=");
                print_hex(frame + 16, 6);
                if (hex) { printf(" frame="); print_hex(frame, flen > 64 ? 64 : flen); }
            }
            printf("\n");
            off += RXREC_HDR + flen;
            shown++;
            if (count >= 0 && shown >= count) break;
        }
        free(buf);
        if (count >= 0 && shown >= count) break;
        if (len < 64 * 1024 - 2384) break;    /* drained for now */
    }
    if (f) { fclose(f); printf("raw stream -> %s\n", out); }
    printf("(%d frames)\n", shown);
}

static void inject(const uint8_t *frame, int flen, int rate, int ht, int ack)
{
    uint8_t req[8 + MAX_FRAME];
    int reqlen = 8 + flen;
    req[0] = 2;
    req[1] = (uint8_t)((ht ? 0x02 : 0) | (ack ? 0x01 : 0));
    req[2] = (uint8_t)rate;
    req[3] = 0;
    req[4] = (uint8_t)(flen & 0xff);
    req[5] = (uint8_t)(flen >> 8);
    req[6] = req[7] = 0;
    memcpy(req + 8, frame, flen);
    if (sysctlbyname("debug.itlawdl_tx", NULL, NULL, req, reqlen) != 0) {
        perror("debug.itlawdl_tx");
        exit(1);
    }
    printf("injected %d bytes (rate=%d%s%s)\n", flen, rate,
           ht ? " ht" : "", ack ? " ack" : "");
}

static void cmd_tx(int argc, char **argv)
{
    const char *hex = NULL;
    int rate = 0, ht = 0, ack = 0;
    uint8_t frame[MAX_FRAME];
    for (int i = 0; i < argc; i++) {
        if (!strcmp(argv[i], "--hex") && i + 1 < argc) hex = argv[++i];
        else if (!strcmp(argv[i], "--rate") && i + 1 < argc) rate = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--ht")) ht = 1;
        else if (!strcmp(argv[i], "--ack")) ack = 1;
        else { fprintf(stderr, "unknown arg %s\n", argv[i]); exit(2); }
    }
    if (!hex) { fprintf(stderr, "tx needs --hex <frame>\n"); exit(2); }
    int n = parse_hex(hex, frame, sizeof(frame));
    if (n < 10) { fprintf(stderr, "bad hex frame (%d)\n", n); exit(2); }
    inject(frame, n, rate, ht, ack);
}

/* a probe request with a distinctive SSID: for validating injection on
 * a second machine (Linux live USB + owly capture) -- it just has to be
 * visible on channel 6, nothing answers it. */
static void cmd_tx_probe(int argc, char **argv)
{
    const char *ssid = "ITLAWDL-PROBE";
    int rate = 0, ack = 0;
    uint8_t mac[6];
    uint8_t frame[MAX_FRAME];
    for (int i = 0; i < argc; i++) {
        if (!strcmp(argv[i], "--ssid") && i + 1 < argc) ssid = argv[++i];
        else if (!strcmp(argv[i], "--rate") && i + 1 < argc) rate = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--ack")) ack = 1;
        else { fprintf(stderr, "unknown arg %s\n", argv[i]); exit(2); }
    }
    if (wifi_mac("en0", mac) != 0) {
        memset(mac, 0x02, 6);    /* locally administered, made up */
        fprintf(stderr, "warning: en0 MAC not found, using 02:02:02:02:02:02\n");
    }
    int slen = (int)strlen(ssid);
    if (slen > 32) slen = 32;
    int n = 0;
    uint8_t *f = frame;
    f[n++] = 0x40; f[n++] = 0x00;                    /* fc: probe request */
    f[n++] = 0; f[n++] = 0;                          /* duration */
    memset(f + n, 0xff, 6); n += 6;                  /* addr1 broadcast */
    memcpy(f + n, mac, 6); n += 6;                   /* addr2 = us */
    memset(f + n, 0xff, 6); n += 6;                  /* addr3 broadcast */
    f[n++] = 0; f[n++] = 0;                          /* seq */
    f[n++] = 0; f[n++] = (uint8_t)slen;              /* SSID IE */
    memcpy(f + n, ssid, slen); n += slen;
    f[n++] = 1; f[n++] = 4;                          /* rates IE: 6,12,24,54? (x2: 12,24,48,108) */
    f[n++] = 12; f[n++] = 24; f[n++] = 48; f[n++] = 108;
    f[n++] = 50; f[n++] = 4;                         /* ext rates? no: 50 = ext supp rates IE */
    f[n++] = 12; f[n++] = 24; f[n++] = 48; f[n++] = 108;
    printf("probe request, %d bytes, src=", n);
    print_hex(mac, 6);
    printf(" ssid=%s\n", ssid);
    inject(frame, n, rate ? rate : 24, 0, ack);
}

/* an AWDL-shaped vendor action frame (AWDL-3): broadcast, AWDL's BSSID,
 * Apple's OUI, type 8, version 0x10, subtype 0 (sync). No TLVs: the
 * point is to see our own bytes on air; the iPhone ignores it. */
static void cmd_tx_awdl(int argc, char **argv)
{
    const char *src = NULL;
    int rate = 0, ack = 0;
    uint8_t mac[6], bssid[6], oui[3];
    uint8_t frame[MAX_FRAME];
    for (int i = 0; i < argc; i++) {
        if (!strcmp(argv[i], "--src") && i + 1 < argc) src = argv[++i];
        else if (!strcmp(argv[i], "--rate") && i + 1 < argc) rate = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--ack")) ack = 1;
        else { fprintf(stderr, "unknown arg %s\n", argv[i]); exit(2); }
    }
    if (src) {
        uint8_t tmp[6];
        if (parse_hex(src, tmp, 6) != 6) { fprintf(stderr, "bad --src\n"); exit(2); }
        memcpy(mac, tmp, 6);
    } else if (wifi_mac("en0", mac) != 0) {
        memset(mac, 0x02, 6);
        fprintf(stderr, "warning: en0 MAC not found, using 02:02:02:02:02:02\n");
    }
    memcpy(bssid, AWDL_BSSID, 6);
    memcpy(oui, AWDL_OUI, 3);
    int n = 0;
    uint8_t *f = frame;
    f[n++] = 0xd0; f[n++] = 0x00;                    /* fc: action */
    f[n++] = 0; f[n++] = 0;                          /* duration */
    memset(f + n, 0xff, 6); n += 6;                  /* addr1 broadcast */
    memcpy(f + n, mac, 6); n += 6;                   /* addr2 = us */
    memcpy(f + n, bssid, 6); n += 6;                 /* addr3 = AWDL BSSID (AWDL-1) */
    f[n++] = 0; f[n++] = 0;                          /* seq */
    f[n++] = 127;                                    /* category vendor */
    memcpy(f + n, oui, 3); n += 3;                   /* Apple OUI */
    f[n++] = 8;                                      /* AWDL type */
    f[n++] = 0x10;                                   /* version compat */
    f[n++] = 0;                                      /* subtype: sync */
    f[n++] = 0;                                      /* pad */
    f[n++] = 0; f[n++] = 0; f[n++] = 0; f[n++] = 0;  /* phy tx time */
    f[n++] = 0; f[n++] = 0; f[n++] = 0; f[n++] = 0;  /* target tx time */
    printf("awdl action frame, %d bytes, src=", n);
    print_hex(mac, 6);
    printf(" bssid=");
    print_hex(bssid, 6);
    printf("\n");
    inject(frame, n, rate ? rate : 24, 0, ack);
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr,
            "usage: %s status | up [listen|cell] [ibss|pibss|listener] | down\n"
            "       %s cell [0|2|3|4]\n"
            "       %s rx [n] [--hex] [--out FILE]\n"
            "       %s tx --hex <frame> [--rate R] [--ht] [--ack]\n"
            "       %s tx-probe [--ssid S] [--rate R] [--ack]\n"
            "       %s tx-awdl [--src MAC] [--rate R] [--ack]\n",
            argv[0], argv[0], argv[0], argv[0], argv[0], argv[0]);
        return 2;
    }
    if (!strcmp(argv[1], "status")) cmd_status();
    else if (!strcmp(argv[1], "up")) cmd_up(argc > 2 ? argv[2] : NULL,
                                            argc > 3 ? argv[3] : NULL);
    else if (!strcmp(argv[1], "down")) cmd_down();
    else if (!strcmp(argv[1], "cell")) cmd_cell(argc > 2 ? argv[2] : "0");
    else if (!strcmp(argv[1], "rx")) {
        int count = -1, hex = 0;
        const char *out = NULL;
        for (int i = 2; i < argc; i++) {
            if (!strcmp(argv[i], "--hex")) hex = 1;
            else if (!strcmp(argv[i], "--out") && i + 1 < argc) out = argv[++i];
            else { count = atoi(argv[i]); if (count <= 0) count = -1; }
        }
        cmd_rx(count, hex, out);
    }
    else if (!strcmp(argv[1], "tx")) cmd_tx(argc - 2, argv + 2);
    else if (!strcmp(argv[1], "tx-probe")) cmd_tx_probe(argc - 2, argv + 2);
    else if (!strcmp(argv[1], "tx-awdl")) cmd_tx_awdl(argc - 2, argv + 2);
    else { fprintf(stderr, "unknown command %s\n", argv[1]); return 2; }
    return 0;
}
