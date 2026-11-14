//! Pulling images from an OCI registry (Docker Hub by default) and turning them into what the VM mounts.
//!
//! containerd in sbx unpacks each image layer into its own erofs file (`layer.erofs`), which the shim
//! attaches as a read-only disk and stacks with overlayfs. We do the same: download each layer, decompress
//! it to a tar, and run sbx's own `mkfs.erofs.exe --tar=f --aufs` on it (`--aufs` turns `.wh.` whiteouts
//! into overlayfs ones). Layers are cached by digest under `%USERPROFILE%\elpis\images\layers`, and
//! each pulled image gets a record (`images\<name>.json`) with its layers and run config.
//!
//! Anonymous pulls only, with the registry's bearer-token challenge (enough for public Docker Hub repos).

use crate::session::debug_root;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// `[registry/]repository[:tag|@digest]`
#[derive(Clone, Debug, PartialEq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    /// A tag or a `sha256:` digest
    pub tag: String,
}

impl Reference {
    pub fn parse(text: &str) -> Result<Reference> {
        let text = text.trim();
        let (name, tag) = match text.split_once('@') {
            Some((n, d)) => (n, d.to_owned()),
            None => match text.rsplit_once(':') {
                Some((n, t)) if !t.contains('/') => (n, t.to_owned()),
                _ => (text, "latest".to_owned()),
            },
        };
        let (registry, repository) = match name.split_once('/') {
            Some((first, rest)) if first.contains('.') || first.contains(':') || first == "localhost" => (first.to_owned(), rest.to_owned()),
            _ => ("docker.io".to_owned(), name.to_owned()),
        };
        let repository = if registry == "docker.io" && !repository.contains('/') { format!("library/{repository}") } else { repository };
        if repository.is_empty() || !repository.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-/".contains(c)) {
            bail!("not an image reference: {text}");
        }
        Ok(Reference { registry, repository, tag })
    }

    fn api_host(&self) -> &str {
        if self.registry == "docker.io" { "registry-1.docker.io" } else { &self.registry }
    }

    /// A file-name-safe key for the image record.
    pub fn key(&self) -> String {
        self.to_string().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sep = if self.tag.starts_with("sha256:") { '@' } else { ':' };
        write!(f, "{}/{}{sep}{}", self.registry, self.repository, self.tag)
    }
}

/// A registry client for one repository, holding its pull token.
pub struct Registry {
    pub reference: Reference,
    agent: ureq::Agent,
    token: Mutex<Option<String>>,
}

impl Registry {
    pub fn new(reference: Reference) -> Registry {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).timeout_read(Duration::from_secs(60)).build();
        Registry { reference, agent, token: Mutex::new(None) }
    }

    fn url(&self, kind: &str, what: &str) -> String {
        format!("https://{}/v2/{}/{kind}/{what}", self.reference.api_host(), self.reference.repository)
    }

    /// GET with the current token, answering a 401 bearer challenge once.
    fn get(&self, url: &str, accept: Option<&str>) -> Result<ureq::Response> {
        for attempt in 0..2 {
            let mut request = self.agent.get(url);
            if let Some(a) = accept {
                request = request.set("Accept", a);
            }
            if let Some(t) = self.token.lock().unwrap().as_ref() {
                request = request.set("Authorization", &format!("Bearer {t}"));
            }
            match request.call() {
                Ok(response) => return Ok(response),
                Err(ureq::Error::Status(401, response)) if attempt == 0 => {
                    let challenge = response.header("www-authenticate").unwrap_or_default().to_owned();
                    *self.token.lock().unwrap() = Some(self.fetch_token(&challenge)?);
                }
                Err(ureq::Error::Status(code, response)) => {
                    let body = response.into_string().unwrap_or_default();
                    bail!("{url}: HTTP {code} {}", body.chars().take(200).collect::<String>());
                }
                Err(e) => return Err(e).with_context(|| format!("GET {url}")),
            }
        }
        bail!("{url}: not authorized (private repository?)")
    }

    fn fetch_token(&self, challenge: &str) -> Result<String> {
        let params = challenge.strip_prefix("Bearer ").with_context(|| format!("unsupported auth challenge: {challenge}"))?;
        let field = |name: &str| {
            params.split(',').find_map(|p| p.trim().strip_prefix(&format!("{name}=")).map(|v| v.trim_matches('"').to_owned()))
        };
        let realm = field("realm").context("auth challenge without a realm")?;
        let mut request = self.agent.get(&realm);
        if let Some(service) = field("service") {
            request = request.query("service", &service);
        }
        let scope = field("scope").unwrap_or_else(|| format!("repository:{}:pull", self.reference.repository));
        let body: Value = serde_json::from_reader(request.query("scope", &scope).call().context("fetching a pull token")?.into_reader())?;
        body["token"].as_str().or(body["access_token"].as_str()).map(str::to_owned).context("token response without a token")
    }

    /// The manifest for linux/amd64 (resolving an index) and its digest.
    pub fn manifest(&self) -> Result<(Value, String)> {
        let (mut manifest, mut digest) = self.manifest_at(&self.reference.tag)?;
        if manifest.get("manifests").is_some() {
            let chosen = manifest["manifests"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|m| m["platform"]["os"] == "linux" && m["platform"]["architecture"] == "amd64")
                .and_then(|m| m["digest"].as_str())
                .context("the image has no linux/amd64 variant")?
                .to_owned();
            (manifest, digest) = self.manifest_at(&chosen)?;
        }
        Ok((manifest, digest))
    }

    fn manifest_at(&self, tag: &str) -> Result<(Value, String)> {
        let response = self.get(&self.url("manifests", tag), Some(MANIFEST_TYPES))?;
        let header = response.header("docker-content-digest").map(str::to_owned);
        let mut body = Vec::new();
        response.into_reader().take(8 << 20).read_to_end(&mut body)?;
        let digest = header.unwrap_or_else(|| format!("sha256:{:x}", Sha256::digest(&body)));
        Ok((serde_json::from_slice(&body)?, digest))
    }

    /// A small blob (a config), read whole.
    pub fn blob_bytes(&self, digest: &str) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        self.get(&self.url("blobs", digest), None)?.into_reader().take(64 << 20).read_to_end(&mut body)?;
        verify(digest, &Sha256::digest(&body).into())?;
        Ok(body)
    }

    pub fn blob(&self, digest: &str) -> Result<Box<dyn Read + Send + Sync>> {
        Ok(self.get(&self.url("blobs", digest), None)?.into_reader())
    }
}

