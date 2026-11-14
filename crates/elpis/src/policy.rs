//! Network policy for a VM's egress, modeled on `sbx policy`: allow/deny rules matched against the
//! destination's hostname (as the guest resolved it through our DNS) or IP, with deny beating allow and a
//! default for everything else. Persisted per work folder in `network-policy.json`, and reloaded when that
//! file changes, so a running VM picks up edits made by another process (the CLI, a TUI).
//!
//! The default can also be `ask` (sbx's approval prompts): a connection no rule covers waits for an answer.
//! "Allow once" answers are kept in memory only, for as long as the VM runs.
//!
//! The Windows host (the gateway address, `host.docker.internal`) is opt-in: only a rule or grant that names
//! it allows it. `**`, CIDRs and an `allow` default don't, so a box can't reach services on the host's
//! loopback unless someone allowed exactly that.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Deny,
    /// Only as the default: hold the connection until someone allows or denies it
    Ask,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
            Action::Ask => "ask",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    pub id: u64,
    /// `example.com`, `*.example.com` (any subdomain), `**.example.com` (it and any subdomain), `**` (everything), `1.2.3.4`, `10.0.0.0/8`,
    /// each optionally with `:PORT`
    pub pattern: String,
    pub action: Action,
    pub created: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyFile {
    pub default: Action,
    pub rules: Vec<Rule>,
}

impl Default for PolicyFile {
    fn default() -> Self {
        PolicyFile { default: Action::Allow, rules: Vec::new() }
    }
}

/// The outcome for one destination: what happened and which rule (if any) decided it.
#[derive(Clone, Debug)]
pub struct Decision {
    pub action: Action,
    pub rule: Option<String>,
}

pub struct Policy {
    path: PathBuf,
    state: RwLock<(PolicyFile, Option<SystemTime>)>,
    /// Hosts allowed until the VM stops ("allow once")
    grants: Mutex<HashSet<String>>,
}

impl Policy {
    pub fn path_for(work: &Path) -> PathBuf {
        work.join("network-policy.json")
    }

    /// Loads the policy for a work folder; a missing file means "allow everything, no rules".
    pub fn load(work: &Path) -> Arc<Policy> {
        let path = Self::path_for(work);
        let (file, mtime) = read(&path);
        Arc::new(Policy { path, state: RwLock::new((file, mtime)), grants: Mutex::default() })
    }

    fn refresh(&self) {
        let disk = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if disk != self.state.read().unwrap().1 {
            *self.state.write().unwrap() = read(&self.path);
        }
    }

    pub fn snapshot(&self) -> PolicyFile {
        self.refresh();
        self.state.read().unwrap().0.clone()
    }

    pub fn decide(&self, name: Option<&str>, ip: IpAddr, port: u16) -> Decision {
        self.refresh();
        let state = self.state.read().unwrap();
        let host = ip == HOST_IP;
        // Rules about the host match it however the box addressed it (by name, either alias, or IP)
        let name = if host { Some(name.unwrap_or(HOST_NAMES[0])) } else { name };
        let hits = |pattern: &str| matches(pattern, name, ip, port) || (host && matches(pattern, Some(HOST_NAMES[0]), ip, port));
        // Any deny covers the host, but only an allow that names it opens it
        let applies = |pattern: &str, action| hits(pattern) && (action == Action::Deny || !host || names_host(pattern));
        let matching = |action| state.0.rules.iter().find(|r| r.action == action && applies(&r.pattern, action));
        if let Some(r) = matching(Action::Deny) {
            return Decision { action: Action::Deny, rule: Some(r.pattern.clone()) };
        }
        if let Some(r) = matching(Action::Allow) {
            return Decision { action: Action::Allow, rule: Some(r.pattern.clone()) };
        }
        if let Some(g) = self.grants.lock().unwrap().iter().find(|g| applies(g, Action::Allow)) {
            return Decision { action: Action::Allow, rule: Some(format!("{g} (once)")) };
        }
        if host && state.0.default != Action::Ask {
            return Decision { action: Action::Deny, rule: Some("host access is opt-in".into()) };
        }
        Decision { action: state.0.default, rule: None }
    }

