# owlyd-mac — the AWDL engine on macOS, with the AirDrop receiver

The engine that makes the Mac take part in AirDrop's link layer: owly's
AWDL node (vendored, MIT) run on the radio the itlawdl kext of this
repository provides — the PIBSS cell on channel 6, its RX ring, its
inject queue — with a utun interface for the IPv6 the peers exchange.
Since v0.1.5 the AirDrop receiver rides on it too: the `_airdrop._tcp`
records announced over mDNS in bursts, and the TLS `/Discover` on port
8770 that an iPhone requires before it lists anyone — the whole receive
flow, ported from owly's owly-airdrop (MIT), validated on Linux against
iOS 26.

## Run

    sudo owlyd-mac

It cuts the Wi-Fi (the cell needs the interface down), brings the cell
up, proves it can hear (12 s, the counter sampled every 2 s), runs the
engine, announces `« <the Mac's name> »` over AirDrop, prints peers and
the kext's counters every 10 s, and prompts on the terminal when an
iPhone sends: answer `o` to accept. Received files land in the directory
you launched from, given back to your user (not root). Ctrl-C stops it
and puts everything back.

Requirements: the AirportItlwm kext of this repository (v2.6 or later
for `fw_err=` in the stats — the gate's honest witness; v2.5 runs
everything else but the gate judges on rx alone; branch awdl-v2)
loaded, and nothing else holding the radio. The self-signed identity
persists in `/var/db/owlyd-mac`, so the phone keeps recognising the Mac
between sessions.

## Testing against an iPhone

1. iPhone: unlocked, AirDrop receiving set to *Everyone* (Contacts Only
   filters unknown receivers out before they can be listed), the share
   sheet or the AirDrop picker open — that is what makes the phone's
   AWDL radio active at all.
2. Stay next to the Mac; the phone visits channel 6 a slot or two in
   sixteen, so give the announcement bursts ~15 s.