fn verify(digest: &str, actual: &[u8; 32]) -> Result<()> {
    let hex: String = actual.iter().map(|b| format!("{b:02x}")).collect();
    match digest.strip_prefix("sha256:") {
        Some(want) if want == hex => Ok(()),
        Some(want) => bail!("digest mismatch: expected sha256:{want}, got sha256:{hex}"),
        None => bail!("unsupported digest {digest}"),
    }
}

// ---------------------------------------------------------------- the image store

/// What an image runs by default, from its config.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunConfig {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Image {
    pub reference: String,
    pub digest: String,
    /// Layer files, bottom layer first
    pub layers: Vec<PathBuf>,
    pub config: RunConfig,
    /// The image's /etc/passwd and /etc/group, for resolving user names
    #[serde(default)]
    pub passwd: String,
    #[serde(default)]
    pub group: String,
    /// Total size of the layer files
    pub size: u64,
    pub pulled: String,
}

/// What we remember about a converted layer.
#[derive(Default, Serialize, Deserialize)]
struct LayerMeta {
    passwd: Option<String>,
    group: Option<String>,
}

pub fn images_root() -> PathBuf {
    debug_root().join("images")
}

fn layers_dir() -> PathBuf {
    images_root().join("layers")
}

/// The label sbx images carry when they want a Docker engine started inside (`sbx/shell-docker-image` and
/// every agent image built on it).
pub const START_DOCKER_LABEL: &str = "com.docker.sandboxes.start-docker";

impl Image {
    pub fn wants_docker(&self) -> bool {
        self.config.labels.get(START_DOCKER_LABEL).is_some_and(|v| v == "true")
    }

    /// (name, uid, gid) for the image's configured user; root when unset.
    pub fn user_ids(&self) -> (String, u32, u32) {
        resolve_user(&self.config.user, &self.passwd, &self.group)
    }

    /// Every user in the image's /etc/passwd: name → (uid, gid).
    pub fn users(&self) -> Vec<(String, u32, u32)> {
        self.passwd
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                Some((f.first()?.to_string(), f.get(2)?.parse().ok()?, f.get(3)?.parse().ok()?))
            })
            .collect()
    }

    /// Supplementary groups of `name` (from /etc/group).
    pub fn groups_of(&self, name: &str) -> Vec<u32> {
        self.group
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                let members = f.get(3)?;
                members.split(',').any(|m| m == name).then(|| f.get(2)?.parse().ok())?
            })
            .collect()
    }
}