    /// Allow a host until the VM stops, without saving a rule.
    pub fn grant_once(&self, pattern: &str) -> Result<()> {
        let pattern = pattern.trim().to_lowercase();
        validate(&pattern)?;
        self.grants.lock().unwrap().insert(pattern);
        Ok(())
    }

    /// Adds a rule, replacing any existing rule for the same pattern. Returns the stored rule.
    pub fn add(&self, pattern: &str, action: Action) -> Result<Rule> {
        if action == Action::Ask {
            bail!("'ask' can only be the default; rules allow or deny");
        }
        let pattern = pattern.trim().to_lowercase();
        validate(&pattern)?;
        self.update(|file| {
            file.rules.retain(|r| r.pattern != pattern);
            let id = file.rules.iter().map(|r| r.id).max().unwrap_or(0) + 1;
            let rule = Rule { id, pattern: pattern.clone(), action, created: crate::win::local_time() };
            file.rules.push(rule.clone());
            rule
        })
    }

    pub fn remove(&self, id: u64) -> Result<bool> {
        self.update(|file| {
            let before = file.rules.len();
            file.rules.retain(|r| r.id != id);
            file.rules.len() != before
        })
    }

    pub fn set_default(&self, action: Action) -> Result<()> {
        self.update(|file| file.default = action)
    }

    fn update<T>(&self, change: impl FnOnce(&mut PolicyFile) -> T) -> Result<T> {
        self.refresh();
        let mut state = self.state.write().unwrap();
        let out = change(&mut state.0);
        std::fs::write(&self.path, serde_json::to_string_pretty(&state.0)?).with_context(|| format!("writing {}", self.path.display()))?;
        state.1 = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        Ok(out)
    }
}

fn read(path: &Path) -> (PolicyFile, Option<SystemTime>) {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let file = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    (file, mtime)
}

/// Splits an optional `:PORT` off a pattern (IPv6 isn't supported here, so the last colon is the port).
fn split_port(pattern: &str) -> (&str, Option<u16>) {
    match pattern.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => match port.parse() {
            Ok(p) => (host, Some(p)),
            Err(_) => (pattern, None),
        },
        _ => (pattern, None),
    }
}

fn validate(pattern: &str) -> Result<()> {
    let (host, _) = split_port(pattern);
    if host.is_empty() || host == "*" {
        bail!("pattern '{pattern}' matches nothing useful; use '**' for everything");
    }
    if let Some((ip, bits)) = host.split_once('/') {
        let ok = ip.parse::<IpAddr>().is_ok() && bits.parse::<u8>().is_ok_and(|b| b <= 32);
        if !ok {
            bail!("'{host}' is not a valid IPv4 CIDR");
        }
    }
    Ok(())
}

/// The gateway address, which the network stack maps to the Windows host's loopback.
const HOST_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(172, 17, 0, 0));
/// Names the box's DNS answers with [`HOST_IP`].
const HOST_NAMES: [&str; 2] = ["host.docker.internal", "gateway.docker.internal"];

/// Whether a pattern names the host itself (optionally with a port), as opposed to covering it by accident.
fn names_host(pattern: &str) -> bool {
    let host = split_port(pattern).0.to_lowercase();
    HOST_NAMES.contains(&host.as_str()) || host.parse::<IpAddr>() == Ok(HOST_IP)
}

