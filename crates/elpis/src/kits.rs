//! sbx kits: the agent definitions `sbx run <agent>` uses, published on Docker Hub under `docker.io/sbx`.
//!
//! A kit is an OCI artifact (`application/vnd.docker.sandbox.kit.v2`) whose config blob is a YAML spec:
//! the image to run (`sandbox.image`, e.g. `docker.io/sbx/opencode-image:latest`), the agent's command
//! (`entrypoint`), environment variables, the domains it needs (`permissions.network.allow`), files to
//! write, and commands to run once (`setup.install`) or on every boot (`setup.startup`). `kind: sandbox`
//! kits are agents; `kind: mixin` kits add tools to one (`--with neovim`).
//!
//! Not carried over: sbx's credential injection (its proxy adds API keys to matching HTTPS requests; here
//! the agent signs in or reads keys from the environment itself) and MCP gateways.

use crate::boxfile::{SetupCommand, SetupFile};
use crate::registry::{Reference, Registry};
use crate::session::debug_root;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const HUB_NAMESPACE: &str = "sbx";

/// A kit as listed on Docker Hub.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KitSummary {
    /// The short name `sbx run` takes ("opencode")
    pub name: String,
    pub repository: String,
    pub description: String,
    pub updated: String,
    /// Has a matching `<name>-image` repository: an agent, not a mixin
    pub agent: bool,
}

/// The parts of a kit spec elpis uses.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Kit {
    pub kind: String,
    pub name: String,
    pub version: String,
    pub display_name: String,
    pub description: String,
    pub sandbox: SandboxSpec,
    pub environment: EnvironmentSpec,
    pub permissions: PermissionsSpec,
    pub setup: SetupSpec,
    pub credentials: Vec<CredentialSpec>,
    pub volumes: Vec<VolumeSpec>,
}