/// "name", "uid", "name:group" or "uid:gid" → (name, uid, gid).
fn resolve_user(user: &str, passwd: &str, group: &str) -> (String, u32, u32) {
    if user.is_empty() {
        return ("root".into(), 0, 0);
    }
    let (u, g) = user.split_once(':').map_or((user, None), |(a, b)| (a, Some(b)));
    let entry = passwd.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.len() > 3 && (f[0] == u || f[2] == u));
    let (name, uid, mut gid) = match entry {
        Some(f) => (f[0].to_owned(), f[2].parse().unwrap_or(0), f[3].parse().unwrap_or(0)),
        None => (u.to_owned(), u.parse().unwrap_or(0), u.parse().unwrap_or(0)),
    };
    if let Some(g) = g {
        gid = g.parse().ok().or_else(|| {
            group.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.len() > 2 && f[0] == g).and_then(|f| f[2].parse().ok())
        }).unwrap_or(gid);
    }
    (name, uid, gid)
}

/// Images pulled so far.
pub fn list() -> Vec<Image> {
    let mut images: Vec<Image> = std::fs::read_dir(images_root())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect();
    images.sort_by(|a, b| a.reference.cmp(&b.reference));
    images
}

/// The pulled image for a reference, if any.
pub fn find(reference: &str) -> Option<Image> {
    let key = Reference::parse(reference).ok()?.key();
    serde_json::from_str(&std::fs::read_to_string(images_root().join(format!("{key}.json"))).ok()?).ok()
}

/// Remove an image record, and any layer files no other image or box uses.
pub fn remove(reference: &str) -> Result<()> {
    let r = Reference::parse(reference)?;
    let record = images_root().join(format!("{}.json", r.key()));
    let image: Image = serde_json::from_str(&std::fs::read_to_string(&record).with_context(|| format!("{r} isn't pulled"))?)?;
    std::fs::remove_file(&record)?;
    let in_use = |layer: &Path| {
        list().iter().any(|i| i.layers.iter().any(|l| l == layer)) || crate::workdir::layer_in_use(layer).is_some()
    };
    for layer in &image.layers {
        if !in_use(layer) {
            let _ = std::fs::remove_file(layer);
            let _ = std::fs::remove_file(layer.with_extension("json"));
        }
    }
    Ok(())
}

/// Pull an image (or reuse its cached layers) and record it. `progress` gets a line now and then.
pub fn pull(reference: &str, progress: &dyn Fn(&str)) -> Result<Image> {
    let reference = Reference::parse(reference)?;
    let registry = Registry::new(reference.clone());
    progress(&format!("resolving {reference}"));
    let (manifest, digest) = registry.manifest()?;
    let config: Value = serde_json::from_slice(&registry.blob_bytes(manifest["config"]["digest"].as_str().context("manifest without a config")?)?)?;
    let layers = manifest["layers"].as_array().context("manifest without layers")?;
    std::fs::create_dir_all(layers_dir())?;

    let mut files = Vec::new();
    let (mut passwd, mut group) = (String::new(), String::new());
    let total: u64 = layers.iter().filter_map(|l| l["size"].as_u64()).sum();
    progress(&format!("{} layer(s), {} to download", layers.len(), mb(total)));
    for (i, layer) in layers.iter().enumerate() {
        let digest = layer["digest"].as_str().context("layer without a digest")?;
        let media = layer["mediaType"].as_str().unwrap_or_default();
        let hex = digest.strip_prefix("sha256:").context("unsupported layer digest")?;
        let out = layers_dir().join(format!("{hex}.erofs"));
        let meta_path = out.with_extension("json");
        let label = format!("layer {}/{}", i + 1, layers.len());
        if !(out.exists() && meta_path.exists()) {
            let meta = convert_layer(&registry, digest, media, layer["size"].as_u64().unwrap_or(0), &out, &label, progress)?;
            std::fs::write(&meta_path, serde_json::to_string(&meta)?)?;
        } else {
            progress(&format!("{label}: cached"));
        }
        let meta: LayerMeta = serde_json::from_str(&std::fs::read_to_string(&meta_path)?).unwrap_or_default();
        if let Some(p) = meta.passwd {
            passwd = p;
        }
        if let Some(g) = meta.group {
            group = g;
        }
        files.push(out);
    }

    let c = &config["config"];
    let strings = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let image = Image {
        reference: reference.to_string(),
        digest,
        size: files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum(),
        layers: files,
        config: RunConfig {
            env: strings(&c["Env"]),
            user: c["User"].as_str().unwrap_or_default().to_owned(),
            working_dir: c["WorkingDir"].as_str().unwrap_or_default().to_owned(),
            entrypoint: strings(&c["Entrypoint"]),
            cmd: strings(&c["Cmd"]),
            labels: c["Labels"]
                .as_object()
                .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
                .unwrap_or_default(),
        },
        passwd,
        group,
        pulled: crate::win::local_time(),
    };
    std::fs::write(images_root().join(format!("{}.json", reference.key())), serde_json::to_string_pretty(&image)?)?;
    progress(&format!("pulled {reference} ({} on disk)", mb(image.size)));
    Ok(image)
}

