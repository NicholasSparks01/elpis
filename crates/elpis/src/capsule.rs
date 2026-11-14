//! Capsules: a portable, versioned description of an agent implementation (see `docs/capsule-manifest.md`).
//!
//! A Capsule says *what to run*; credentials, workspaces, host ports and network permissions come from the
//! consumer. This module is the manifest only: parse, validate, and turn it into a [`BoxFile`]. Building
//! layers, registries and signing come later.
//!
//! The manifest is Elpis's own format. Its field names deliberately differ from sbx kits (`crate::kits`), and
//! it must never reuse the sbx kit media type. Unknown fields are rejected: a typo in `requests` that
//! silently dropped a restriction would be worse than an error.

use crate::boxfile::{BoxFile, PortMap, SetupCommand, Volume};
use crate::kits::{CommandSpec, InjectSpec, StepSpec};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const API_VERSION: &str = "elpis.dev/v1alpha1";
pub const KIND: &str = "Capsule";
/// Binding types a Capsule may declare.
const BINDING_TYPES: &[&str] = &["mcp-endpoint", "url", "string"];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Capsule {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: Spec,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Metadata {
    /// `namespace/name`
    pub name: String,
    /// `MAJOR.MINOR.PATCH`; immutable once published
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Filled in by `capsule build`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<GitSource>,
    /// `namespace/name@version` of the Capsule this was forked from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSource {
    pub git: String,
    pub commit: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Spec {
    #[serde(default)]
    pub compatibility: Compatibility,
    pub runtime: Runtime,
    #[serde(default)]
    pub payload: Vec<PayloadEntry>,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub interfaces: Interfaces,
    #[serde(default)]
    pub mcp: McpSpec,
    #[serde(default)]
    pub requests: Requests,
    /// Named inputs the consumer supplies at run time
    #[serde(default)]
    pub bindings: BTreeMap<String, Binding>,
    #[serde(default)]
    pub tests: Vec<TestSpec>,
    #[serde(default)]
    pub examples: Vec<Example>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    /// A minimum Elpis version: `">=0.4"` or `">=0.4.1"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elpis: Option<String>,
    /// The guest kernel/rootfs build it was tested on (informational)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Runtime {
    /// Pinned by digest (`name@sha256:…`); tags are rejected
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub entrypoint: Vec<String>,
    /// Never secrets
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// First boot only
    #[serde(default)]
    pub install: Vec<StepSpec>,
    /// Every boot
    #[serde(default)]
    pub startup: Vec<StepSpec>,
}

/// A folder of the Capsule shipped as an OCI layer and copied into the box.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadEntry {
    /// Relative to the Capsule folder
    pub from: String,
    /// Absolute path in the box
    pub to: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Resources {
    #[serde(default)]
    pub volumes: Vec<VolumeReq>,
    #[serde(default)]
    pub docker: bool,
    /// Seconds after the last terminal closes; 0 keeps the box running
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_stop: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeReq {
    pub path: String,
    /// "2g", "512m"
    pub size: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interfaces {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceReq>,
    #[serde(default)]
    pub ports: Vec<PortReq>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceReq {
    #[serde(default)]
    pub required: bool,
    /// Where the workspace appears in the box
    pub path: String,
    /// "ro" or "rw" [default "rw"]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
}

/// A guest port the agent serves. The host side is the consumer's choice.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortReq {
    #[serde(rename = "box")]
    pub guest: u16,
    #[serde(default)]
    pub purpose: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSpec {
    #[serde(default)]
    pub servers: Vec<McpServer>,
}

/// An MCP server: either a command run in the box, or an endpoint the consumer binds.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<McpEndpoint>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEndpoint {
    /// A key of `spec.bindings`
    #[serde(rename = "ref")]
    pub binding: String,
}

/// What the Capsule asks for. The consumer's policy decides what is granted; none of it is automatic.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Requests {
    #[serde(default)]
    pub network: NetworkReq,
    #[serde(default)]
    pub credentials: Vec<CredentialReq>,
    /// Reach the Windows host (172.17.0.0); off by default, as in `policy.rs`
    #[serde(default)]
    pub host_access: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkReq {
    #[serde(default)]
    pub allow: Vec<String>,
}

/// A credential the agent accepts: which service and how, never a value.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialReq {
    pub service: String,
    /// `apiKey` and/or `oauth`
    pub auth: Vec<String>,
    /// Where the proxy would put the key. Each domain is a capability: it receives the user's key.
    #[serde(default)]
    pub inject: Vec<InjectSpec>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestSpec {
    pub name: String,
    pub command: CommandSpec,
    /// "120s", "5m" [default 300s]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub name: String,
    pub prompt: String,
}

impl Capsule {
    pub fn parse(yaml: &str) -> Result<Capsule> {
        let capsule: Capsule = serde_yaml::from_str(yaml).context("parsing the Capsule manifest")?;
        capsule.validate()?;
        Ok(capsule)
    }

    /// Read `capsule.yaml` from a Capsule folder (or a path to the file itself).
    pub fn load(path: &Path) -> Result<Capsule> {
        let file = if path.is_dir() { path.join("capsule.yaml") } else { path.to_owned() };
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}; run this in a Capsule folder or pass its path", file.display()))?;
        Capsule::parse(&text).with_context(|| format!("in {}", file.display()))
    }

    /// `namespace/name@version`
    pub fn reference(&self) -> String {
        format!("{}@{}", self.metadata.name, self.metadata.version)
    }

    /// Every rule that doesn't need the filesystem or the network. Reports the first problem, naming the field.
    pub fn validate(&self) -> Result<()> {
        if self.api_version != API_VERSION {
            bail!("apiVersion is {:?}; this Elpis reads {API_VERSION}", self.api_version);
        }
        if self.kind != KIND {
            bail!("kind is {:?}; expected {KIND}", self.kind);
        }
        validate_capsule_name(&self.metadata.name)?;
        parse_version(&self.metadata.version).context("metadata.version")?;
        if let Some(from) = &self.metadata.derived_from {
            let (name, version) = from.split_once('@').with_context(|| format!("metadata.derivedFrom {from:?} should be namespace/name@version"))?;
            validate_capsule_name(name).context("metadata.derivedFrom")?;
            parse_version(version).context("metadata.derivedFrom")?;
        }
        if let Some(min) = self.spec.compatibility.elpis.as_deref() {
            parse_min_version(min).context("spec.compatibility.elpis")?;
        }
        self.validate_runtime()?;
        self.validate_payload()?;
        self.validate_resources_and_interfaces()?;
        self.validate_bindings_and_mcp()?;
        self.validate_requests()?;
        self.validate_tests()?;
        Ok(())
    }

    fn validate_runtime(&self) -> Result<()> {
        let rt = &self.spec.runtime;
        check_digest_pinned(&rt.image).context("spec.runtime.image")?;
        if rt.entrypoint.is_empty() || rt.entrypoint[0].is_empty() {
            bail!("spec.runtime.entrypoint is empty; give the command that starts the agent");
        }
        for (k, v) in &rt.env {
            if k.is_empty() || k.contains('=') {
                bail!("spec.runtime.env has an invalid name {k:?}");
            }
            if let Some(why) = looks_like_secret(k, v) {
                bail!("spec.runtime.env {k} {why}; Capsules never carry secrets. Declare it under spec.requests.credentials instead");
            }
        }
        let steps = rt.install.iter().chain(&rt.startup);
        for (i, step) in steps.enumerate() {
            let argv = step.to_command().argv;
            if argv.is_empty() || argv[0].is_empty() {
                bail!("spec.runtime install/startup step {} has an empty command", i + 1);
            }
            // scripts are one argument, so look at each word as well as the whole
            for arg in argv.iter().flat_map(|a| std::iter::once(a.as_str()).chain(a.split_whitespace())) {
                if let Some(why) = looks_like_secret("", arg) {
                    bail!("spec.runtime install/startup step {} {why}; Capsules never carry secrets", i + 1);
                }
            }
        }
        for arg in &rt.entrypoint {
            if let Some(why) = looks_like_secret("", arg) {
                bail!("spec.runtime.entrypoint {why}; Capsules never carry secrets");
            }
        }
        Ok(())
    }

    fn validate_payload(&self) -> Result<()> {
        let mut targets = BTreeSet::new();
        for p in &self.spec.payload {
            check_relative(&p.from).with_context(|| format!("spec.payload from {:?}", p.from))?;
            check_guest_path(&p.to).with_context(|| format!("spec.payload to {:?}", p.to))?;
            if p.to.trim_end_matches('/').is_empty() {
                bail!("spec.payload to {:?} would replace the whole root filesystem", p.to);
            }
            if !targets.insert(p.to.trim_end_matches('/').to_owned()) {
                bail!("spec.payload has two entries for {}", p.to);
            }
        }
        Ok(())
    }

    fn validate_resources_and_interfaces(&self) -> Result<()> {
        let mut paths = BTreeSet::new();
        for v in &self.spec.resources.volumes {
            check_guest_path(&v.path).with_context(|| format!("spec.resources.volumes path {:?}", v.path))?;
            if !paths.insert(v.path.clone()) {
                bail!("spec.resources.volumes lists {} twice", v.path);
            }
            if crate::workdir::parse_size(&v.size).with_context(|| format!("spec.resources.volumes {}", v.path))? == 0 {
                bail!("spec.resources.volumes {} has size 0", v.path);
            }
        }
        if let Some(ws) = &self.spec.interfaces.workspace {
            check_guest_path(&ws.path).context("spec.interfaces.workspace.path")?;
            if let Some(a) = ws.access.as_deref().filter(|a| !matches!(*a, "ro" | "rw")) {
                bail!("spec.interfaces.workspace.access is {a:?}; use \"ro\" or \"rw\"");
            }
        }
        let mut ports = BTreeSet::new();
        for p in &self.spec.interfaces.ports {
            if p.guest == 0 {
                bail!("spec.interfaces.ports: the box port can't be 0");
            }
            if !ports.insert(p.guest) {
                bail!("spec.interfaces.ports lists {} twice", p.guest);
            }
        }
        Ok(())
    }

    fn validate_bindings_and_mcp(&self) -> Result<()> {
        for (name, b) in &self.spec.bindings {
            check_ident(name).with_context(|| format!("spec.bindings {name:?}"))?;
            if !BINDING_TYPES.contains(&b.kind.as_str()) {
                bail!("spec.bindings.{name} has type {:?}; use one of {}", b.kind, BINDING_TYPES.join(", "));
            }
        }
        let mut names = BTreeSet::new();
        for s in &self.spec.mcp.servers {
            check_ident(&s.name).with_context(|| format!("spec.mcp.servers {:?}", s.name))?;
            if !names.insert(s.name.as_str()) {
                bail!("spec.mcp.servers lists {} twice", s.name);
            }
            match (&s.command, &s.endpoint) {
                (Some(c), None) if !c.is_empty() && !c[0].is_empty() => {}
                (None, Some(e)) => match self.spec.bindings.get(&e.binding) {
                    Some(b) if b.kind == "mcp-endpoint" => {}
                    Some(b) => bail!("spec.mcp.servers {} uses binding {} of type {:?}; it must be mcp-endpoint", s.name, e.binding, b.kind),
                    None => bail!("spec.mcp.servers {} refers to binding {:?}, which isn't declared under spec.bindings", s.name, e.binding),
                },
                _ => bail!("spec.mcp.servers {} needs exactly one of command (non-empty) or endpoint", s.name),
            }
        }
        Ok(())
    }

    fn validate_requests(&self) -> Result<()> {
        let allow = &self.spec.requests.network.allow;
        for h in allow {
            check_host_pattern(h).with_context(|| format!("spec.requests.network.allow {h:?}"))?;
        }
        let mut services = BTreeSet::new();
        for c in &self.spec.requests.credentials {
            check_ident(&c.service).with_context(|| format!("spec.requests.credentials service {:?}", c.service))?;
            if !services.insert(c.service.as_str()) {
                bail!("spec.requests.credentials lists {} twice", c.service);
            }
            if c.auth.is_empty() {
                bail!("spec.requests.credentials {} has no auth methods; use apiKey and/or oauth", c.service);
            }
            if let Some(a) = c.auth.iter().find(|a| !matches!(a.as_str(), "apiKey" | "oauth")) {
                bail!("spec.requests.credentials {} has auth {a:?}; use apiKey or oauth", c.service);
            }
            // a key is only ever sent to a host the Capsule also asks to reach
            for i in &c.inject {
                check_host_pattern(&i.domain).with_context(|| format!("spec.requests.credentials {} inject", c.service))?;
                if i.domain.starts_with("*.") {
                    bail!("spec.requests.credentials {} injects the key into {}; name exact hosts, a wildcard would send it to any subdomain", c.service, i.domain);
                }
                if !allow.iter().any(|p| host_matches(p, &i.domain)) {
                    bail!("spec.requests.credentials {} injects into {}, which spec.requests.network.allow doesn't include", c.service, i.domain);
                }
            }
        }
        Ok(())
    }

    fn validate_tests(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        for t in &self.spec.tests {
            check_ident(&t.name).with_context(|| format!("spec.tests {:?}", t.name))?;
            if !names.insert(t.name.as_str()) {
                bail!("spec.tests lists {} twice", t.name);
            }
            let empty = match &t.command {
                CommandSpec::Script(s) => s.trim().is_empty(),
                CommandSpec::Argv(a) => a.is_empty() || a[0].is_empty(),
            };
            if empty {
                bail!("spec.tests {} has an empty command", t.name);
            }
            if let Some(t_out) = &t.timeout {
                parse_duration_secs(t_out).with_context(|| format!("spec.tests {} timeout", t.name))?;
            }
        }
        let mut names = BTreeSet::new();
        for e in &self.spec.examples {
            check_ident(&e.name).with_context(|| format!("spec.examples {:?}", e.name))?;
            if !names.insert(e.name.as_str()) {
                bail!("spec.examples lists {} twice", e.name);
            }
        }
        Ok(())
    }

    /// Whether this Elpis satisfies `spec.compatibility.elpis` (true when none is given).
    pub fn compatible_with(&self, elpis_version: &str) -> Result<bool> {
        let Some(min) = self.spec.compatibility.elpis.as_deref() else { return Ok(true) };
        Ok(parse_version_lenient(elpis_version)? >= parse_min_version(min)?)
    }

    /// A plain-text description for `capsule inspect`: what it runs, then what it asks for. The requests come
    /// last and are worded as requests, since the consumer's policy decides what is granted.
    pub fn summary(&self, digest: Option<&str>) -> String {
        let (rt, spec) = (&self.spec.runtime, &self.spec);
        let mut s = format!("{}\n", self.reference());
        let mut line = |label: &str, text: String| s.push_str(&format!("  {label:<11}{text}\n"));
        if !self.metadata.description.is_empty() {
            line("about", self.metadata.description.clone());
        }
        if let Some(d) = digest {
            line("digest", d.to_owned());
        }
        if let Some(from) = &self.metadata.derived_from {
            line("forked from", from.clone());
        }
        if let Some(g) = &self.metadata.source {
            line("source", format!("{} @ {}", g.git, g.commit));
        }
        if let Some(min) = &spec.compatibility.elpis {
            line("needs elpis", min.clone());
        }
        line("image", rt.image.clone());
        line("runs", format!("{}{}", rt.entrypoint.join(" "), rt.user.as_ref().map_or(String::new(), |u| format!("  (as {u})"))));
        if !rt.env.is_empty() {
            line("env", rt.env.keys().cloned().collect::<Vec<_>>().join(", "));
        }
        if !rt.install.is_empty() || !rt.startup.is_empty() {
            line("setup", format!("{} install step(s), {} startup step(s)", rt.install.len(), rt.startup.len()));
        }
        for p in &spec.payload {
            line("payload", format!("{} → {}", p.from, p.to));
        }
        if let Some(w) = &spec.interfaces.workspace {
            line("workspace", format!("{} at {} ({})", if w.required { "required" } else { "optional" }, w.path, w.access.as_deref().unwrap_or("rw")));
        }
        for p in &spec.interfaces.ports {
            line("port", format!("{}{}", p.guest, if p.purpose.is_empty() { String::new() } else { format!("  {}", p.purpose) }));
        }
        for v in &spec.resources.volumes {
            line("volume", format!("{} ({})", v.path, v.size));
        }
        if spec.resources.docker {
            line("docker", "starts a Docker engine in the box".to_owned());
        }
        for (name, b) in &spec.bindings {
            line("input", format!("{name} ({}{})", b.kind, if b.required { ", required" } else { "" }));
        }
        for m in &spec.mcp.servers {
            let how = match (&m.command, &m.endpoint) {
                (Some(c), _) => format!("runs {}", c.join(" ")),
                (_, Some(e)) => format!("input {}", e.binding),
                _ => String::new(),
            };
            line("mcp", format!("{}  {how}", m.name));
        }
        for t in &spec.tests {
            line("test", t.name.clone());
        }
        for e in &spec.examples {
            line("example", e.name.clone());
        }

        let req = &spec.requests;
        s.push_str("  asks for (you decide what is granted):\n");
        let mut ask = |label: &str, text: String| s.push_str(&format!("    {label:<11}{text}\n"));
        ask("network", if req.network.allow.is_empty() { "none".to_owned() } else { req.network.allow.join(", ") });
        for c in &req.credentials {
            let hosts: Vec<&str> = c.inject.iter().map(|i| i.domain.as_str()).collect();
            ask("credential", format!("{} ({}){}", c.service, c.auth.join(", "), if hosts.is_empty() { String::new() } else { format!(" sent to {}", hosts.join(", ")) }));
        }
        ask("host", if req.host_access { "access to the Windows host".to_owned() } else { "no access".to_owned() });
        s
    }

    /// Environment the agent runs with (the engine merges it over the image's, as it does for kits).
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.spec.runtime.env
    }

    /// The `BoxFile` for a new instance. Carries only what `BoxFile` holds: the payload layer, the environment
    /// and the granted network rules are applied by the caller. `host_ports` are the consumer's choices, and each
    /// must name a guest port the Capsule declares.
    pub fn to_boxfile(&self, host_ports: &[PortMap]) -> Result<BoxFile> {
        let declared: BTreeSet<u16> = self.spec.interfaces.ports.iter().map(|p| p.guest).collect();
        for p in host_ports {
            if !declared.contains(&p.guest) {
                bail!("{} doesn't declare box port {}; it declares {}", self.reference(), p.guest, list_or_none(&declared));
            }
        }
        let rt = &self.spec.runtime;
        let steps = |v: &[StepSpec]| -> Vec<SetupCommand> { v.iter().map(StepSpec::to_command).collect() };
        let mut volumes = Vec::new();
        for v in &self.spec.resources.volumes {
            volumes.push(Volume { path: v.path.clone(), size: crate::workdir::parse_size(&v.size)? });
        }
        Ok(BoxFile {
            source: format!("capsule {}", self.reference()),
            image: Some(rt.image.clone()),
            entrypoint: rt.entrypoint.clone(),
            workspace: self.spec.interfaces.workspace.as_ref().map(|w| w.path.clone()),
            user_name: rt.user.clone(),
            install: steps(&rt.install),
            startup: steps(&rt.startup),
            docker: self.spec.resources.docker,
            volumes,
            ports: host_ports.to_vec(),
            auto_stop: self.spec.resources.auto_stop,
            ..BoxFile::default()
        })
    }
}

/// What the consumer allowed. Today that is network hosts and access to the Windows host; credential requests
/// aren't injected yet (the proxy's spec is richer than a Capsule's), so they are never granted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Grants {
    pub network: Vec<String>,
    pub host_access: bool,
}

/// `capsule.lock` in a box folder: which Capsule made the box, what it asked for, and what was granted.
/// A later version's requests are compared with `requests` to decide what needs approving again.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Lock {
    pub capsule: String,
    pub digest: String,
    pub requests: Requests,
    pub granted: Grants,
    /// Credential services the Capsule asked for that were not injected
    pub credentials_ignored: Vec<String>,
}