3. What the log tells you, in order: `porte TX` climbing the ramp with
   rx moving at every step (the radio surviving our own emission),
   `tx_multicast` growing (the engine carrying the announcements and the
   mDNS replies as AWDL data frames), `— AirDrop : vu par [..]` (an
   iPhone's `/Discover` answered — the name should be in its list), then
   the ask prompt when someone taps. If `porte TX` says the radio died,
   the line names the pace that killed it and what the firmware said
   (`fw_err`, `fwid=`/`il1=`/`hcmd=`) — that log is the next fix's brief.

## Layout

    vendor/owly-awdl      owly's sans-IO AWDL crate (MIT); two documented
                          deviations in its driver: Running::stop waits at
                          most 2 s, and the engine loop publishes its
                          iteration count (a liveness heartbeat)
    vendor/owly-airdrop   owly's AirDrop crate (MIT); one documented
                          deviation in its mDNS driver: an outbox lane for
                          every egress, for a kernel that refuses to
                          carry multicast off the utun — the codec, the
                          TLS receiver (/Discover, /Ask, /Upload) and the
                          sandboxed extraction are upstream's
    owlyd-mac             the daemon: sysctl bindings, the Air (a reader
                          thread over the kext's RX ring + a writer thread
                          for the injects), the utun Netif (a reader thread
                          of its own — AsyncFd's kqueue readiness on the
                          kernel-control socket starved the whole engine in
                          the v03 field run), and the AirDrop wiring

## Field notes

The v03 run froze the engine after its first MIF: no STATS line ever,
not one peer from a ring the reader kept feeding. Two causes, both
fixed in v0.1.3: the utun's AsyncFd reads (a cached-readiness spin
inside the driver's biased select) and a zombie session from a
brutally killed run (the OFF→CELL sysctl pair collapsed, and the new
session inherited the dead one's ring).

The v04 run proved the engine alive — STATS every 10 s, ~26 loop
iterations a second, 24 frames/s built and injected — and uncovered
what only a live engine could: the kext's inject queue never sees its
completions (330 TFDs, then ENOBUFS forever: the Mac goes silent ~14 s
in), and a cell can come up deaf (90 s of rx=0, not even infra
beacons — every session that ever received was brought up right after
a full Wi-Fi power cycle). v0.1.4 listens for 12 s after the cell goes
up and, hearing nothing, recycles the radio (Wi-Fi on → off) and tries
again; itlawdl v2.4 reclaims the inject queue's slots when the ring
comes back around, and an enable() that fails now fails the session
instead of leaving a deaf one up.

The v05 run: the inject queue held (2171 frames, no errors) and the
cell heard during the 12 s check — and then rx froze at 51 for 100 s,
not one frame after the engine started, with no mDNS to answer for
either (tx_multicast 0: nothing announced, so no phone could have
listed the Mac). v0.1.5 announces (the missing half of AirDrop), and
samples the rx counter every 2 s during the listen, so the next run
says *when* the radio died: stalled during the listen is a dying cell
(the kext's to fix), stalled exactly at the engine's first frames is
our own TX choking the firmware's receive.

The v06 run answered that question, and it was the second: rx grew
steadily through the 12 s listen (6→10→15→19→25→29) and stood at 29 for
the whole session — dead within the engine's first ten seconds, while
~240 frames went out. And the announcements never left: `utun : 0
paquets lus au total` while ~1700 left the mDNS socket, every send's
error thrown away — macOS never routed one multicast packet into the
utun. v0.1.6 answers both: the engine's frames now pass a ramp (1, 4,
12 a second, ten seconds each, the kext's rx read at every boundary —
the pace that kills the radio names the fix, and the last healthy one
is held for the session), and the announcements ride a direct lane into
the engine's netif queue (the socket stays for queries, and says its
errors out loud now). itlawdl v2.5 counts the inject queue's TX
completions (`done=`) and the v2.4 slot take-backs (`recycled=`) — with
`ok=` they say whether the firmware ever touched what we queued — and
stops setting a pre-AX210 ACK bit the gen3 TX command reserves.

The v08 run (kext v2.6, binding 1) settled it: ok=10, done=0, rx dead
at the very first TFD, and fw_err=2 — the firmware's own error dumps.
itlawdl v2.7 found the poison in the code: the inject path never set
the TX command's rate byte on this firmware (VER_8), so every legacy
frame ever injected left with rate_n_flags = antennas | a stack byte.
A rate the PHY cannot honour takes the whole MAC down. v2.7 sets it
(hwrate = the PLCP code; `txrate=` in the stats proves it in every
log).

v0.2.0 is the full-code audit's release. Beyond the kext's two safety
fixes (v2.7.1: the stats buffer's length is clamped — a truncated
snprintf could walk sysctl_handle_string past it — and awdlInject
demands a live session), the daemon learned three things the audit
found: the mDNS REPLIES to a phone's queries rode the socket path the
v06 run proved dead (only the announcements had the direct lane — all
egress rides it now: announcements, replies, goodbyes); the gate judged
a quiet channel dead (the field's channel 6 runs at 5 frames in 12 s —
a healthy radio can look silent for a window, so the verdict now needs
the firmware's own word: `fw_err` cuts at once, and rx silence holds
the step, twice in a row being the only rx-only death — a cut that
hears rx again re-opens the ramp, thrice at most); and in free run
nothing watched the firmware anymore (an error dump there now falls
back to the last proven pace, and the monitor's line carries rx, and
fw_err at every boundary).

v0.2.1 rides the v10 run's verdict. The binding-2 test (aux station)
cleared the last two suspects — txrate was valid and the first frame
finally survived the firmware — but the UMAC still died within ten,
rx frozen. What the firmware never had is a station typed for
broadcast: iwlwifi gives every vif an IWL_STA_MULTICAST one and itlwm
never creates one. The kext's v2.7.2 adds binding 3
(`sysctl -w debug.itlawdl_txsta=3`: station 0 typed MULTICAST,
address ff:ff:ff:ff:ff:ff, queue and tid untouched — the single-
variable experiment against binding 1), feeds the error dump's UMAC
table into the stats (`umac id=… il=… d1=… d2=… d3=… ch=…`, kept from
the session's first dump — the one that killed it), and names the
station's type in the txsta line. The daemon's side: the crash message
prints the actual values (it said "fwid=/il1=/hcmd=" since v0.2.0
shipped it as a literal), and parses the UMAC half to say it.

v0.2.2/v0.2.3 ride the v11 run's verdict — and the review that came
back from two directions at once. The binding-3 test (mcast station)
died the identical death (fwid=0x71, same interrupt link, same command
word), and the UMAC table the run finally carried said almost nothing
(d1=0x2, d2=0x0, d3=0xdeadbeef — a field the firmware never wrote).
The decisive number was an old one: done=0 with ok=10 — across four
bindings the firmware NEVER serviced the inject queue, so nothing we
injected ever reached the air and the station axis was never the
operative variable.

The daemon grew `--sans-emission` (the control run: the session and
the engine live, not one frame is sent — fw_err climbing anyway blames
the context, not TX), `--liaison N` and `--cellule N` (the two sysctls,
said on the command line). The kext grew, in two passes: binding 4
(mvm's snif_sta — a GENERAL_PURPOSE station, zero address, tid 8, no
HT flags, meant for a LISTENER cell), then the verified second shape:
there is no "STATION_TYPE_BCAST_MGMT = 1" anywhere in iwlwifi — Linux
master still carries this driver's own enum, and commit ced19f26 types
the vif's broadcast station GENERAL_PURPOSE, the one mvm/tx.c hands
ADHOC management frames to. Binding 5 (`--liaison 5`) is binding 3
with only the type changed again (mcast → gp): sta 0, broadcast
address, queue 2, tid 15, on the PIBSS cell. Binding 6 (`--liaison 6`)
is binding 5 plus the one thing the working STA path does that the
session never did — a session protection (the firmware advertises the
capability; `prot=` in the stats says what it did with it). The kext
also has its own, lower-layer control: `sysctl -w
debug.itlawdl_notx=1` makes it hold every injected frame before the
TFD (`held=` counts them) — the same experiment as `--sans-emission`,
but the writes reach the kext. And the stats grow the first dump's
LMAC data1-3 (`lmac ld1=… ld2=… ld3=…`) — the 0x71 NMI is the UMAC
relaying another block's death, and the LMAC half usually names the
real fault; the daemon parses it and the crash message says it.

## Credits

- [owly](https://github.com/t4t5/owly) (MIT): the AWDL engine — sync,
  election, channel sequence, peers — the AirDrop receiver ported here
  whole, and the driver this port plugs into.
- [AirportItlwm](https://github.com/OpenIntelWireless/itlwm) and the
  laobamac fork it builds on: the driver underneath.
