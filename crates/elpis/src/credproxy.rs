//! The credential proxy, like sbx's: API keys stay on the host (see [`crate::secrets`]) and are added to a box's
//! HTTPS requests on the way out, so the agent inside only ever holds the sentinel `proxy-managed`.
//!
//! What to inject comes from the box's kit: each credential's `apiKey.inject` rules say "on requests to
//! DOMAIN, set HEADER to FORMAT with %s = the key". A TCP flow to port 443 of such a domain (named by the box's
//! own DNS lookups) gets an intercepting worker instead of a plain relay:
//!
//! ```text
//! box ──TLS (cert for the host, signed by elpis's CA)──► proxy ──TLS (verified, public roots)──► the API
//!                                     request heads rewritten: HEADER: FORMAT(key)
//! ```
//!
//! The box trusts elpis's CA because the engine installs it there at start ([`guest_setup_script`]). Only
//! HTTP/1.1 is offered to the box, so every request head can be read and rewritten; responses pass through
//! as they are.

use crate::kits::Kit;
use crate::log::{Color, Log};
use anyhow::{Context, Result, bail};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The value a proxy-managed key variable holds inside the box.
pub const SENTINEL: &str = "proxy-managed";
/// Where the box finds elpis's CA (installed into the system trust store too).
/// The CA's common name; leaves name it as their issuer.
const CA_NAME: &str = "Elpis credential proxy CA";
pub const GUEST_CA_PATH: &str = "/usr/local/share/ca-certificates/elpis-proxy.crt";

/// One injection, resolved: requests to `domain` get `header: value`.
#[derive(Clone, Debug)]
pub struct Rule {
    pub domain: String,
    pub header: String,
    pub value: String,
    pub service: String,
}

pub struct CredProxy {
    rules: Vec<Rule>,
    /// `NAME=proxy-managed` for the kit's proxy-managed key variables that have a secret
    env: Vec<String>,
    ca: Ca,
    client: Arc<ClientConfig>,
    server: Arc<ServerConfig>,
}

impl std::fmt::Debug for CredProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredProxy").field("services", &self.services()).finish()
    }
}

impl CredProxy {
    /// The proxy for a box made from a kit, if any of the kit's credentials has a secret stored.
    pub fn for_box(work: &Path) -> Result<Option<Arc<CredProxy>>> {
        let Some(kit) = crate::boxfile::BoxFile::load(work).and_then(|b| b.kit).and_then(|k: String| crate::kits::cached(k.as_str())) else {
            return Ok(None);
        };
        Self::for_kit(&kit, crate::secrets::get)
    }