impl Lock {
    pub fn path_for(work: &Path) -> std::path::PathBuf {
        work.join("capsule.lock")
    }

    pub fn load(work: &Path) -> Option<Lock> {
        serde_json::from_slice(&std::fs::read(Lock::path_for(work)).ok()?).ok()
    }

    pub fn save(&self, work: &Path) -> Result<()> {
        std::fs::write(Lock::path_for(work), serde_json::to_vec_pretty(self)?).context("writing capsule.lock")
    }
}

impl Requests {
    /// Everything this asks for that can be granted today (what `--yes` accepts).
    pub fn grantable(&self) -> Grants {
        Grants { network: self.network.allow.clone(), host_access: self.host_access }
    }

    /// What `self` asks for that `previous` didn't: the lines a consent prompt shows when a new version
    /// widens the Capsule's reach. Empty means nothing new needs approval.
    pub fn added_since(&self, previous: &Requests) -> Vec<String> {
        let mut out = Vec::new();
        for h in &self.network.allow {
            if !previous.network.allow.contains(h) {
                out.push(format!("network: {h}"));
            }
        }
        if self.host_access && !previous.host_access {
            out.push("access to the Windows host".to_owned());
        }
        for c in &self.credentials {
            let old = previous.credentials.iter().find(|o| o.service == c.service);
            match old {
                None => out.push(format!("credential: {} ({})", c.service, c.auth.join(", "))),
                Some(o) => {
                    for a in c.auth.iter().filter(|a| !o.auth.contains(a)) {
                        out.push(format!("credential: {} via {a}", c.service));
                    }
                }
            }
            // each domain that would receive the key is a separate approval
            for i in &c.inject {
                if !old.is_some_and(|o| o.inject.iter().any(|oi| oi.domain == i.domain)) {
                    out.push(format!("{} key sent to {}", c.service, i.domain));
                }
            }
        }
        out
    }
}