fn mb(n: u64) -> String {
    format!("{:.1} MB", n as f64 / (1u64 << 20) as f64)
}

/// Download one layer, verify it, decompress it to a tar, note its /etc/passwd and /etc/group, make the erofs.
fn convert_layer(registry: &Registry, digest: &str, media: &str, size: u64, out: &Path, label: &str, progress: &dyn Fn(&str)) -> Result<LayerMeta> {
    if media.contains("zstd") {
        bail!("{label} is zstd-compressed, which isn't supported yet");
    }
    let gzip = media.contains("gzip") || media.is_empty();
    let tar_path = out.with_extension("tar.part");
    {
        let source = Counting { inner: registry.blob(digest)?, hasher: Sha256::new(), count: 0, size, label, progress, last: Instant::now() };
        let mut file = std::io::BufWriter::new(std::fs::File::create(&tar_path)?);
        let mut source = source;
        let copied = if gzip {
            let mut decoder = flate2::read::MultiGzDecoder::new(&mut source);
            std::io::copy(&mut decoder, &mut file)
        } else {
            std::io::copy(&mut source, &mut file)
        };
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&tar_path);
            return Err(e).with_context(|| format!("{label}: downloading"));
        }
        // Drain anything after the gzip stream so the digest covers the whole blob
        std::io::copy(&mut source, &mut std::io::sink())?;
        file.flush()?;
        let hash: [u8; 32] = source.hasher.finalize().into();
        if let Err(e) = verify(digest, &hash) {
            let _ = std::fs::remove_file(&tar_path);
            return Err(e.context(format!("{label}: corrupted download")));
        }
    }

    let meta = scan_tar(&tar_path).unwrap_or_default();
    progress(&format!("{label}: converting to erofs"));
    let tmp = out.with_extension("erofs.part");
    let result = crate::mkfs::erofs_from_tar(&tar_path, &tmp);
    let _ = std::fs::remove_file(&tar_path);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context(format!("{label}: converting to erofs")));
    }
    std::fs::rename(&tmp, out)?;
    Ok(meta)
}

/// The layer's /etc/passwd and /etc/group, if it has them.
fn scan_tar(path: &Path) -> Result<LayerMeta> {
    let mut meta = LayerMeta::default();
    let mut archive = tar::Archive::new(std::io::BufReader::new(std::fs::File::open(path)?));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().trim_start_matches("./").to_owned();
        if name == "etc/passwd" || name == "etc/group" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            if name == "etc/passwd" { meta.passwd = Some(text) } else { meta.group = Some(text) }
        }
    }
    Ok(meta)
}

/// Hashes and counts what passes through, reporting progress about once a second.
struct Counting<'a, R> {
    inner: R,
    hasher: Sha256,
    count: u64,
    size: u64,
    label: &'a str,
    progress: &'a dyn Fn(&str),
    last: Instant,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        if self.last.elapsed() > Duration::from_secs(1) {
            self.last = Instant::now();
            (self.progress)(&format!("{}: {} of {}", self.label, mb(self.count), mb(self.size)));
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references() {
        let r = Reference::parse("sbx/opencode-image").unwrap();
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_str()), ("docker.io", "sbx/opencode-image", "latest"));
        assert_eq!(Reference::parse("ubuntu:24.04").unwrap().repository, "library/ubuntu");
        let r = Reference::parse("ghcr.io/foo/bar:1.2").unwrap();
        assert_eq!((r.registry.as_str(), r.tag.as_str()), ("ghcr.io", "1.2"));
        let r = Reference::parse("localhost:5000/x@sha256:abc").unwrap();
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_str()), ("localhost:5000", "x", "sha256:abc"));
        assert_eq!(r.to_string(), "localhost:5000/x@sha256:abc");
    }

    #[test]
    fn users() {
        let passwd = "root:x:0:0::/root:/bin/sh\nagent:x:1000:1000::/home/agent:/bin/bash\n";
        let group = "root:x:0:\nagent:x:1000:\ndocker:x:1001:agent\n";
        assert_eq!(resolve_user("agent", passwd, group), ("agent".into(), 1000, 1000));
        assert_eq!(resolve_user("1000:docker", passwd, group), ("agent".into(), 1000, 1001));
        assert_eq!(resolve_user("", passwd, group), ("root".into(), 0, 0));
    }
}
