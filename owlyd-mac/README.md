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

    vendor/owly-awdl   owly's sans-IO AWDL crate (MIT); one documented
                       deviation: Running::stop waits at most 2 s
    owlyd-mac          the daemon: sysctl bindings, the Air (a reader
                       thread over the kext's RX ring + inline inject),
                       the utun Netif

## Credits

- [owly](https://github.com/t4t5/owly) (MIT): the AWDL engine — sync,
  election, channel sequence, peers — and the driver this port plugs
  into.
- [AirportItlwm](https://github.com/OpenIntelWireless/itlwm) and the
  laobamac fork it builds on: the driver underneath.