fn list_or_none(ports: &BTreeSet<u16>) -> String {
    if ports.is_empty() { "none".to_owned() } else { ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ") }
}

/// `namespace/name`: lowercase letters, digits and hyphens in each part (max 40).
pub fn validate_capsule_name(name: &str) -> Result<()> {
    let ok_part = |p: &str| {
        !p.is_empty()
            && p.len() <= 40
            && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !p.starts_with('-')
            && !p.ends_with('-')
    };
    match name.split_once('/') {
        Some((ns, n)) if ok_part(ns) && ok_part(n) => Ok(()),
        _ => bail!("metadata.name {name:?} should be namespace/name, each of lowercase letters, digits and hyphens (e.g. myteam/code-reviewer)"),
    }
}

/// `MAJOR.MINOR.PATCH`, numbers only. Pre-release tags aren't supported yet.
pub fn parse_version(v: &str) -> Result<(u64, u64, u64)> {
    let parts: Vec<&str> = v.split('.').collect();
    let num = |s: &str| s.parse::<u64>().ok().filter(|_| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));
    match parts.as_slice() {
        [a, b, c] => match (num(a), num(b), num(c)) {
            (Some(a), Some(b), Some(c)) => Ok((a, b, c)),
            _ => bail!("{v:?} isn't a version; use MAJOR.MINOR.PATCH, e.g. 1.2.0"),
        },
        _ => bail!("{v:?} isn't a version; use MAJOR.MINOR.PATCH, e.g. 1.2.0"),
    }
}

