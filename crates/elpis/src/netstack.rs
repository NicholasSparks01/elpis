//! Networking for a standalone elpis VM: a host TCP/IP stack on the shim's NIC socket.
//!
//! The NIC contract (as both sbx's shim and elpis-shim provide it): a `mode=unixgram` NIC is an AF_UNIX **stream**
//! socket (we listen, the shim dials) carrying `[u32 big-endian length][raw Ethernet frame]`, no
//! virtio-net header. The guest is 172.17.0.1/31, gateway 172.17.0.0; we are the gateway.
//!
//! We run [`smoltcp`] as the gateway. It answers ARP and ICMP itself. For TCP we NAT: each guest
//! connection to any destination is accepted (smoltcp `any_ip`) by a listening socket bound to that
//! destination, and bridged to a real Windows TCP socket, subject to policy and counted per host.
//!
//! When the policy's default is `ask`, a connection no rule covers is held: we drop the guest's SYN without
//! answering, the guest retransmits it (after 1, 3, 7… s), and once someone allows or denies the host the
//! next retransmission is handled normally. Waiting hosts are listed in [`NetStats::waiting`] and in
//! `<work>\state\approvals.json`, for the TUI and CLI.
//!
//! Published ports go the other way: a Windows listener accepts, and the stack dials the guest from the
//! gateway address with a smoltcp socket, then bridges the two the same way (`sbx run -p`).

use crate::log::{Color, Log};
use crate::policy::{Action, Decision, Policy};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpProtocol, Ipv4Address,
    Ipv4Packet, TcpPacket, UdpPacket,
};
use socket2::{Domain, SockAddr, Socket, Type};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GUEST_IP: &str = "172.17.0.1/31";
const GATEWAY_IP: &str = "172.17.0.0";
const GATEWAY_V4: Ipv4Address = Ipv4Address::new(172, 17, 0, 0);
const GUEST_V4: Ipv4Address = Ipv4Address::new(172, 17, 0, 1);
const MTU: usize = 1500;
const FLOW_BUF: usize = 256 * 1024;
/// An unanswered approval request is dropped after this; the guest's connection has usually given up by then
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-destination traffic, shared with the UI. Key is "name:port" when DNS told us the name, else
/// "ip:port"; UDP keys end in "/udp".
#[derive(Default)]
pub struct NetStats {
    pub hosts: Mutex<BTreeMap<String, HostStat>>,
    /// IPv4 address -> the name the guest looked up, learned from DNS answers passing through us
    pub names: Mutex<HashMap<Ipv4Addr, String>>,
    /// Hosts waiting for an allow/deny answer, by the host a rule would name (no port)
    approvals: Mutex<BTreeMap<String, Approval>>,
    /// Where the waiting list is mirrored for other processes (`<work>\state\approvals.json`)
    approvals_path: Option<PathBuf>,
}

/// A connection held until someone answers for its host.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Approval {
    /// What a rule for it would say: the name the guest looked up, or the IP
    pub host: String,
    pub ports: std::collections::BTreeSet<u16>,
    pub first_seen: String,
    /// SYNs held so far (the guest retries)
    pub attempts: u64,
    #[serde(skip)]
    since: Option<Instant>,
    /// Answered "not now": refuse the connection that's waiting, then forget it
    #[serde(skip)]
    dismissed: bool,
}

#[derive(Default, Clone)]
pub struct HostStat {
    /// Connections (TCP) or new peers (UDP) that were allowed
    pub conns: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    /// Attempts the policy refused
    pub blocked: u64,
    pub last_seen: String,
    pub last_action: Option<Action>,
    /// The rule pattern behind the last decision; None means the default applied
    pub rule: Option<String>,
}

impl NetStats {
    fn on_open(&self, host: &str, decision: &Decision) {
        let mut h = self.hosts.lock().unwrap();
        let e = h.entry(host.to_owned()).or_default();
        match decision.action {
            Action::Allow => e.conns += 1,
            Action::Deny => e.blocked += 1,
            Action::Ask => {}
        }
        e.last_seen = crate::win::local_time();
        e.last_action = Some(decision.action);
        e.rule = decision.rule.clone();
    }

    /// Hosts waiting for an answer, oldest first.
    pub fn waiting(&self) -> Vec<Approval> {
        let mut list: Vec<Approval> = self.approvals.lock().unwrap().values().filter(|a| !a.dismissed).cloned().collect();
        list.sort_by_key(|a| a.since);
        list
    }

    /// "Not now": refuse the waiting connection for `host` (it can ask again later).
    pub fn dismiss(&self, host: &str) {
        if let Some(a) = self.approvals.lock().unwrap().get_mut(host) {
            a.dismissed = true;
        }
        self.save_approvals();
    }

    /// Hold a connection for `host`; true the first time it asks.
    fn ask(&self, host: &str, port: u16) -> bool {
        let new = {
            let mut map = self.approvals.lock().unwrap();
            map.retain(|_, a| a.since.is_some_and(|t| t.elapsed() < APPROVAL_TIMEOUT));
            let entry = map.entry(host.to_owned()).or_insert_with(|| Approval {
                host: host.to_owned(),
                ports: Default::default(),
                first_seen: crate::win::local_time(),
                attempts: 0,
                since: Some(Instant::now()),
                dismissed: false,
            });
            entry.attempts += 1;
            entry.ports.insert(port) && entry.attempts == 1
        };
        self.save_approvals();
        new
    }

