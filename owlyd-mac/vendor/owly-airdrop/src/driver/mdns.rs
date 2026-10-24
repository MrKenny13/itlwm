//! An mDNS responder and browser for `_airdrop._tcp` on one interface, for
//! hosts without a system responder (MDNS-1, MDNS-3, MDNS-6), announcing in
//! the bursts the radio asks for (MDNS-7).

use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Instant;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;

use crate::proto::mdns::{
    self, Announcer, Browser, Burst, MDNS_GROUP, MDNS_PORT, Message, Service,
};

/// DNS-SD on the interface with this scope ID.
#[derive(Debug, Clone)]
pub struct Mdns {
    scope_id: u32,
    burst: Option<Burst>,
    /// The host's own lane for packets the kernel refuses to carry off
    /// this interface. (The macOS port's third deviation from the vendored
    /// sources: on its utun, not one multicast send off the socket ever
    /// reached the air -- every egress of the responder, announcements,
    /// replies and goodbyes alike, rides this instead. The socket keeps
    /// the receive side; a host that passes `None` sends on it, as
    /// upstream does.)
    outbox: Option<mpsc::Sender<Egress>>,
}

/// One mDNS message on its way out, through the host's lane: the DNS
/// message, and where it goes.
#[derive(Debug)]
pub struct Egress {
    /// The DNS message, as the socket would have carried it.
    pub message: Vec<u8>,
    /// Its destination: the group, or the querier for a unicast reply.
    pub to: SocketAddrV6,
}

/// A registered service, withdrawn when dropped.
pub struct Registration {
    _stop: oneshot::Sender<()>,
}

/// A running browse, which stops when dropped.
pub struct Browse {
    pub events: mpsc::Receiver<mdns::BrowseEvent>,
    _stop: oneshot::Sender<()>,
}

impl Mdns {
    pub fn new(scope_id: u32) -> Self {
        Mdns {
            scope_id,
            burst: None,
            outbox: None,
        }
    }

    /// Repeats the announcement in bursts for as long as the service is
    /// registered, for a radio that doesn't carry the records itself
    /// (MDNS-7). The radio's link says which, if any.
    pub fn with_burst(mut self, burst: Option<Burst>) -> Self {
        self.burst = burst;
        self
    }

    /// Sends every egress through `outbox` instead of the socket (the
    /// macOS port's lane -- see [`Mdns`]).
    pub fn with_outbox(mut self, outbox: mpsc::Sender<Egress>) -> Self {
        self.outbox = Some(outbox);
        self
    }

    /// Publishes a receiver's service until the registration is dropped
    /// (MDNS-1).
    pub async fn register(&self, service: Service) -> io::Result<Registration> {
        let socket = self.socket()?;
        let (stop, stopped) = oneshot::channel::<()>();
        let this = self.clone();
        tokio::spawn(async move { this.respond(socket, service, stopped).await });
        Ok(Registration { _stop: stop })
    }

    /// Browses `_airdrop._tcp` until the browse is dropped (MDNS-6).
    pub async fn browse(&self) -> io::Result<Browse> {
        let socket = self.socket()?;
        let (events, rx) = mpsc::channel(64);
        let (stop, stopped) = oneshot::channel::<()>();
        let this = self.clone();
        tokio::spawn(async move { this.browse_loop(socket, events, stopped).await });
        Ok(Browse {
            events: rx,
            _stop: stop,
        })
    }