/// Like [`parse_version`] but takes `MAJOR.MINOR` too and ignores a `-pre`/`+build` suffix (for Elpis's own version).
fn parse_version_lenient(v: &str) -> Result<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.');
    let mut next = || -> Result<u64> { it.next().map_or(Ok(0), |s| s.parse().with_context(|| format!("{v:?} isn't a version"))) };
    Ok((next()?, next()?, next()?))
}

fn parse_min_version(req: &str) -> Result<(u64, u64, u64)> {
    let v = req.trim().strip_prefix(">=").with_context(|| format!("{req:?} should be a minimum version like \">=0.4\""))?;
    parse_version_lenient(v.trim())
}

/// "120s", "5m", "1h"; a bare number is seconds.
fn parse_duration_secs(text: &str) -> Result<u64> {
    let t = text.trim();
    let (n, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len()));
    let n: u64 = n.parse().with_context(|| format!("{text:?} isn't a duration; use 120s or 5m"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => bail!("{text:?} isn't a duration; use 120s or 5m"),
    };
    if secs == 0 {
        bail!("{text:?} is zero");
    }
    Ok(secs)
}

/// `name@sha256:<64 hex>`. A tag can be repointed after a Capsule is released, so it isn't accepted.
pub fn check_digest_pinned(image: &str) -> Result<()> {
    match image.rsplit_once("@sha256:") {
        Some((name, d)) if !name.is_empty() && d.len() == 64 && d.chars().all(|c| c.is_ascii_hexdigit()) => Ok(()),
        _ => bail!("{image:?} isn't pinned by digest; use name@sha256:<64 hex digits> (e.g. from `docker buildx imagetools inspect`) so a release can't change under its users"),
    }
}