    /// Whether `host` was answered "not now" (and forget it, so it can ask again).
    fn take_dismissed(&self, host: &str) -> bool {
        let mut map = self.approvals.lock().unwrap();
        let dismissed = map.get(host).is_some_and(|a| a.dismissed);
        if dismissed {
            map.remove(host);
        }
        dismissed
    }

    /// The host was decided by a rule or grant: it no longer waits.
    fn resolved(&self, host: &str) {
        if self.approvals.lock().unwrap().remove(host).is_some() {
            self.save_approvals();
        }
    }

    fn save_approvals(&self) {
        if let Some(path) = &self.approvals_path {
            let _ = std::fs::write(path, serde_json::to_string_pretty(&self.waiting()).unwrap_or_default());
        }
    }

    /// Hits for a host (allowed + blocked), for "how often did this host come up".
    pub fn hits(stat: &HostStat) -> u64 {
        stat.conns + stat.blocked
    }

    fn name_of(&self, dst: &SocketAddr) -> Option<String> {
        match dst.ip() {
            IpAddr::V4(ip) => self.names.lock().unwrap().get(&ip).cloned(),
            IpAddr::V6(_) => None,
        }
    }
    fn on_bytes(&self, host: &str, up: u64, down: u64) {
        let mut h = self.hosts.lock().unwrap();
        let e = h.entry(host.to_owned()).or_default();
        e.bytes_up += up;
        e.bytes_down += down;
    }

    /// "name:port" if the guest resolved this address through us, else "ip:port".
    fn label(&self, dst: &SocketAddr) -> String {
        if let IpAddr::V4(ip) = dst.ip()
            && let Some(name) = self.names.lock().unwrap().get(&ip)
        {
            return format!("{name}:{}", dst.port());
        }
        dst.to_string()
    }
}

pub struct NetProbe {
    socket_path: PathBuf,
    stopped: Arc<AtomicBool>,
    stats: Arc<NetStats>,
    policy: Arc<Policy>,
    /// Connections accepted on published ports, handed to the stack thread
    inbound: Sender<Inbound>,
    ports: Mutex<Vec<PublishedPort>>,
    log: Log,
}

/// A connection from Windows to a published port, waiting to be dialed into the guest.
struct Inbound {
    stream: TcpStream,
    guest_port: u16,
    label: String,
}

/// A port published from the box to Windows: `host` (e.g. 127.0.0.1:8080) forwards to the box's `guest_port`.
pub struct PublishedPort {
    pub host: SocketAddr,
    pub guest_port: u16,
    stop: Arc<AtomicBool>,
}

impl Drop for PublishedPort {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the listener thread sees the flag and lets go of the port
        let wake = if self.host.ip().is_unspecified() { SocketAddr::from(([127, 0, 0, 1], self.host.port())) } else { self.host };
        let _ = TcpStream::connect_timeout(&wake, Duration::from_millis(200));
    }
}

impl NetProbe {
    /// Forward Windows `host` (`127.0.0.1:8080`; `0.0.0.0:8080` for the whole LAN) to the box's `guest_port`.
    pub fn publish(&self, host: SocketAddr, guest_port: u16) -> Result<()> {
        let mut ports = self.ports.lock().unwrap();
        if ports.iter().any(|p| p.host.port() == host.port()) {
            anyhow::bail!("port {} is already published", host.port());
        }
        let listener = TcpListener::bind(host).with_context(|| format!("listening on {host} (is something else using it?)"))?;
        let host = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, stop2, log) = (self.inbound.clone(), stop.clone(), self.log.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = conn else { continue };
                let label = format!("published {}→{guest_port}", host.port());
                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                log.line("net", Color::Cyan, &format!("TCP in {peer} → {host} → box :{guest_port}"));
                if tx.send(Inbound { stream, guest_port, label }).is_err() {
                    break;
                }
            }
        });
        self.log.line("net", Color::Cyan, &format!("published {host} → box :{guest_port}"));
        ports.push(PublishedPort { host, guest_port, stop });
        Ok(())
    }

    /// Stop forwarding Windows port `host_port`. Connections already open stay up.
    pub fn unpublish(&self, host_port: u16) -> bool {
        let mut ports = self.ports.lock().unwrap();
        let before = ports.len();
        ports.retain(|p| p.host.port() != host_port);
        let removed = ports.len() != before;
        if removed {
            self.log.line("net", Color::Cyan, &format!("unpublished port {host_port}"));
        }
        removed
    }

    /// (Windows address, box port) for each published port.
    pub fn published(&self) -> Vec<(SocketAddr, u16)> {
        self.ports.lock().unwrap().iter().map(|p| (p.host, p.guest_port)).collect()
    }

    /// Live per-destination traffic, for a dashboard.
    pub fn stats(&self) -> Arc<NetStats> {
        self.stats.clone()
    }

    /// The VM's egress policy; changes apply to new connections immediately.
    pub fn policy(&self) -> Arc<Policy> {
        self.policy.clone()
    }
}

