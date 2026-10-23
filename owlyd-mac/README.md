# owlyd-mac — the AWDL engine on macOS

The engine that makes the Mac take part in AirDrop's link layer: owly's
AWDL node (vendored, MIT) run on the radio the itlawdl kext of this
repository provides — the PIBSS cell on channel 6, its RX ring, its
inject queue — with a utun interface for the IPv6 the peers exchange.

Phase 1 (this one) proves the stack: peers that turn valid, their
packets on the utun, ours out. The AirDrop records (mDNS, the picker,
the receive flow) come next.

## Run

    sudo owlyd-mac

It cuts the Wi-Fi (the cell needs the interface down), brings the cell
up, runs the engine, prints peers and the kext's counters every 10 s.
Ctrl-C stops it and puts everything back.

Requirements: the AirportItlwm kext of this repository (v2.2 or later,
branch awdl-v2) loaded, and nothing else holding the radio.

## Layout

    vendor/owly-awdl   owly's sans-IO AWDL crate (MIT); two documented
                       deviations in its driver: Running::stop waits at
                       most 2 s, and the engine loop publishes its
                       iteration count (a liveness heartbeat)
    owlyd-mac          the daemon: sysctl bindings, the Air (a reader
                       thread over the kext's RX ring + a writer thread
                       for the injects), the utun Netif (a reader thread
                       of its own — AsyncFd's kqueue readiness on the
                       kernel-control socket starved the whole engine in
                       the v03 field run)

## Field notes

The v03 run froze the engine after its first MIF: no STATS line ever,
not one peer from a ring the reader kept feeding. Two causes, both
fixed in v0.1.3: the utun's AsyncFd reads (a cached-readiness spin
inside the driver's biased select) and a zombie session from a
brutally killed run (the OFF→CELL sysctl pair collapsed, and the new
session inherited the dead one's ring). The engine's own loop was
replayed for 80 s on Linux with backlog, garbage frames, slow writes
and kernel DAD traffic — it never froze; the freeze was the I/O
threads' to cause, and they no longer can.

## Credits

- [owly](https://github.com/t4t5/owly) (MIT): the AWDL engine — sync,
  election, channel sequence, peers — and the driver this port plugs
  into.
- [AirportItlwm](https://github.com/OpenIntelWireless/itlwm) and the
  laobamac fork it builds on: the driver underneath.