fn check_ident(s: &str) -> Result<()> {
    let ok = !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && s.starts_with(|c: char| c.is_ascii_alphanumeric());
    if !ok {
        bail!("{s:?} should be letters, digits, '-' or '_', starting with a letter or digit (max 40)");
    }
    Ok(())
}

/// A path inside the Capsule folder: relative, no `..`.
fn check_relative(p: &str) -> Result<()> {
    if p.is_empty() {
        bail!("is empty");
    }
    if p.starts_with('/') || p.starts_with('\\') || p.contains(':') {
        bail!("must be relative to the Capsule folder");
    }
    if p.split(['/', '\\']).any(|c| c == "..") {
        bail!("must stay inside the Capsule folder (no `..`)");
    }
    Ok(())
}

/// An absolute path in the box, normalised: no `..`, no empty or `.` components, no NUL.
fn check_guest_path(p: &str) -> Result<()> {
    if !p.starts_with('/') {
        bail!("must be an absolute path in the box");
    }
    if p.contains('\0') || p.contains('\\') {
        bail!("has invalid characters");
    }
    if p.split('/').skip(1).any(|c| c == ".." || c == ".") {
        bail!("must not contain `.` or `..`");
    }
    Ok(())
}

/// A host or `*.suffix`. A bare `*` would allow everything, so it's rejected.
fn check_host_pattern(h: &str) -> Result<()> {
    if h.is_empty() || h.contains("://") || h.contains('/') || h.contains(char::is_whitespace) || h.contains(':') {
        bail!("should be a host name like api.example.com (no scheme, port or path)");
    }
    let name = h.strip_prefix("*.").unwrap_or(h);
    if name.contains('*') || !name.contains('.') && h.starts_with("*.") {
        bail!("only a leading `*.` is allowed, and it needs a domain after it");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') || name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        bail!("should be a host name like api.example.com");
    }
    Ok(())
}