/// `proxy`: the credential proxy for this box, which takes over HTTPS flows to its injection domains.
pub fn probe(config_path: &Path, work: &Path, log: &Log, proxy: Option<Arc<crate::credproxy::CredProxy>>) -> Result<NetProbe> {
    let socket_path = work.join("net.sock");
    let id = net_id();
    // Three distinct MACs: the VM's virtio NIC (network.mac == ctr.network.vmmac, the "host link" the
    // shim looks up), the container's interface on the VM's internal bridge (ctr.network.mac), and our
    // gateway. Sharing one MAC between NIC and container makes the bridge swallow replies.
    let nic_mac = random_mac(0x02);
    let ctr_mac = random_mac(0x06);
    let gw_mac = random_mac(0x0a);
    inject_annotations(config_path, &socket_path, &id, &nic_mac, &ctr_mac)?;

    let _ = std::fs::remove_file(&socket_path);
    let listener = Socket::new(Domain::UNIX, Type::STREAM, None).context("creating an AF_UNIX stream socket")?;
    listener.bind(&SockAddr::unix(&socket_path)?).with_context(|| format!("binding {}", socket_path.display()))?;
    listener.listen(4)?;
    log.line("net", Color::Blue, &format!("gateway {GATEWAY_IP} up on {} (guest {GUEST_IP})", socket_path.display()));

    let stopped = Arc::new(AtomicBool::new(false));
    let approvals_path = work.join("state").join("approvals.json");
    let _ = std::fs::remove_file(&approvals_path);
    let stats = Arc::new(NetStats { approvals_path: Some(approvals_path), ..Default::default() });
    let policy = Policy::load(work);
    let snapshot = policy.snapshot();
    log.line("net", Color::Blue, &format!("policy: default {}, {} rule(s) from {}", snapshot.default, snapshot.rules.len(), Policy::path_for(work).display()));
    let (stop, stats2, gw, log2, policy2) = (stopped.clone(), stats.clone(), gw_mac_bytes(&gw_mac), log.clone(), policy.clone());
    let (inbound_tx, inbound_rx) = channel::<Inbound>();
    std::thread::spawn(move || {
        let conn = match listener.accept() {
            Ok((conn, _)) => conn,
            Err(e) => {
                if !stop.load(Ordering::SeqCst) {
                    log2.line("net", Color::Red, &format!("accept failed: {e}"));
                }
                return;
            }
        };
        log2.line("net", Color::Green, "shim connected; gateway online");
        if let Err(e) = run_stack(conn, gw, &stop, &stats2, &policy2, &inbound_rx, &log2, proxy.as_ref()) {
            log2.line("net", Color::Red, &format!("stack stopped: {e:#}"));
        }
    });

    Ok(NetProbe { socket_path, stopped, stats, policy, inbound: inbound_tx, ports: Mutex::new(Vec::new()), log: log.clone() })
}

impl Drop for NetProbe {
    fn drop(&mut self) {
        self.ports.lock().unwrap().clear();
        self.stopped.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket_path);
        if let (Ok(s), Ok(a)) = (Socket::new(Domain::UNIX, Type::STREAM, None), SockAddr::unix(&self.socket_path)) {
            let _ = s.connect(&a);
        }
    }
}