fn matches(pattern: &str, name: Option<&str>, ip: IpAddr, port: u16) -> bool {
    let (host, want_port) = split_port(pattern);
    if want_port.is_some_and(|p| p != port) {
        return false;
    }
    if host == "**" {
        return true;
    }
    if let Some((net, bits)) = host.split_once('/') {
        if let (Ok(IpAddr::V4(net)), Ok(bits), IpAddr::V4(ip)) = (net.parse::<IpAddr>(), bits.parse::<u32>(), ip) {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            return u32::from(net) & mask == u32::from(ip) & mask;
        }
        return false;
    }
    if let Ok(literal) = host.parse::<IpAddr>() {
        return literal == ip;
    }
    let Some(name) = name.map(str::to_lowercase) else { return false };
    if let Some(domain) = host.strip_prefix("**.") {
        return name == domain || name.ends_with(&format!(".{domain}"));
    }
    match host.strip_prefix("*.") {
        Some(domain) => name.ends_with(&format!(".{domain}")),
        None => name == host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        let ip: IpAddr = "93.184.215.14".parse().unwrap();
        assert!(matches("example.com", Some("example.com"), ip, 443));
        assert!(!matches("example.com", Some("www.example.com"), ip, 443));
        assert!(matches("*.example.com", Some("www.example.com"), ip, 443));
        assert!(!matches("*.example.com", Some("example.com"), ip, 443));
        assert!(matches("example.com:443", Some("example.com"), ip, 443));
        assert!(!matches("example.com:80", Some("example.com"), ip, 443));
        assert!(matches("93.184.0.0/16", None, ip, 1));
        assert!(matches("93.184.215.14", None, ip, 1));
        assert!(matches("**", None, ip, 1));
        assert!(matches("**.example.com", Some("example.com"), ip, 443));
        assert!(matches("**.example.com", Some("a.b.example.com"), ip, 443));
        assert!(!matches("example.com", None, ip, 443));
    }

    #[test]
    fn ask_and_grants() {
        let dir = std::env::temp_dir().join(format!("elpis-policy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = Policy::load(&dir);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        p.set_default(Action::Ask).unwrap();
        assert_eq!(p.decide(Some("a.example"), ip, 443).action, Action::Ask);
        assert!(p.add("a.example", Action::Ask).is_err()); // ask is only a default
        p.grant_once("a.example").unwrap();
        let d = p.decide(Some("a.example"), ip, 443);
        assert_eq!((d.action, d.rule.as_deref()), (Action::Allow, Some("a.example (once)")));
        p.add("a.example", Action::Deny).unwrap(); // a saved deny beats a grant
        assert_eq!(p.decide(Some("a.example"), ip, 443).action, Action::Deny);
        assert_eq!(Policy::load(&dir).snapshot().default, Action::Ask); // grants aren't saved, the default is
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_is_opt_in() {
        let dir = std::env::temp_dir().join(format!("elpis-policy-host-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = Policy::load(&dir);
        let gw: IpAddr = "172.17.0.0".parse().unwrap();
        let other: IpAddr = "1.2.3.4".parse().unwrap();
        // Default allow: the internet is open, the host isn't
        assert_eq!(p.decide(None, other, 443).action, Action::Allow);
        assert_eq!(p.decide(Some("host.docker.internal"), gw, 11434).action, Action::Deny);
        assert_eq!(p.decide(None, gw, 11434).action, Action::Deny);
        // Broad rules don't open it
        p.add("**", Action::Allow).unwrap();
        p.add("172.16.0.0/12", Action::Allow).unwrap();
        assert_eq!(p.decide(None, gw, 11434).action, Action::Deny);
        // A rule naming it does, by either name or the IP, and a port limits it
        p.add("host.docker.internal:11434", Action::Allow).unwrap();
        assert_eq!(p.decide(None, gw, 11434).action, Action::Allow);
        assert_eq!(p.decide(Some("gateway.docker.internal"), gw, 11434).action, Action::Allow);
        assert_eq!(p.decide(None, gw, 22).action, Action::Deny);
        // Ask still asks
        p.set_default(Action::Ask).unwrap();
        assert_eq!(p.decide(None, gw, 22).action, Action::Ask);
        p.grant_once("172.17.0.0:22").unwrap();
        assert_eq!(p.decide(None, gw, 22).action, Action::Allow);
        // A broad deny still covers it
        p.add("172.16.0.0/12", Action::Deny).unwrap();
        assert_eq!(p.decide(None, gw, 11434).action, Action::Deny);
        let _ = std::fs::remove_dir_all(&dir);
    }
}