/// Whether the allow pattern covers `host` (exact, or `*.suffix` for any subdomain).
fn host_matches(pattern: &str, host: &str) -> bool {
    let (p, h) = (pattern.to_ascii_lowercase(), host.to_ascii_lowercase());
    match p.strip_prefix("*.") {
        Some(suffix) => h.len() > suffix.len() + 1 && h.ends_with(suffix) && h[..h.len() - suffix.len()].ends_with('.'),
        None => p == h,
    }
}

/// A coarse check for credentials committed by mistake. `name` is the variable name when the text is an
/// environment value. Not a guarantee: it catches the common shapes so "credentials stay external" isn't
/// only a convention. Returns what looks wrong.
pub fn looks_like_secret(name: &str, value: &str) -> Option<&'static str> {
    const PREFIXES: &[&str] = &["sk-", "ghp_", "gho_", "ghs_", "github_pat_", "xoxb-", "xoxp-", "AKIA", "AIza", "glpat-"];
    let v = value.trim();
    if v.contains("-----BEGIN") && v.contains("PRIVATE KEY") {
        return Some("contains a private key");
    }
    if PREFIXES.iter().any(|p| v.starts_with(p) && v.len() >= p.len() + 12 && !v.contains(char::is_whitespace)) {
        return Some("looks like an API key or token");
    }
    let upper = name.to_ascii_uppercase();
    let sensitive = ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"].iter().any(|w| upper.contains(w));
    // proxy-managed is the sentinel the credential proxy uses; an empty value is a placeholder
    if sensitive && !v.is_empty() && v != "proxy-managed" {
        return Some("is named like a secret and has a value");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "docker.io/library/node@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn manifest() -> String {
        format!(
            r#"
apiVersion: elpis.dev/v1alpha1
kind: Capsule
metadata:
  name: myteam/code-reviewer
  version: 1.2.0
  description: Reviews a diff
  derivedFrom: myteam/base-reviewer@1.0.0
spec:
  compatibility:
    elpis: ">=0.4"
  runtime:
    image: {DIGEST}
    user: agent
    entrypoint: ["node", "/opt/agent/main.js"]
    env: {{ LOG_LEVEL: info, API_KEY: proxy-managed }}
    install:
      - {{ command: "npm ci --omit=dev", user: agent }}
    startup:
      - {{ command: ["/opt/agent/warmup.sh"] }}
  payload:
    - {{ from: ./agent, to: /opt/agent }}
  resources:
    volumes: [{{ path: /var/cache/agent, size: 2g }}]
    docker: true
    autoStop: 60
  interfaces:
    workspace: {{ required: true, path: /work, access: rw }}
    ports: [{{ box: 3000, purpose: "review UI" }}]
  mcp:
    servers:
      - {{ name: lint, command: ["/opt/agent/mcp-lint"] }}
      - {{ name: tickets, endpoint: {{ ref: tickets }} }}
  requests:
    network:
      allow: ["api.anthropic.com", "*.npmjs.org"]
    credentials:
      - service: anthropic
        auth: [apiKey, oauth]
        inject: [{{ domain: api.anthropic.com, header: x-api-key, format: "%s" }}]
  bindings:
    tickets: {{ type: mcp-endpoint }}
  tests:
    - {{ name: smoke, command: ["/opt/agent/selftest.sh"], timeout: 120s }}
  examples:
    - {{ name: review-last-commit, prompt: "Review HEAD~1" }}
"#
        )
    }

    fn err(yaml: &str) -> String {
        format!("{:#}", Capsule::parse(yaml).unwrap_err())
    }

    #[test]
    fn parses_the_documented_example() {
        let c = Capsule::parse(&manifest()).unwrap();
        assert_eq!(c.reference(), "myteam/code-reviewer@1.2.0");
        assert_eq!(c.spec.runtime.install.len(), 1);
        assert_eq!(c.spec.interfaces.ports[0].guest, 3000);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let bad = manifest().replace("hostAccess", "hostAcess").replace("  requests:\n", "  requests:\n    hostAcess: true\n");
        assert!(err(&bad).contains("hostAcess"), "{}", err(&bad));
    }

    #[test]
    fn wrong_api_version_and_kind() {
        assert!(err(&manifest().replace("v1alpha1", "v2")).contains("apiVersion"));
        assert!(err(&manifest().replace("kind: Capsule", "kind: Kit")).contains("kind"));
    }

    #[test]
    fn names_and_versions() {
        assert!(err(&manifest().replace("myteam/code-reviewer", "code-reviewer")).contains("namespace/name"));
        assert!(err(&manifest().replace("myteam/code-reviewer", "My Team/x")).contains("namespace/name"));
        assert!(err(&manifest().replace("version: 1.2.0", "version: 1.2")).contains("MAJOR.MINOR.PATCH"));
        assert!(err(&manifest().replace("base-reviewer@1.0.0", "base-reviewer")).contains("derivedFrom"));
    }

    #[test]
    fn image_must_be_pinned_by_digest() {
        assert!(err(&manifest().replace(DIGEST, "docker.io/library/node:22")).contains("pinned by digest"));
        assert!(err(&manifest().replace(DIGEST, "docker.io/library/node@sha256:abc")).contains("pinned by digest"));
        assert!(check_digest_pinned("@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").is_err());
    }

    #[test]
    fn secrets_in_env_and_commands_are_refused() {
        assert!(err(&manifest().replace("API_KEY: proxy-managed", "API_KEY: hunter2hunter2")).contains("API_KEY"));
        assert!(err(&manifest().replace("LOG_LEVEL: info", "NOTE: sk-abcdefghijklmnop")).contains("API key"));
        assert!(err(&manifest().replace("npm ci --omit=dev", "curl -H sk-abcdefghijklmnopqrstuv x")).contains("secrets"));
        assert!(looks_like_secret("PASSWORD", "").is_none());
        assert!(looks_like_secret("LOG_LEVEL", "debug").is_none());
        assert!(looks_like_secret("X", "-----BEGIN RSA PRIVATE KEY-----").is_some());
    }

    #[test]
    fn payload_paths_cannot_escape() {
        assert!(err(&manifest().replace("from: ./agent", "from: ../agent")).contains("no `..`"));
        assert!(err(&manifest().replace("from: ./agent", "from: C:/agent")).contains("relative"));
        assert!(err(&manifest().replace("to: /opt/agent", "to: opt/agent")).contains("absolute"));
        assert!(err(&manifest().replace("to: /opt/agent", "to: /opt/../etc")).contains(".."));
        assert!(err(&manifest().replace("to: /opt/agent", "to: /")).contains("root"));
    }

    #[test]
    fn volumes_and_ports() {
        assert!(err(&manifest().replace("size: 2g", "size: lots")).contains("size"));
        assert!(err(&manifest().replace("size: 2g", "size: 0")).contains("size 0"));
        assert!(err(&manifest().replace("box: 3000", "box: 0")).contains("can't be 0"));
        assert!(err(&manifest().replace("access: rw", "access: rwx")).contains("access"));
    }

    #[test]
    fn mcp_servers_need_one_source_and_a_declared_binding() {
        assert!(err(&manifest().replace("ref: tickets", "ref: nope")).contains("isn't declared"));
        assert!(err(&manifest().replace("tickets: { type: mcp-endpoint }", "tickets: { type: string }")).contains("mcp-endpoint"));
        assert!(err(&manifest().replace("command: [\"/opt/agent/mcp-lint\"]", "command: []")).contains("exactly one"));
        assert!(err(&manifest().replace("tickets: { type: mcp-endpoint }", "tickets: { type: blob }")).contains("type"));
    }

    #[test]
    fn network_patterns() {
        assert!(err(&manifest().replace("\"*.npmjs.org\"", "\"*\"")).contains("leading"));
        assert!(err(&manifest().replace("\"*.npmjs.org\"", "\"https://x.com\"")).contains("host name"));
        assert!(err(&manifest().replace("\"*.npmjs.org\"", "\"a.*.com\"")).contains("leading"));
        assert!(host_matches("*.npmjs.org", "registry.npmjs.org"));
        assert!(!host_matches("*.npmjs.org", "npmjs.org"));
        assert!(!host_matches("*.npmjs.org", "evilnpmjs.org"));
        assert!(host_matches("API.example.com", "api.EXAMPLE.com"));
    }

    #[test]
    fn credentials_only_inject_into_requested_hosts() {
        assert!(err(&manifest().replace("domain: api.anthropic.com", "domain: evil.example.com")).contains("doesn't include"));
        assert!(err(&manifest().replace("domain: api.anthropic.com", "domain: \"*.anthropic.com\"")).contains("exact hosts"));
        assert!(err(&manifest().replace("auth: [apiKey, oauth]", "auth: []")).contains("no auth"));
        assert!(err(&manifest().replace("auth: [apiKey, oauth]", "auth: [password]")).contains("password"));
    }

    #[test]
    fn tests_and_examples() {
        assert!(err(&manifest().replace("timeout: 120s", "timeout: soon")).contains("duration"));
        assert!(err(&manifest().replace("name: smoke", "name: \"\"")).contains("spec.tests"));
        assert_eq!(parse_duration_secs("5m").unwrap(), 300);
        assert_eq!(parse_duration_secs("90").unwrap(), 90);
        assert!(parse_duration_secs("0s").is_err());
    }

    #[test]
    fn compatibility() {
        let c = Capsule::parse(&manifest()).unwrap();
        assert!(c.compatible_with("0.4.0").unwrap());
        assert!(c.compatible_with("0.10.2").unwrap());
        assert!(c.compatible_with("1.0.0").unwrap());
        assert!(!c.compatible_with("0.3.9").unwrap());
        assert!(err(&manifest().replace(">=0.4", "0.4")).contains("minimum version"));
    }

    #[test]
    fn converts_to_a_boxfile() {
        let c = Capsule::parse(&manifest()).unwrap();
        let port = PortMap::parse("8080:3000").unwrap();
        let b = c.to_boxfile(&[port.clone()]).unwrap();
        assert_eq!(b.source, "capsule myteam/code-reviewer@1.2.0");
        assert_eq!(b.image.as_deref(), Some(DIGEST));
        assert_eq!(b.workspace.as_deref(), Some("/work"));
        assert_eq!(b.user_name.as_deref(), Some("agent"));
        assert_eq!(b.install[0].argv, ["sh", "-c", "npm ci --omit=dev"]);
        assert_eq!(b.startup[0].argv, ["/opt/agent/warmup.sh"]);
        assert_eq!(b.volumes[0].size, 2 << 30);
        assert!(b.docker);
        assert_eq!(b.auto_stop, Some(60));
        assert_eq!(b.ports, vec![port]);
        assert!(!b.installed);
        assert_eq!(c.env()["LOG_LEVEL"], "info");
    }

    #[test]
    fn host_ports_must_be_declared() {
        let c = Capsule::parse(&manifest()).unwrap();
        let e = c.to_boxfile(&[PortMap::parse("8080:9999").unwrap()]).unwrap_err().to_string();
        assert!(e.contains("9999") && e.contains("3000"), "{e}");
    }

    #[test]
    fn added_requests_are_what_a_new_version_widens() {
        let old = Capsule::parse(&manifest()).unwrap().spec.requests;
        assert!(old.added_since(&old).is_empty());
        let mut new = old.clone();
        new.network.allow.push("evil.example.com".into());
        new.host_access = true;
        new.credentials[0].inject.push(InjectSpec { domain: "evil.example.com".into(), header: "x".into(), format: "%s".into() });
        new.credentials.push(CredentialReq { service: "openai".into(), auth: vec!["apiKey".into()], inject: vec![] });
        let added = new.added_since(&old);
        assert_eq!(added.len(), 4, "{added:?}");
        assert!(added.iter().any(|a| a.contains("network: evil.example.com")));
        assert!(added.iter().any(|a| a.contains("Windows host")));
        assert!(added.iter().any(|a| a.contains("anthropic key sent to evil.example.com")));
        assert!(added.iter().any(|a| a.contains("credential: openai")));
        // dropping requests never needs approval
        assert!(old.added_since(&new).is_empty());
    }

    #[test]
    fn summary_lists_what_it_runs_and_what_it_asks_for() {
        let s = Capsule::parse(&manifest()).unwrap().summary(Some("sha256:abc"));
        for want in [
            "myteam/code-reviewer@1.2.0",
            "sha256:abc",
            "node /opt/agent/main.js  (as agent)",
            "required at /work (rw)",
            "asks for (you decide what is granted)",
            "api.anthropic.com, *.npmjs.org",
            "anthropic (apiKey, oauth) sent to api.anthropic.com",
            "no access",
            "tickets (mcp-endpoint)",
        ] {
            assert!(s.contains(want), "missing {want:?} in:\n{s}");
        }
        assert!(!s.contains("hunter2"));
    }

    #[test]
    fn grantable_is_the_network_and_host_access_only() {
        let mut req = Capsule::parse(&manifest()).unwrap().spec.requests;
        req.host_access = true;
        let g = req.grantable();
        assert_eq!(g.network, ["api.anthropic.com", "*.npmjs.org"]);
        assert!(g.host_access);
    }

    #[test]
    fn the_lock_round_trips_and_rejects_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("elpis-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let requests = Capsule::parse(&manifest()).unwrap().spec.requests;
        let lock = Lock { capsule: "a/b@1.0.0".into(), digest: "sha256:x".into(), granted: requests.grantable(), requests, credentials_ignored: vec!["anthropic".into()] };
        lock.save(&dir).unwrap();
        let back = Lock::load(&dir).unwrap();
        assert_eq!((back.capsule, back.granted, back.credentials_ignored), (lock.capsule, lock.granted, lock.credentials_ignored));
        std::fs::write(Lock::path_for(&dir), r#"{"capsule":"a/b@1.0.0","bogus":1}"#).unwrap();
        assert!(Lock::load(&dir).is_none());
    }

    #[test]
    fn minimal_manifest() {
        let y = format!("apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: a/b, version: 0.1.0 }}\nspec:\n  runtime: {{ image: {DIGEST}, entrypoint: [run] }}\n");
        let c = Capsule::parse(&y).unwrap();
        assert!(!c.spec.requests.host_access);
        assert!(c.spec.requests.network.allow.is_empty());
        assert!(c.compatible_with("0.0.1").unwrap());
    }
}