/// One active TCP connection: a smoltcp socket (guest side) bridged to a worker owning the real socket.
struct Flow {
    handle: SocketHandle,
    host: String,
    to_real: Sender<Vec<u8>>,
    from_real: Receiver<Vec<u8>>,
    done: Arc<AtomicBool>,
    pending: Vec<u8>, // bytes pulled from the worker that didn't fit in one smoltcp send
    half_closed: bool,
    /// Denied by policy: the listener exists only to answer the SYN with a reset
    blocked: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_stack(
    conn: Socket,
    gw_mac: [u8; 6],
    stop: &AtomicBool,
    stats: &Arc<NetStats>,
    policy: &Arc<Policy>,
    inbound: &Receiver<Inbound>,
    log: &Log,
    proxy: Option<&Arc<crate::credproxy::CredProxy>>,
) -> Result<()> {
    let reader = conn.try_clone().context("cloning the NIC socket")?;
    let (rx_tx, rx_rx) = channel::<Vec<u8>>();
    let (tx_tx, tx_rx) = channel::<Vec<u8>>();
    let rstop = Arc::new(AtomicBool::new(false));
    let (rstop2, rlog) = (rstop.clone(), log.clone());
    let reader_handle = std::thread::spawn(move || deframe(reader, &rx_tx, &rstop2, &rlog));

    let mut device = FramedDevice { inbox: VecDeque::new(), tx: tx_tx };
    let mut iface = {
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(gw_mac)));
        config.random_seed = seed() as u64;
        let mut iface = Interface::new(config, &mut device, now());
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::Ipv4(GATEWAY_V4), 31));
        });
        // any_ip only accepts a packet for a foreign address when a route to it goes via one of our own
        // addresses, so route everything via ourselves: that is what makes us a NAT gateway.
        iface.set_any_ip(true);
        let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY_V4);
        iface
    };
    let mut sockets = SocketSet::new(Vec::new());
    let mut flows: HashMap<FlowKey, Flow> = HashMap::new();
    let mut relays: HashMap<UdpKey, UdpRelay> = HashMap::new();
    let dns_upstream = host_dns();
    log.line("net", Color::Blue, &format!("DNS at {GATEWAY_IP}:53 forwards to {dns_upstream}"));
    let mut conn_w = conn;
    let mut next_port: u16 = 49152;

    while !stop.load(Ordering::SeqCst) {
        // Connections to published ports: dial the guest from the gateway
        while let Ok(conn) = inbound.try_recv() {
            next_port = if next_port >= 65000 { 49152 } else { next_port + 1 };
            dial_guest(conn, next_port, &mut iface, &mut sockets, &mut flows, stats, log);
        }

        // Ingest frames; a new TCP SYN or UDP destination opens a NAT flow before smoltcp sees the frame.
        while let Ok(frame) = rx_rx.try_recv() {
            debug_frame(log, "<-rx", &frame);
            if let Some((key, dst)) = new_tcp_syn(&frame, &flows) {
                if open_flow(key, dst, &mut sockets, &mut flows, stats, policy, log, proxy) == Opened::Held {
                    continue; // unanswered: drop the SYN, the guest will retry it
                }
            } else if let Some((key, upstream)) = new_udp(&frame, &relays, dns_upstream) {
                open_udp(key, upstream, &mut sockets, &mut relays, stats, policy, log);
            }
            device.inbox.push_back(frame);
        }

        iface.poll(now(), &mut device, &mut sockets);
        service_flows(&mut sockets, &mut flows, stats);
        service_udp(&mut sockets, &mut relays, stats);

        while let Ok(mut frame) = tx_rx.try_recv() {
            debug_frame(log, "tx->", &frame);
            // Pad to Ethernet's 60-byte minimum (without FCS); short frames may be dropped on the NIC path
            if frame.len() < 60 {
                frame.resize(60, 0);
            }
            let len = (frame.len() as u32).to_be_bytes();
            if conn_w.write_all(&len).and_then(|_| conn_w.write_all(&frame)).is_err() {
                stop.store(true, Ordering::SeqCst);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    rstop.store(true, Ordering::SeqCst);
    let _ = conn_w.shutdown(Shutdown::Both);
    let _ = reader_handle.join();
    log.line("net", Color::Blue, "gateway offline");
    Ok(())
}

type FlowKey = (Ipv4Address, u16, Ipv4Address, u16); // src, sport, dst, dport

/// If the frame is a fresh TCP SYN (no ACK) for a flow we don't know, return its key and destination.
fn new_tcp_syn(frame: &[u8], flows: &HashMap<FlowKey, Flow>) -> Option<(FlowKey, SocketAddr)> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Tcp {
        return None;
    }
    let (src, dst) = (ip.src_addr(), ip.dst_addr());
    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
    if !tcp.syn() || tcp.ack() {
        return None;
    }
    let key = (src, tcp.src_port(), dst, tcp.dst_port());
    if flows.contains_key(&key) {
        return None;
    }
    let dst_addr = SocketAddr::from((std::net::Ipv4Addr::from(dst.octets()), tcp.dst_port()));
    Some((key, dst_addr))
}

#[derive(PartialEq)]
enum Opened {
    Flow,
    /// Waiting for approval: no listener, and the SYN is dropped
    Held,
}

/// Create the smoltcp listener for a new flow and, if policy allows, spawn the worker that owns the real
/// socket. A denied flow still gets a listener, but only so it can answer the SYN with a reset: the guest
/// sees "connection refused" at once instead of waiting for a timeout. An unanswered `ask` holds it.
#[allow(clippy::too_many_arguments)]
fn open_flow(
    key: FlowKey,
    dst: SocketAddr,
    sockets: &mut SocketSet,
    flows: &mut HashMap<FlowKey, Flow>,
    stats: &Arc<NetStats>,
    policy: &Policy,
    log: &Log,
    proxy: Option<&Arc<crate::credproxy::CredProxy>>,
) -> Opened {
    let host = stats.label(&dst);
    let rule_host = stats.name_of(&dst).unwrap_or_else(|| dst.ip().to_string());
    let mut decision = policy.decide(stats.name_of(&dst).as_deref(), dst.ip(), dst.port());
    if decision.action == Action::Ask {
        if !stats.take_dismissed(&rule_host) {
            if stats.ask(&rule_host, dst.port()) {
                stats.on_open(&host, &decision);
                log.line("net", Color::Magenta, &format!("TCP {host} waits for approval (no rule covers {rule_host})"));
            }
            return Opened::Held;
        }
        decision = Decision { action: Action::Deny, rule: Some("not now".into()) };
    } else {
        stats.resolved(&rule_host);
    }
    stats.on_open(&host, &decision);
    let blocked = decision.action == Action::Deny;
    let why = decision.rule.as_deref().map_or_else(|| "default".to_owned(), |r| format!("rule {r}"));
    if blocked {
        log.line("net", Color::Yellow, &format!("TCP blocked {host} ({why})"));
    } else {
        log.line("net", Color::Green, &format!("TCP open {host} ({why})"));
    }

    let buf = if blocked { 64 } else { FLOW_BUF };
    let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(vec![0u8; buf]), tcp::SocketBuffer::new(vec![0u8; buf]));
    // any_ip lets us listen on the destination address even though it isn't ours
    if socket.listen((IpAddress::Ipv4(key.2), key.3)).is_err() {
        return Opened::Flow;
    }
    let handle = sockets.add(socket);

    // The gateway address stands for the Windows host (host.docker.internal), like Docker's convention
    let real = if key.2 == GATEWAY_V4 { SocketAddr::from(([127, 0, 0, 1], dst.port())) } else { dst };
    let (to_real_tx, to_real_rx) = channel::<Vec<u8>>();
    let (from_real_tx, from_real_rx) = channel::<Vec<u8>>();
    let done = Arc::new(AtomicBool::new(blocked));
    if !blocked {
        let (wdone, wlog, whost) = (done.clone(), log.clone(), host.clone());
        // HTTPS to a host the box's kit injects a credential for: the proxy takes the flow
        let intercept = proxy
            .filter(|_| dst.port() == 443)
            .and_then(|p| stats.name_of(&dst).filter(|name| p.rule_for(name).is_some()).map(|name| (p.clone(), name)));
        match intercept {
            Some((p, name)) => {
                log.line("proxy", Color::Cyan, &format!("TCP {host} through the credential proxy"));
                std::thread::spawn(move || p.intercept(real, name, to_real_rx, from_real_tx, wdone, wlog));
            }
            None => {
                std::thread::spawn(move || worker(real, to_real_rx, from_real_tx, wdone, whost, wlog));
            }
        }
    }
    flows.insert(
        key,
        Flow { handle, host, to_real: to_real_tx, from_real: from_real_rx, done, pending: Vec::new(), half_closed: false, blocked },
    );
    Opened::Flow
}