/// A sized volume: its own disk mounted at `path` (sbx gives these block volumes too).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VolumeSpec {
    pub path: String,
    /// "2g", "512m"
    pub size: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxSpec {
    pub image: String,
    pub entrypoint: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EnvironmentSpec {
    pub variables: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsSpec {
    pub network: NetworkSpec,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSpec {
    pub allow: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupSpec {
    pub install: Vec<StepSpec>,
    pub startup: Vec<StepSpec>,
    pub files: Vec<FileSpec>,
}

/// A command is a shell script (string) or an argv list.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandSpec {
    Script(String),
    Argv(Vec<String>),
}

impl Default for CommandSpec {
    fn default() -> Self {
        CommandSpec::Argv(Vec::new())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StepSpec {
    pub command: CommandSpec,
    pub user: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileSpec {
    pub path: String,
    pub content: String,
    pub mode: Option<String>,
    pub user: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CredentialSpec {
    pub service: String,
    pub api_key: Option<ApiKeySpec>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApiKeySpec {
    /// The environment variable the agent reads the key from ("" when it's handled proxy-side only)
    pub name: String,
    /// Set `name` to the sentinel `proxy-managed` in the box; the proxy puts the real key in requests
    pub proxy_managed: bool,
    /// Where the proxy puts the key: a header on requests to each domain
    pub inject: Vec<InjectSpec>,
}

/// One injection rule: on HTTPS requests to `domain`, set `header` to `format` with `%s` replaced by the key.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InjectSpec {
    pub domain: String,
    pub header: String,
    pub format: String,
}

impl StepSpec {
    pub fn to_command(&self) -> SetupCommand {
        let argv = match &self.command {
            CommandSpec::Script(s) => vec!["sh".into(), "-c".into(), s.clone()],
            CommandSpec::Argv(a) => a.clone(),
        };
        SetupCommand { argv, user: self.user.clone(), description: self.description.clone() }
    }
}

impl Kit {
    pub fn is_agent(&self) -> bool {
        self.kind == "sandbox"
    }

    pub fn install_steps(&self) -> Vec<SetupCommand> {
        self.setup.install.iter().map(StepSpec::to_command).collect()
    }

    pub fn startup_steps(&self) -> Vec<SetupCommand> {
        self.setup.startup.iter().map(StepSpec::to_command).collect()
    }

    pub fn files(&self) -> Vec<SetupFile> {
        self.setup
            .files
            .iter()
            .map(|f| SetupFile { path: f.path.clone(), content: f.content.clone(), mode: f.mode.clone(), user: f.user.clone() })
            .collect()
    }

    /// Environment variables the agent reads its API keys from (sbx's proxy fills these in; here you do).
    pub fn key_variables(&self) -> Vec<String> {
        self.credentials.iter().filter_map(|c| c.api_key.as_ref()).map(|k| k.name.clone()).filter(|n| !n.is_empty()).collect()
    }

    /// Fold a mixin into this kit.
    pub fn add_mixin(&mut self, mixin: &Kit) {
        self.environment.variables.extend(mixin.environment.variables.clone());
        self.permissions.network.allow.extend(mixin.permissions.network.allow.clone());
        self.setup.install.extend(mixin.setup.install.clone());
        self.setup.startup.extend(mixin.setup.startup.clone());
        self.setup.files.extend(mixin.setup.files.clone());
        self.volumes.extend(mixin.volumes.clone());
    }
}

fn kits_dir() -> PathBuf {
    debug_root().join("kits")
}

/// "opencode" → docker.io/sbx/opencode-kit:latest; anything with a '/' is a full reference.
pub fn reference(name: &str) -> Result<Reference> {
    if name.contains('/') {
        Reference::parse(name)
    } else {
        let (n, tag) = name.split_once(':').unwrap_or((name, "latest"));
        let n = n.strip_suffix("-kit").unwrap_or(n);
        Reference::parse(&format!("docker.io/{HUB_NAMESPACE}/{n}-kit:{tag}"))
    }
}

/// Fetch a kit's spec from the registry, and keep a copy beside the images.
pub fn fetch(name: &str) -> Result<Kit> {
    let reference = reference(name)?;
    let registry = Registry::new(reference.clone());
    let (manifest, _) = registry.manifest().with_context(|| format!("no kit {reference}"))?;
    let artifact = manifest["artifactType"].as_str().or(manifest["config"]["mediaType"].as_str()).unwrap_or_default();
    if !artifact.contains("sandbox.kit") {
        bail!("{reference} isn't a sandbox kit ({artifact})");
    }
    let yaml = registry.blob_bytes(manifest["config"]["digest"].as_str().context("kit without a spec")?)?;
    let kit: Kit = serde_yaml::from_slice(&yaml).with_context(|| format!("parsing {reference}'s spec"))?;
    std::fs::create_dir_all(kits_dir())?;
    std::fs::write(kits_dir().join(format!("{}.yaml", kit.name)), &yaml)?;
    Ok(kit)
}

/// A kit fetched before, from the local copy.
pub fn cached(name: &str) -> Option<Kit> {
    serde_yaml::from_slice(&std::fs::read(kits_dir().join(format!("{name}.yaml"))).ok()?).ok()
}

/// Every kit in the sbx namespace on Docker Hub.
///
/// Anonymous Hub listings stop at an offset of 100 ("sign in to page further"), so this asks for the `-kit`
/// and `-image` repositories by name, each sorted both ways, which covers up to 200 of each.
pub fn catalog() -> Result<Vec<KitSummary>> {
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30)).build();
    let mut repos: Vec<(String, String, String)> = Vec::new();
    for filter in ["-kit", "-image"] {
        for ordering in ["name", "-name"] {
            let url = format!("https://hub.docker.com/v2/namespaces/{HUB_NAMESPACE}/repositories?page_size=100&name={filter}&ordering={ordering}");
            let page: serde_json::Value = serde_json::from_reader(agent.get(&url).call().context("listing Docker Hub")?.into_reader())?;
            let results = page["results"].as_array().cloned().unwrap_or_default();
            for r in &results {
                let name = r["name"].as_str().unwrap_or_default().to_owned();
                if !repos.iter().any(|(n, ..)| *n == name) {
                    repos.push((
                        name,
                        r["description"].as_str().unwrap_or_default().to_owned(),
                        r["last_updated"].as_str().unwrap_or_default().chars().take(10).collect(),
                    ));
                }
            }
            // One page held them all
            if page["count"].as_u64().unwrap_or(0) <= results.len() as u64 {
                break;
            }
        }
    }
    let images: std::collections::HashSet<String> =
        repos.iter().filter_map(|(n, ..)| n.strip_suffix("-image").map(str::to_owned)).collect();
    let mut kits: Vec<KitSummary> = repos
        .iter()
        .filter_map(|(n, d, u)| {
            let name = n.strip_suffix("-kit")?;
            let description = match d.trim() {
                "|" | ">-" | ">" => String::new(),
                d => d.to_owned(),
            };
            Some(KitSummary {
                name: name.to_owned(),
                repository: format!("{HUB_NAMESPACE}/{n}"),
                description,
                updated: u.clone(),
                agent: images.contains(name),
            })
        })
        .collect();
    kits.sort_by(|a, b| b.agent.cmp(&a.agent).then(a.name.cmp(&b.name)));
    std::fs::create_dir_all(kits_dir())?;
    std::fs::write(kits_dir().join("catalog.json"), serde_json::to_string_pretty(&kits)?)?;
    Ok(kits)
}

/// The catalog from the last time it was fetched.
pub fn cached_catalog() -> Vec<KitSummary> {
    std::fs::read_to_string(kits_dir().join("catalog.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Everything `sbx run <kit>` needs before creating a box: the kit (with any mixins folded in) and its image,
/// pulled now (cached layers are reused). Offline, a kit and image fetched before are used as they are.
pub fn prepare(name: &str, mixins: &[String], progress: &dyn Fn(&str)) -> Result<(Kit, crate::registry::Image)> {
    progress(&format!("fetching kit {name}"));
    let mut kit = match fetch(name) {
        Ok(k) => k,
        Err(e) => cached(name.split(':').next().unwrap_or(name)).ok_or(e)?,
    };
    if !kit.is_agent() {
        bail!("{} is a {} kit, not an agent; add it to one with --with {}", kit.name, kit.kind, kit.name);
    }
    for m in mixins {
        let mixin = fetch(m)?;
        progress(&format!("adding {} ({})", mixin.name, mixin.kind));
        kit.add_mixin(&mixin);
    }
    if kit.sandbox.image.is_empty() {
        bail!("kit {} names no image", kit.name);
    }
    let image = match crate::registry::pull(&kit.sandbox.image, progress) {
        Ok(i) => i,
        Err(e) => match crate::registry::find(&kit.sandbox.image) {
            Some(i) => {
                progress(&format!("couldn't refresh {} ({e:#}); using the copy pulled {}", kit.sandbox.image, i.pulled));
                i
            }
            None => return Err(e),
        },
    };
    Ok((kit, image))
}
