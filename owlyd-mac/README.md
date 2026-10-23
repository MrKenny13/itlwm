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

Requirements: the AirportItlwm kext of this repository (v2.5 or later
for `done=`/`recycled=` in the stats; v2.4 still runs everything else,
branch awdl-v2) loaded, and nothing else holding the radio. The
self-signed identity persists in `/var/db/owlyd-mac`, so the phone keeps
recognising the Mac between sessions.

## Testing against an iPhone

1. iPhone: unlocked, AirDrop receiving set to *Everyone* (Contacts Only
   filters unknown receivers out before they can be listed), the share
   sheet or the AirDrop picker open — that is what makes the phone's
   AWDL radio active at all.
2. Stay next to the Mac; the phone visits channel 6 a slot or two in
   sixteen, so give the announcement bursts ~15 s.
3. What the log tells you, in order: `porte TX` climbing the ramp with
   rx moving at every step (the radio surviving our own emission),
   `tx_multicast` growing (the engine carrying the announcements as AWDL
   data frames), `— AirDrop : vu par [..]` (an iPhone's `/Discover`
   answered — the name should be in its list), then the ask prompt when
   someone taps. If `porte TX` says the radio died, the line names the
   pace that killed it — that log is the next fix's brief.

## Layout

    vendor/owly-awdl      owly's sans-IO AWDL crate (MIT); two documented
                          deviations in its driver: Running::stop waits at
                          most 2 s, and the engine loop publishes its
                          iteration count (a liveness heartbeat)
    vendor/owly-airdrop   owly's AirDrop crate (MIT): the mDNS codec and
                          responder, the TLS receiver (/Discover, /Ask,
                          /Upload), the sandboxed extraction
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

## Credits

- [owly](https://github.com/t4t5/owly) (MIT): the AWDL engine — sync,
  election, channel sequence, peers — the AirDrop receiver ported here
  whole, and the driver this port plugs into.
- [AirportItlwm](https://github.com/OpenIntelWireless/itlwm) and the
  laobamac fork it builds on: the driver underneath.