/// Open a connection from the gateway to the guest for a published port, bridged to the accepted stream.
fn dial_guest(
    conn: Inbound,
    local_port: u16,
    iface: &mut Interface,
    sockets: &mut SocketSet,
    flows: &mut HashMap<FlowKey, Flow>,
    stats: &Arc<NetStats>,
    log: &Log,
) {
    let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(vec![0u8; FLOW_BUF]), tcp::SocketBuffer::new(vec![0u8; FLOW_BUF]));
    let remote = (IpAddress::Ipv4(GUEST_V4), conn.guest_port);
    if let Err(e) = socket.connect(iface.context(), remote, (IpAddress::Ipv4(GATEWAY_V4), local_port)) {
        log.line("net", Color::Yellow, &format!("{}: can't dial the box: {e}", conn.label));
        return;
    }
    let handle = sockets.add(socket);
    stats.on_open(&conn.label, &Decision { action: Action::Allow, rule: Some("published port".into()) });
    let (to_real_tx, to_real_rx) = channel::<Vec<u8>>();
    let (from_real_tx, from_real_rx) = channel::<Vec<u8>>();
    let done = Arc::new(AtomicBool::new(false));
    let (wdone, wlog, whost) = (done.clone(), log.clone(), conn.label.clone());
    std::thread::spawn(move || bridge(conn.stream, to_real_rx, from_real_tx, wdone, whost, wlog));
    flows.insert(
        (GATEWAY_V4, local_port, GUEST_V4, conn.guest_port),
        Flow { handle, host: conn.label, to_real: to_real_tx, from_real: from_real_rx, done, pending: Vec::new(), half_closed: false, blocked: false },
    );
}