    fn socket(&self) -> io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_only_v6(true)?;
        socket.set_reuse_address(true)?;
        #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
        socket.set_reuse_port(true)?;
        socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, MDNS_PORT)).into())?;
        socket.join_multicast_v6(&MDNS_GROUP, self.scope_id)?;
        socket.set_multicast_if_v6(self.scope_id)?;
        socket.set_multicast_hops_v6(255)?;
        socket.set_multicast_loop_v6(false)?;
        socket.set_nonblocking(true)?;
        UdpSocket::from_std(socket.into())
    }

    fn group(&self) -> SocketAddrV6 {
        SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, self.scope_id)
    }

    /// Only packets from this interface count; with several interfaces
    /// joined, others arrive on the same socket.
    fn on_this_link(&self, from: SocketAddr) -> Option<SocketAddrV6> {
        match from {
            SocketAddr::V6(from) if from.scope_id() == self.scope_id || from.scope_id() == 0 => {
                Some(from)
            }
            _ => None,
        }
    }

    async fn respond(
        self,
        socket: UdpSocket,
        service: Service,
        mut stopped: oneshot::Receiver<()>,
    ) {
        let group = self.group();
        let announcement = service.announcement();
        let mut announcer = Announcer::new(self.burst, Instant::now());
        let mut buf = vec![0; mdns::MAX_MESSAGE];
        // macOS port, v0.1.6: this socket's sends were the v06 field run's
        // silent failure — ~1700 announcements "sent", not one ever read
        // off the utun, every error thrown away with `let _`. They are
        // said out loud now (the first, then every 128th): a send that
        // never fails means the kernel carries them, one that always does
        // means only the daemon's direct lane rides.
        let mut send_errs: u64 = 0;
        let mut reply_errs: u64 = 0;
        loop {
            let next = announcer.next_timer();
            let wait = next.map(|at| at.saturating_duration_since(Instant::now()));
            tokio::select! {
                _ = &mut stopped => break,
                () = sleep(wait.unwrap_or_default()), if wait.is_some() => {
                    if announcer.on_timer(Instant::now()) {
                        self.egress(&socket, &announcement, group, &mut send_errs).await;
                    }
                }
                received = socket.recv_from(&mut buf) => {
                    let Ok((n, from)) = received else { continue };
                    let Some(from) = self.on_this_link(from) else { continue };
                    let Ok(query) = Message::decode(&buf[..n]) else { continue };
                    let Some(reply) = service.answer(&query, from.port() != MDNS_PORT) else { continue };
                    let to = if reply.unicast { from } else { group };
                    // RFC 6762 §6: a short random delay for shared records
                    // isn't needed with one responder per link.
                    self.egress(&socket, &reply.message, to, &mut reply_errs).await;
                }
            }
        }
        // Withdraw the records (RFC 6762 §10.1).
        self.egress(&socket, &service.goodbye(), group, &mut send_errs).await;
    }

    /// Sends one message: through the outbox when the host gave one (its
    /// utun never carried a multicast send off the socket), else through
    /// the socket, as upstream does. (The macOS port's third deviation.)
    async fn egress(
        &self,
        socket: &UdpSocket,
        message: &[u8],
        to: SocketAddrV6,
        errs: &mut u64,
    ) {
        if let Some(outbox) = &self.outbox {
            // The lane dies only with the engine under it; nothing else
            // is left to try, and the session is ending anyway.
            if outbox
                .send(Egress { message: message.to_vec(), to })
                .await
                .is_err()
            {
                *errs += 1;
            }
            return;
        }
        // macOS port, v0.1.6: this socket's sends were the v06 field run's
        // silent failure — ~1700 announcements "sent", not one ever read
        // off the utun, every error thrown away with `let _`. They are
        // said out loud now (the first, then every 128th): a send that
        // never fails means the kernel carries them, one that always does
        // means only the daemon's direct lane rides.
        if let Err(e) = socket.send_to(message, to).await {
            *errs += 1;
            if *errs == 1 || *errs % 128 == 0 {
                tracing::warn!(
                    "mDNS socket : un envoi vers le groupe a rendu {e} ({} échecs)",
                    *errs
                );
            }
        }
    }

    async fn browse_loop(
        self,
        socket: UdpSocket,
        events: mpsc::Sender<mdns::BrowseEvent>,
        mut stopped: oneshot::Receiver<()>,
    ) {
        let group = self.group();
        let mut browser = Browser::new(Instant::now());
        let mut buf = vec![0; mdns::MAX_MESSAGE];
        loop {
            let deadline = browser.next_timer();
            let wait = deadline.saturating_duration_since(Instant::now());
            tokio::select! {
                _ = &mut stopped => return,
                () = events.closed() => return,
                () = sleep(wait) => {
                    if let Some(query) = browser.on_timer(Instant::now()) {
                        let _ = socket.send_to(&query, group).await;
                    }
                }
                received = socket.recv_from(&mut buf) => {
                    let Ok((n, from)) = received else { continue };
                    if self.on_this_link(from).is_none() {
                        continue;
                    }
                    if let Ok(message) = Message::decode(&buf[..n]) {
                        browser.handle(&message, Instant::now());
                    }
                }
            }
            while let Some(event) = browser.poll_event() {
                if events.send(event).await.is_err() {
                    return;
                }
            }
        }
    }
}