    /// The rules of `kit` for which `secret` has a value.
    pub fn for_kit(kit: &Kit, secret: impl Fn(&str) -> Option<String>) -> Result<Option<Arc<CredProxy>>> {
        let (mut rules, mut env) = (Vec::new(), Vec::new());
        for c in &kit.credentials {
            let Some(key) = &c.api_key else { continue };
            if key.inject.is_empty() {
                continue;
            }
            let Some(value) = secret(&c.service) else { continue };
            for i in &key.inject {
                if i.domain.is_empty() || i.header.is_empty() {
                    continue;
                }
                let format = if i.format.is_empty() { "%s" } else { &i.format };
                rules.push(Rule {
                    domain: i.domain.to_ascii_lowercase(),
                    header: i.header.clone(),
                    value: format.replacen("%s", value.trim(), 1),
                    service: c.service.clone(),
                });
            }
            if key.proxy_managed && !key.name.is_empty() {
                env.push(format!("{}={SENTINEL}", key.name));
            }
        }
        if rules.is_empty() {
            return Ok(None);
        }
        let ca = Ca::load_or_create(&crate::session::debug_root().join("proxy"))?;
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut client = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        let resolver = Arc::new(LeafResolver { ca: ca.clone_signer()?, cache: Mutex::new(HashMap::new()), provider: provider.clone() });
        let mut server = ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_no_client_auth().with_cert_resolver(resolver);
        // HTTP/1.1 only, so every request head is ours to read
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Some(Arc::new(CredProxy { rules, env, ca, client: Arc::new(client), server: Arc::new(server) })))
    }

    /// The rule for a host the box connects to (exact name match, case-insensitive).
    pub fn rule_for(&self, host: &str) -> Option<&Rule> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.rules.iter().find(|r| r.domain == host)
    }

    pub fn services(&self) -> Vec<String> {
        let mut s: Vec<String> = self.rules.iter().map(|r| r.service.clone()).collect();
        s.dedup();
        s
    }

    /// `NAME=proxy-managed` for each proxy-managed key variable, for the box's environment.
    pub fn env(&self) -> &[String] {
        &self.env
    }

    pub fn ca_pem(&self) -> &str {
        &self.ca.cert_pem
    }

    /// Takes over a flow from the box to `host` (at `real`): TLS in from the box, TLS out to the host, request
    /// heads rewritten in between. Runs until either side is done.
    pub fn intercept(
        self: Arc<Self>,
        real: SocketAddr,
        host: String,
        from_guest: Receiver<Vec<u8>>,
        to_guest: Sender<Vec<u8>>,
        done: Arc<AtomicBool>,
        log: Log,
    ) {
        if let Err(e) = self.run(real, &host, from_guest, &to_guest, &log) {
            log.line("proxy", Color::Yellow, &format!("{host}: {e:#}"));
        }
        done.store(true, Ordering::SeqCst);
    }

    fn run(&self, real: SocketAddr, host: &str, from_guest: Receiver<Vec<u8>>, to_guest: &Sender<Vec<u8>>, log: &Log) -> Result<()> {
        let rule = self.rule_for(host).context("no rule for this host")?.clone();
        let mut upstream = TcpStream::connect_timeout(&real, Duration::from_secs(10)).context("connecting")?;
        upstream.set_read_timeout(Some(Duration::from_millis(15)))?;
        let _ = upstream.set_nodelay(true);
        let name = ServerName::try_from(host.to_owned()).context("bad host name")?;
        let mut client = ClientConnection::new(self.client.clone(), name)?;
        let mut server = ServerConnection::new(self.server.clone())?;
        let mut requests = RequestRewriter::new(&rule);
        let mut buf = vec![0u8; 32 * 1024];
        let (mut guest_eof, mut upstream_eof) = (false, false);
        let mut injected_logged = false;

        loop {
            // box → proxy (TLS records from the box)
            loop {
                match from_guest.try_recv() {
                    Ok(data) => {
                        let mut slice = data.as_slice();
                        while !slice.is_empty() {
                            server.read_tls(&mut slice)?;
                            if let Err(e) = server.process_new_packets() {
                                let _ = flush_server(&mut server, to_guest);
                                bail!("TLS from the box failed ({e}); is elpis's CA trusted in the box?");
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        guest_eof = true;
                        break;
                    }
                }
            }
            // plaintext from the box → rewritten → upstream
            loop {
                match server.reader().read(&mut buf) {
                    Ok(0) => {
                        guest_eof = true;
                        client.send_close_notify();
                        break;
                    }
                    Ok(n) => {
                        let out = requests.feed(&buf[..n]);
                        if requests.injected > 0 && !injected_logged {
                            log.line("proxy", Color::Cyan, &format!("{host}: {} added for {}", rule.header, rule.service));
                            injected_logged = true;
                        }
                        client.writer().write_all(&out)?;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            while client.wants_write() {
                client.write_tls(&mut upstream)?;
            }

            // upstream → proxy
            if !upstream_eof {
                match client.read_tls(&mut upstream) {
                    Ok(0) => upstream_eof = true,
                    Ok(_) => {
                        client.process_new_packets().context("TLS to the server failed")?;
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            // plaintext from upstream → the box
            loop {
                match client.reader().read(&mut buf) {
                    Ok(0) => {
                        upstream_eof = true;
                        server.send_close_notify();
                        break;
                    }
                    Ok(n) => server.writer().write_all(&buf[..n])?,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    // The server closed without close_notify: treat as the end
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                        upstream_eof = true;
                        server.send_close_notify();
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            if !flush_server(&mut server, to_guest) {
                return Ok(()); // the flow was reaped
            }

            if upstream_eof && !server.wants_write() {
                return Ok(());
            }
            if guest_eof && !client.wants_write() && upstream_eof {
                return Ok(());
            }
            if guest_eof && !server.is_handshaking() && server.wants_read() && requests.idle() && from_guest_gone(&from_guest) {
                // the box is gone and nothing is in flight
                return Ok(());
            }
        }
    }
}

fn from_guest_gone(rx: &Receiver<Vec<u8>>) -> bool {
    matches!(rx.try_recv(), Err(TryRecvError::Disconnected))
}

/// Sends the TLS records the box-side session has queued; false if the flow is gone.
fn flush_server(server: &mut ServerConnection, to_guest: &Sender<Vec<u8>>) -> bool {
    while server.wants_write() {
        let mut out = Vec::new();
        if server.write_tls(&mut out).is_err() {
            return false;
        }
        if !out.is_empty() && to_guest.send(out).is_err() {
            return false;
        }
    }
    true
}

// ---- the CA and per-host certificates ----

struct Ca {
    cert_pem: String,
    key_pem: String,
}

impl Ca {
    /// `dir\ca.pem` and `dir\ca.key.pem`, made on first use.
    fn load_or_create(dir: &Path) -> Result<Ca> {
        let (cert_path, key_path) = (dir.join("ca.pem"), dir.join("ca.key.pem"));
        if let (Ok(cert_pem), Ok(key_pem)) = (std::fs::read_to_string(&cert_path), std::fs::read_to_string(&key_path)) {
            // Leaves are signed as CA_NAME (see `params`): a stored CA with another name (an older version's)
            // wouldn't chain, so it's replaced
            let der = rustls_pemfile_cert(&cert_pem).map(|c| c.to_vec()).unwrap_or_default();
            if der.windows(CA_NAME.len()).any(|w| w == CA_NAME.as_bytes()) {
                return Ok(Ca { cert_pem, key_pem });
            }
        }
        std::fs::create_dir_all(dir)?;
        let key = rcgen::KeyPair::generate()?;
        let cert = Self::params().self_signed(&key)?;
        let ca = Ca { cert_pem: cert.pem(), key_pem: key.serialize_pem() };
        std::fs::write(&key_path, &ca.key_pem)?;
        std::fs::write(&cert_path, &ca.cert_pem)?;
        Ok(ca)
    }

    fn params() -> rcgen::CertificateParams {
        let mut p = rcgen::CertificateParams::default();
        p.distinguished_name = rcgen::DistinguishedName::new();
        p.distinguished_name.push(rcgen::DnType::CommonName, CA_NAME);
        p.distinguished_name.push(rcgen::DnType::OrganizationName, "elpis");
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        p.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign, rcgen::KeyUsagePurpose::DigitalSignature];
        p
    }

    /// The CA as a signer: the same name and key as the stored certificate, so what it signs chains to it.
    fn clone_signer(&self) -> Result<Signer> {
        let key = rcgen::KeyPair::from_pem(&self.key_pem)?;
        let cert = Self::params().self_signed(&key)?;
        let ca_der = rustls_pemfile_cert(&self.cert_pem)?;
        Ok(Signer { cert, key, ca_der })
    }
}

fn rustls_pemfile_cert(pem: &str) -> Result<CertificateDer<'static>> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64_decode(&body).context("bad CA certificate")?;
    Ok(CertificateDer::from(der))
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        acc = (acc << 6) | T.iter().position(|&t| t == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

struct Signer {
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
    ca_der: CertificateDer<'static>,
}

/// Picks (and makes, once per host) the leaf certificate for the name the box asked for in its ClientHello.
struct LeafResolver {
    ca: Signer,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl std::fmt::Debug for LeafResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LeafResolver")
    }
}

impl LeafResolver {
    fn leaf(&self, host: &str) -> Result<Arc<CertifiedKey>> {
        if let Some(k) = self.cache.lock().unwrap().get(host) {
            return Ok(k.clone());
        }
        let key = rcgen::KeyPair::generate()?;
        let mut params = rcgen::CertificateParams::new(vec![host.to_owned()])?;
        params.distinguished_name.push(rcgen::DnType::CommonName, host);
        params.use_authority_key_identifier_extension = true;
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &self.ca.cert, &self.ca.key)?;
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing = self.provider.key_provider.load_private_key(der)?;
        let certified = Arc::new(CertifiedKey::new(vec![cert.der().clone(), self.ca.ca_der.clone()], signing));
        self.cache.lock().unwrap().insert(host.to_owned(), certified.clone());
        Ok(certified)
    }
}

impl ResolvesServerCert for LeafResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.leaf(hello.server_name()?).ok()
    }
}

// ---- HTTP/1.1 request rewriting ----

enum State {
    /// Collecting a request head
    Head(Vec<u8>),
    /// Passing `n` more body bytes
    Body(u64),
    /// Chunked body: reading a chunk-size line
    ChunkSize(Vec<u8>),
    /// Chunked body: `n` more data bytes, then CRLF
    ChunkData(u64),
    /// Chunked body: trailers until an empty line
    Trailers(Vec<u8>),
    /// After an Upgrade (WebSocket): bytes pass through untouched
    Raw,
}

/// Follows the box's request stream and rewrites each request head; bodies pass through unchanged.
struct RequestRewriter {
    header: String,
    value: String,
    state: State,
    injected: u64,
}

const MAX_HEAD: usize = 256 * 1024;

impl RequestRewriter {
    fn new(rule: &Rule) -> Self {
        Self { header: rule.header.clone(), value: rule.value.clone(), state: State::Head(Vec::new()), injected: 0 }
    }

    fn idle(&self) -> bool {
        matches!(&self.state, State::Head(b) if b.is_empty())
    }

    fn feed(&mut self, mut input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + 128);
        while !input.is_empty() {
            match &mut self.state {
                State::Raw => {
                    out.extend_from_slice(input);
                    input = &[];
                }
                State::Head(head) => {
                    head.extend_from_slice(input);
                    input = &[];
                    if let Some(end) = find(head, b"\r\n\r\n") {
                        let rest = head.split_off(end + 4);
                        let head = std::mem::take(head);
                        let (rewritten, next) = self.rewrite(&head);
                        out.extend_from_slice(&rewritten);
                        self.state = next;
                        // re-feed what followed the head under the new state
                        let mut more = self.feed(&rest);
                        out.append(&mut more);
                    } else if head.len() > MAX_HEAD {
                        // Not HTTP we understand: stop rewriting rather than buffer forever
                        out.append(head);
                        self.state = State::Raw;
                    }
                }
                State::Body(n) => {
                    let take = (*n).min(input.len() as u64) as usize;
                    out.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    *n -= take as u64;
                    if *n == 0 {
                        self.state = State::Head(Vec::new());
                    }
                }
                State::ChunkSize(line) => {
                    let b = input[0];
                    input = &input[1..];
                    out.push(b);
                    line.push(b);
                    if line.ends_with(b"\r\n") {
                        let text = String::from_utf8_lossy(&line[..line.len() - 2]).into_owned();
                        let size = u64::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
                        self.state = if size == 0 { State::Trailers(Vec::new()) } else { State::ChunkData(size + 2) };
                    }
                }
                State::ChunkData(n) => {
                    let take = (*n).min(input.len() as u64) as usize;
                    out.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    *n -= take as u64;
                    if *n == 0 {
                        self.state = State::ChunkSize(Vec::new());
                    }
                }
                State::Trailers(line) => {
                    let b = input[0];
                    input = &input[1..];
                    out.push(b);
                    line.push(b);
                    if line.ends_with(b"\r\n") {
                        if line.len() == 2 {
                            self.state = State::Head(Vec::new());
                        } else {
                            line.clear();
                        }
                    }
                }
            }
        }
        out
    }

    /// The head with our header set (replacing any the box sent), and how the body that follows is framed.
    fn rewrite(&mut self, head: &[u8]) -> (Vec<u8>, State) {
        let text = String::from_utf8_lossy(head);
        let mut lines = text.split("\r\n").filter(|l| !l.is_empty());
        let request_line = lines.next().unwrap_or_default();
        let (mut length, mut chunked, mut upgrade) = (0u64, false, false);
        let mut out = format!("{request_line}\r\n");
        for line in lines {
            let (name, value) = line.split_once(':').map(|(n, v)| (n.trim(), v.trim())).unwrap_or((line, ""));
            if name.eq_ignore_ascii_case(&self.header) {
                continue;
            }
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            } else if name.eq_ignore_ascii_case("upgrade") {
                upgrade = true;
            }
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.push_str(&format!("{}: {}\r\n\r\n", self.header, self.value));
        self.injected += 1;
        let next = if upgrade {
            State::Raw
        } else if chunked {
            State::ChunkSize(Vec::new())
        } else if length > 0 {
            State::Body(length)
        } else {
            State::Head(Vec::new())
        };
        (out.into_bytes(), next)
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---- the box side ----

/// A root shell script that trusts elpis's CA in the box: written where Debian/Ubuntu/Alpine look, added to
/// the system bundle (update-ca-certificates, else appended), and kept at [`GUEST_CA_PATH`] for
/// `NODE_EXTRA_CA_CERTS`.
pub fn guest_setup_script(ca_pem: &str) -> String {
    let b64 = base64_encode(ca_pem.as_bytes());
    format!(
        "set -e; mkdir -p /usr/local/share/ca-certificates; \
         echo {b64} | base64 -d > {GUEST_CA_PATH}; \
         if command -v update-ca-certificates >/dev/null 2>&1; then update-ca-certificates >/dev/null 2>&1 || true; \
         elif [ -f /etc/ssl/certs/ca-certificates.crt ] && ! grep -q 'elpis' /etc/ssl/certs/ca-certificates.crt; then cat {GUEST_CA_PATH} >> /etc/ssl/certs/ca-certificates.crt; fi"
    )
}

/// Environment for the box's processes: proxy-managed key sentinels, and the CA for Node/Bun/Python/Go/curl.
pub fn guest_env(proxy: &CredProxy) -> Vec<String> {
    let mut env = proxy.env().to_vec();
    env.push(format!("NODE_EXTRA_CA_CERTS={GUEST_CA_PATH}"));
    env.push("REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt".into());
    env
}

/// The environment keys elpis manages in a spec, so a box without the proxy loses them again.
pub fn managed_env_keys(kit: Option<&Kit>) -> Vec<String> {
    let mut keys = vec!["NODE_EXTRA_CA_CERTS".to_owned(), "REQUESTS_CA_BUNDLE".to_owned()];
    if let Some(kit) = kit {
        keys.extend(kit.credentials.iter().filter_map(|c| c.api_key.as_ref()).filter(|k| k.proxy_managed && !k.name.is_empty()).map(|k| k.name.clone()));
    }
    keys
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Sets the box's proxy environment in its spec (`proxy`), or removes it (`None`), leaving the rest alone.
pub fn apply_env(config_path: &Path, work: &Path, proxy: Option<&CredProxy>) -> Result<()> {
    let kit = crate::boxfile::BoxFile::load(work).and_then(|b| b.kit).and_then(|k: String| crate::kits::cached(k.as_str()));
    let keys = managed_env_keys(kit.as_ref());
    let mut spec: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
    let Some(env) = spec["process"]["env"].as_array_mut() else { return Ok(()) };
    let before = env.clone();
    env.retain(|e| e.as_str().and_then(|s| s.split_once('=')).is_none_or(|(k, _)| !keys.iter().any(|m| m == k)));
    if let Some(p) = proxy {
        env.extend(guest_env(p).into_iter().map(serde_json::Value::String));
    }
    if *env != before {
        std::fs::write(config_path, serde_json::to_string_pretty(&spec)?)?;
    }
    Ok(())
}

/// Where elpis keeps its CA.
pub fn ca_dir() -> PathBuf {
    crate::session::debug_root().join("proxy")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Rule {
        Rule { domain: "api.example.com".into(), header: "x-api-key".into(), value: "REAL".into(), service: "example".into() }
    }

    #[test]
    fn rewrites_heads_across_bodies_and_pipelining() {
        let mut r = RequestRewriter::new(&rule());
        let input = b"POST /v1 HTTP/1.1\r\nHost: api.example.com\r\nX-Api-Key: proxy-managed\r\nContent-Length: 5\r\n\r\nhelloGET /two HTTP/1.1\r\nHost: a\r\n\r\n";
        // feed in awkward pieces
        let mut out = Vec::new();
        for piece in input.chunks(7) {
            out.extend(r.feed(piece));
        }
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("proxy-managed"), "{text}");
        assert_eq!(text.matches("x-api-key: REAL\r\n").count(), 2, "{text}");
        assert!(text.contains("\r\n\r\nhelloGET /two"), "{text}");
        assert!(r.idle());
    }

    #[test]
    fn chunked_bodies() {
        let mut r = RequestRewriter::new(&rule());
        let input = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\nGET / HTTP/1.1\r\n\r\n";
        let text = String::from_utf8(r.feed(input)).unwrap();
        assert_eq!(text.matches("x-api-key: REAL").count(), 2, "{text}");
        assert!(r.idle());
    }

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "abcd", "-----BEGIN CERTIFICATE-----\nxyz\n"] {
            assert_eq!(base64_decode(&base64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
    }

    #[test]
    fn rules_from_a_kit() {
        let kit: Kit = serde_yaml::from_str(
            "credentials:\n- service: anthropic\n  apiKey:\n    name: ANTHROPIC_API_KEY\n    proxyManaged: true\n    inject:\n    - domain: api.anthropic.com\n      header: x-api-key\n      format: \"%s\"\n- service: github\n  apiKey:\n    name: \"\"\n    inject:\n    - domain: api.github.com\n      header: Authorization\n      format: Bearer %s\n",
        )
        .unwrap();
        let secret = |s: &str| (s == "github").then(|| "ghp_x".to_owned());
        let rules: Vec<Rule> = kit
            .credentials
            .iter()
            .filter_map(|c| c.api_key.as_ref().map(|k| (c, k)))
            .filter(|(c, _)| secret(&c.service).is_some())
            .flat_map(|(c, k)| k.inject.iter().map(move |i| (c.service.clone(), i.clone())))
            .map(|(service, i)| Rule { domain: i.domain, header: i.header, value: i.format.replacen("%s", "ghp_x", 1), service })
            .collect();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value, "Bearer ghp_x");
    }
}