/// Move data between each flow's smoltcp socket and its worker, and reap finished flows.
fn service_flows(sockets: &mut SocketSet, flows: &mut HashMap<FlowKey, Flow>, stats: &Arc<NetStats>) {
    let mut remove = Vec::new();
    for (key, flow) in flows.iter_mut() {
        let socket = sockets.get_mut::<tcp::Socket>(flow.handle);

        if flow.blocked {
            // The listener has taken the SYN; reset the connection and let it be reaped
            if !matches!(socket.state(), tcp::State::Listen) {
                socket.abort();
            }
            if matches!(socket.state(), tcp::State::Closed) {
                remove.push(*key);
            }
            continue;
        }

        // guest -> real
        if socket.can_recv() {
            let mut up = 0u64;
            let _ = socket.recv(|buf| {
                if !buf.is_empty() {
                    let _ = flow.to_real.send(buf.to_vec());
                    up = buf.len() as u64;
                }
                (buf.len(), ())
            });
            if up > 0 {
                stats.on_bytes(&flow.host, up, 0);
            }
        }

        // real -> guest (flush any leftover first, then pull more)
        if !flow.pending.is_empty() && socket.can_send()
            && let Ok(sent) = socket.send_slice(&flow.pending) {
                flow.pending.drain(..sent);
            }
        while flow.pending.is_empty() && socket.can_send() {
            match flow.from_real.try_recv() {
                Ok(data) => {
                    let sent = socket.send_slice(&data).unwrap_or(0);
                    stats.on_bytes(&flow.host, 0, data.len() as u64);
                    if sent < data.len() {
                        flow.pending.extend_from_slice(&data[sent..]);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    flow.half_closed = true;
                    break;
                }
            }
        }

        // close our send side once the real peer is done and everything is flushed
        if flow.done.load(Ordering::SeqCst) && flow.pending.is_empty() && flow.half_closed && socket.may_send() {
            socket.close();
        }
        if matches!(socket.state(), tcp::State::Closed) {
            remove.push(*key);
        }
    }
    for key in remove {
        if let Some(flow) = flows.remove(&key) {
            sockets.remove(flow.handle); // dropping the channels ends the worker
        }
    }
}

/// Owns the real Windows TCP socket; bridges it to the flow's channels. One thread, blocking with a
/// short read timeout so it notices both guest data and the guest going away.
fn worker(dst: SocketAddr, from_guest: Receiver<Vec<u8>>, to_guest: Sender<Vec<u8>>, done: Arc<AtomicBool>, host: String, log: Log) {
    let stream = match TcpStream::connect_timeout(&dst, Duration::from_secs(10)) {
        Ok(s) => s,
        Err(e) => {
            log.line("net", Color::Yellow, &format!("TCP {host}: connect failed: {e}"));
            done.store(true, Ordering::SeqCst);
            return;
        }
    };
    bridge(stream, from_guest, to_guest, done, host, log);
}

/// Pump bytes between a real Windows TCP stream and a flow's channels until either side closes.
fn bridge(stream: TcpStream, from_guest: Receiver<Vec<u8>>, to_guest: Sender<Vec<u8>>, done: Arc<AtomicBool>, _host: String, _log: Log) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut stream = stream;
    let mut buf = [0u8; 32 * 1024];
    let mut guest_gone = false;
    loop {
        // guest -> real
        loop {
            match from_guest.try_recv() {
                Ok(data) => {
                    if stream.write_all(&data).is_err() {
                        done.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    guest_gone = true;
                    let _ = stream.shutdown(Shutdown::Write);
                    break;
                }
            }
        }
        // real -> guest
        match stream.read(&mut buf) {
            Ok(0) => break, // real peer closed
            Ok(n) => {
                if to_guest.send(buf[..n].to_vec()).is_err() {
                    break; // flow was reaped
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                if guest_gone {
                    // nothing more to send and we've half-closed; keep draining until the peer FINs
                }
            }
            Err(_) => break,
        }
    }
    done.store(true, Ordering::SeqCst);
}

// ---- UDP NAT and DNS ----

type UdpKey = (Ipv4Address, u16); // dst, dport as the guest addressed it

/// One guest-facing UDP endpoint (a smoltcp socket bound to dst:dport via any_ip), with a real Windows
/// socket per guest peer. DNS to the gateway is the same thing with the host's resolver as upstream.
struct UdpRelay {
    handle: SocketHandle,
    upstream: SocketAddr,
    host: String,
    dns: bool,
    peers: HashMap<IpEndpoint, Arc<UdpSocket>>,
    replies_tx: Sender<(IpEndpoint, Vec<u8>)>,
    replies_rx: Receiver<(IpEndpoint, Vec<u8>)>,
}

/// A UDP datagram for a destination we don't relay yet: return its key and the real upstream.
fn new_udp(frame: &[u8], relays: &HashMap<UdpKey, UdpRelay>, dns_upstream: SocketAddr) -> Option<(UdpKey, SocketAddr)> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp {
        return None;
    }
    let dst = ip.dst_addr();
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    let key = (dst, udp.dst_port());
    if relays.contains_key(&key) || dst.is_multicast() || dst.is_broadcast() {
        return None;
    }
    if dst == GATEWAY_V4 {
        // Only DNS is served at the gateway itself
        return (udp.dst_port() == 53).then_some((key, dns_upstream));
    }
    Some((key, SocketAddr::from((Ipv4Addr::from(dst.octets()), udp.dst_port()))))
}

#[allow(clippy::too_many_arguments)]
fn open_udp(
    key: UdpKey,
    upstream: SocketAddr,
    sockets: &mut SocketSet,
    relays: &mut HashMap<UdpKey, UdpRelay>,
    stats: &Arc<NetStats>,
    policy: &Policy,
    log: &Log,
) {
    // DNS at the gateway is always served: blocking names is done on the connections that follow
    let dns = key.0 == GATEWAY_V4 && key.1 == 53;
    let host = if dns { format!("dns ({upstream})") } else { format!("{}/udp", stats.label(&upstream)) };
    if !dns {
        let decision = policy.decide(stats.name_of(&upstream).as_deref(), upstream.ip(), upstream.port());
        stats.on_open(&host, &decision);
        // UDP has no handshake to hold, so an unanswered "ask" is a "no" for now
        if decision.action != Action::Allow {
            log.line("net", Color::Yellow, &format!("UDP blocked {host}"));
            return;
        }
    }
    let mut socket = udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0u8; 64 * 1024]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0u8; 64 * 1024]),
    );
    if socket.bind((IpAddress::Ipv4(key.0), key.1)).is_err() {
        return;
    }
    let handle = sockets.add(socket);
    if !dns {
        log.line("net", Color::Green, &format!("UDP open {host}"));
    }
    let (replies_tx, replies_rx) = channel();
    relays.insert(key, UdpRelay { handle, upstream, host, dns, peers: HashMap::new(), replies_tx, replies_rx });
}

fn service_udp(sockets: &mut SocketSet, relays: &mut HashMap<UdpKey, UdpRelay>, stats: &Arc<NetStats>) {
    for relay in relays.values_mut() {
        let socket = sockets.get_mut::<udp::Socket>(relay.handle);
        let mut local_answers = Vec::new();
        // guest -> real
        while let Ok((data, meta)) = socket.recv() {
            let guest = meta.endpoint;
            if relay.dns
                && let Some(answer) = local_dns(data, stats)
            {
                local_answers.push((guest, answer));
                continue;
            }
            if !relay.peers.contains_key(&guest)
                && let Some(peer) = spawn_udp_peer(relay.upstream, guest, relay.replies_tx.clone())
            {
                relay.peers.insert(guest, peer);
            }
            if let Some(peer) = relay.peers.get(&guest)
                && peer.send(data).is_ok()
                && !relay.dns
            {
                stats.on_bytes(&relay.host, data.len() as u64, 0);
            }
        }
        for (guest, answer) in local_answers {
            let _ = socket.send_slice(&answer, guest);
        }
        // real -> guest
        while socket.can_send() {
            let Ok((guest, data)) = relay.replies_rx.try_recv() else { break };
            if relay.dns {
                learn_names(&data, stats);
            } else {
                stats.on_bytes(&relay.host, 0, data.len() as u64);
            }
            let _ = socket.send_slice(&data, guest);
        }
    }
}

/// A real Windows UDP socket for one guest peer; its replies come back tagged with that peer.
fn spawn_udp_peer(upstream: SocketAddr, guest: IpEndpoint, replies: Sender<(IpEndpoint, Vec<u8>)>) -> Option<Arc<UdpSocket>> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.connect(upstream).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    let sock = Arc::new(sock);
    let reader = sock.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        let mut idle = 0;
        while idle < 60 {
            match reader.recv(&mut buf) {
                Ok(n) => {
                    idle = 0;
                    if replies.send((guest, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(_) => idle += 1, // timeout tick; give up after a minute of silence
            }
        }
    });
    Some(sock)
}

/// Answers `host.docker.internal` / `gateway.docker.internal` ourselves with the gateway address (which
/// the NAT maps to the Windows host's loopback). Anything else returns None and is forwarded upstream.
fn local_dns(query: &[u8], stats: &NetStats) -> Option<Vec<u8>> {
    if query.len() < 12 || query[2] & 0x80 != 0 {
        return None; // not a query
    }
    let (name, qend) = dns_name(query, 12)?;
    let lname = name.to_lowercase();
    if lname != "host.docker.internal" && lname != "gateway.docker.internal" {
        return None;
    }
    let qtype = u16::from_be_bytes([*query.get(qend)?, *query.get(qend + 1)?]);
    let question = query.get(12..qend + 4)?;
    let answer_a = qtype == 1;
    let mut resp = Vec::with_capacity(64);
    resp.extend_from_slice(&query[0..2]); // id
    resp.extend_from_slice(&[0x81, 0x80]); // response, recursion desired + available, NOERROR
    resp.extend_from_slice(&[0, 1, 0, u8::from(answer_a), 0, 0, 0, 0]); // 1 question, 0/1 answer
    resp.extend_from_slice(question);
    if answer_a {
        resp.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]); // name ptr, A, IN, ttl 60, len 4
        resp.extend_from_slice(&GATEWAY_V4.octets());
        stats.names.lock().unwrap().insert(Ipv4Addr::from(GATEWAY_V4.octets()), lname);
    }
    Some(resp)
}

/// Remember IPv4 answers from a DNS response, so later connections show the name the guest asked for.
fn learn_names(msg: &[u8], stats: &NetStats) {
    if msg.len() < 12 {
        return;
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
    let ancount = u16::from_be_bytes([msg[6], msg[7]]);
    let mut off = 12;
    let mut qname = String::new();
    for i in 0..qdcount {
        let Some((name, next)) = dns_name(msg, off) else { return };
        if i == 0 {
            qname = name;
        }
        off = next + 4; // qtype + qclass
    }
    if qname.is_empty() {
        return;
    }
    let mut names = stats.names.lock().unwrap();
    for _ in 0..ancount {
        let Some((_, next)) = dns_name(msg, off) else { return };
        let Some(rr) = msg.get(next..next + 10) else { return };
        let (rtype, rdlen) = (u16::from_be_bytes([rr[0], rr[1]]), u16::from_be_bytes([rr[8], rr[9]]) as usize);
        let data = next + 10;
        if rtype == 1 && rdlen == 4
            && let Some(a) = msg.get(data..data + 4)
        {
            names.insert(Ipv4Addr::new(a[0], a[1], a[2], a[3]), qname.clone());
        }
        off = data + rdlen;
    }
}

/// Reads a (possibly compressed) DNS name at `off`; returns it and the offset just past it.
fn dns_name(msg: &[u8], mut off: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *msg.get(off)? as usize;
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(off + 1)));
        }
        if len & 0xc0 == 0xc0 {
            let ptr = ((len & 0x3f) << 8) | *msg.get(off + 1)? as usize;
            end.get_or_insert(off + 2);
            off = ptr;
            continue;
        }
        labels.push(String::from_utf8_lossy(msg.get(off + 1..off + 1 + len)?).into_owned());
        off += 1 + len;
    }
    None
}

/// The host's first configured DNS server (ELPIS_DNS overrides; 1.1.1.1 if none is found).
fn host_dns() -> SocketAddr {
    use windows_sys::Win32::NetworkManagement::IpHelper::{FIXED_INFO_W2KSP1, GetNetworkParams};
    if let Some(ip) = std::env::var("ELPIS_DNS").ok().and_then(|v| v.parse::<IpAddr>().ok()) {
        return (ip, 53).into();
    }
    unsafe {
        let mut len: u32 = 0;
        GetNetworkParams(std::ptr::null_mut(), &mut len);
        let words = (len.max(size_of::<FIXED_INFO_W2KSP1>() as u32) as usize).div_ceil(8);
        let mut buf = vec![0u64; words]; // u64 storage keeps the struct's pointers aligned
        let info = buf.as_mut_ptr() as *mut FIXED_INFO_W2KSP1;
        if GetNetworkParams(info, &mut len) == 0 {
            let raw = &(*info).DnsServerList.IpAddress.String;
            let text: String = raw.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect();
            if let Ok(ip) = text.trim().parse::<IpAddr>() {
                return (ip, 53).into();
            }
        }
    }
    ([1, 1, 1, 1], 53).into()
}

/// With ELPIS_NET_DEBUG set, log IPv4/ARP frames in both directions (hex for ARP).
fn debug_frame(log: &Log, dir: &str, f: &[u8]) {
    if std::env::var_os("ELPIS_NET_DEBUG").is_none() || f.len() < 14 {
        return;
    }
    let et = u16::from_be_bytes([f[12], f[13]]);
    if et == 0x86dd {
        return; // IPv6 chatter (MLD, router solicitation) is noise here
    }
    let mac = |o: usize| f[o..o + 6].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
    let mut line = format!("{dir} {} > {} et=0x{et:04x} len={}", mac(6), mac(0), f.len());
    if et == 0x0806 {
        let hex: Vec<String> = f.iter().take(42).map(|b| format!("{b:02x}")).collect();
        line.push_str(&format!(" arp=[{}]", hex.join(" ")));
    } else if et == 0x0800 && f.len() >= 34 {
        line.push_str(&format!(" ip {}.{}.{}.{} > {}.{}.{}.{} proto={}", f[26], f[27], f[28], f[29], f[30], f[31], f[32], f[33], f[23]));
    }
    log.line("net", Color::DarkCyan, &line);
}

// ---- deframing + smoltcp device over the framed stream ----

fn deframe(mut sock: Socket, out: &Sender<Vec<u8>>, stop: &AtomicBool, _log: &Log) {
    let mut hdr = [0u8; 4];
    while !stop.load(Ordering::SeqCst) {
        if read_exact(&mut sock, &mut hdr).is_err() {
            break;
        }
        let len = u32::from_be_bytes(hdr) as usize;
        if len == 0 || len > 65535 {
            break;
        }
        let mut frame = vec![0u8; len];
        if read_exact(&mut sock, &mut frame).is_err() || out.send(frame).is_err() {
            break;
        }
    }
}

fn read_exact(sock: &mut Socket, buf: &mut [u8]) -> std::io::Result<()> {
    let mut got = 0;
    while got < buf.len() {
        match sock.read(&mut buf[got..]) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

struct FramedDevice {
    inbox: VecDeque<Vec<u8>>,
    tx: Sender<Vec<u8>>,
}

impl Device for FramedDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _t: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.inbox.pop_front().map(|buffer| (RxToken { buffer }, TxToken { tx: &self.tx }))
    }

    fn transmit(&mut self, _t: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(TxToken { tx: &self.tx })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps
    }
}

struct RxToken {
    buffer: Vec<u8>,
}
impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.buffer)
    }
}

struct TxToken<'a> {
    tx: &'a Sender<Vec<u8>>,
}
impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        let _ = self.tx.send(buffer);
        result
    }
}

// ---- annotations and helpers ----

/// Drop NIC annotations a previous `--net` run left in the spec; without our socket serving them, Sailor
/// waits on the dead socket and the guest gets a NIC that goes nowhere.
pub fn remove_annotations(config_path: &Path) -> Result<()> {
    let mut spec: Value = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
    let Some(annotations) = spec["annotations"].as_object_mut() else { return Ok(()) };
    let before = annotations.len();
    annotations.retain(|k, _| {
        !k.starts_with("io.containerd.nerdbox.network.") && !k.starts_with("io.containerd.nerdbox.ctr.network.") && k != "io.containerd.nerdbox.ctr.dns"
    });
    if annotations.len() != before {
        std::fs::write(config_path, serde_json::to_string_pretty(&spec)?)?;
    }
    Ok(())
}

fn inject_annotations(config_path: &Path, socket: &Path, id: &str, nic_mac: &str, ctr_mac: &str) -> Result<()> {
    let mut spec: Value = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
    let annotations = spec["annotations"].as_object_mut().context("spec has no annotations")?;
    annotations.retain(|k, _| !k.starts_with("io.containerd.nerdbox.network.") && !k.starts_with("io.containerd.nerdbox.ctr.network."));
    let socket = std::path::absolute(socket)?.to_string_lossy().into_owned();
    annotations.insert(
        format!("io.containerd.nerdbox.network.{id}"),
        json!(format!("socket={socket},mode=unixgram,vnet_hdr=false,mac={nic_mac}")),
    );
    annotations.insert(
        format!("io.containerd.nerdbox.ctr.network.{id}"),
        json!(format!("vmmac={nic_mac},mac={ctr_mac},addr={GUEST_IP},gw={GATEWAY_IP}")),
    );
    // The shim writes this annotation into the guest's /etc/resolv.conf verbatim
    annotations.insert("io.containerd.nerdbox.ctr.dns".into(), json!(format!("nameserver {GATEWAY_IP}\n")));
    std::fs::write(config_path, serde_json::to_string_pretty(&spec)?)?;
    Ok(())
}

fn now() -> SmolInstant {
    SmolInstant::from_micros(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as i64)
}

fn seed() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() ^ u128::from(std::process::id())
}

fn net_id() -> String {
    let mut x = seed();
    (0..64)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            char::from_digit(((x >> 60) & 0xf) as u32, 16).unwrap()
        })
        .collect()
}

fn random_mac(first: u8) -> String {
    let mut x = seed().wrapping_add(u128::from(first) << 40);
    let mut octet = || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (x >> 56) as u8
    };
    format!("{first:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", octet(), octet(), octet(), octet(), octet())
}

fn gw_mac_bytes(mac: &str) -> [u8; 6] {
    let mut out = [0u8; 6];
    for (i, part) in mac.split(':').take(6).enumerate() {
        out[i] = u8::from_str_radix(part, 16).unwrap_or(0);
    }
    out
}
