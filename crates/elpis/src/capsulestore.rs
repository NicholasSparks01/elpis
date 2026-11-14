//! The local Capsule store: `capsule build` output, kept under `<data folder>\capsules\NAMESPACE\NAME\VERSION\`.
//!
//! Each built version holds the manifest as written (`capsule.yaml`), the payload as one deterministic
//! `payload.tar.gz` (if the manifest has payload entries) and a `build.json` record with the digests. The
//! Capsule's digest covers the manifest bytes and the payload bytes, so the same inputs always give the same
//! digest. Registries, signing and running come later; this is the part they will all read from.
//!
//! Security: payload folders are walked without following symlinks, must stay inside the Capsule folder,
//! skip `.git`, and are scanned for obvious secrets, so a build can't quietly pick up files from elsewhere
//! on the machine or ship a credential. Stored Capsules are re-hashed on load.

use crate::capsule::{Capsule, PayloadEntry, looks_like_secret, parse_version, validate_capsule_name};
use crate::session::debug_root;
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "capsule.yaml";
pub const PAYLOAD_FILE: &str = "payload.tar.gz";
const RECORD_FILE: &str = "build.json";
/// A payload bigger than this is almost certainly a mistake (a `node_modules`, a disk image): fail loudly.
const MAX_PAYLOAD_BYTES: u64 = 256 << 20;
/// Files up to this size are scanned for secrets.
const SCAN_LIMIT: u64 = 2 << 20;

/// What `build` recorded about a stored Capsule version.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Built {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Covers the manifest and the payload
    pub digest: String,
    pub manifest_digest: String,
    pub payload_digest: Option<String>,
    pub payload_files: u64,
    /// Size of `payload.tar.gz`
    pub payload_bytes: u64,
    #[serde(skip)]
    pub path: PathBuf,
}

impl Built {
    pub fn reference(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    pub fn payload_path(&self) -> Option<PathBuf> {
        self.payload_digest.as_ref().map(|_| self.path.join(PAYLOAD_FILE))
    }
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The store in the Elpis data folder.
    pub fn open() -> Store {
        Store { root: debug_root().join("capsules") }
    }

    pub fn at(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    fn version_dir(&self, name: &str, version: &str) -> PathBuf {
        // both were validated (lowercase/digits/hyphens, numbers), so they can't carry a path separator or `..`
        self.root.join(name).join(version)
    }

    /// Validate the Capsule in `dir` (or at the `capsule.yaml` path), pack its payload and store it. Building a
    /// version that exists with the same content does nothing; with different content it fails unless `force`.
    pub fn build(&self, dir: &Path, force: bool) -> Result<Built> {
        let manifest_path = if dir.is_dir() { dir.join(MANIFEST_FILE) } else { dir.to_owned() };
        let folder = manifest_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let bytes = std::fs::read(&manifest_path).with_context(|| format!("reading {}; run this in a Capsule folder or pass its path", manifest_path.display()))?;
        let text = std::str::from_utf8(&bytes).with_context(|| format!("{} isn't UTF-8", manifest_path.display()))?;
        let capsule = Capsule::parse(text).with_context(|| format!("in {}", manifest_path.display()))?;
        if !capsule.compatible_with(env!("CARGO_PKG_VERSION"))? {
            bail!(
                "{} needs Elpis {} or newer; this is {}. Update Elpis, or lower spec.compatibility.elpis if that's not true",
                capsule.reference(),
                capsule.spec.compatibility.elpis.as_deref().unwrap_or("").trim_start_matches(">="),
                env!("CARGO_PKG_VERSION")
            );
        }

        let (payload, payload_files) = if capsule.spec.payload.is_empty() {
            (None, 0)
        } else {
            let (gz, n) = build_payload(folder, &capsule.spec.payload)?;
            (Some(gz), n)
        };
        let manifest_digest = sha256(&bytes);
        let payload_digest = payload.as_deref().map(sha256);
        let built = Built {
            name: capsule.metadata.name.clone(),
            version: capsule.metadata.version.clone(),
            description: capsule.metadata.description.clone(),
            digest: combined_digest(&manifest_digest, payload_digest.as_deref()),
            manifest_digest,
            payload_digest,
            payload_files,
            payload_bytes: payload.as_ref().map_or(0, |p| p.len() as u64),
            path: PathBuf::new(),
        };

        let dest = self.version_dir(&built.name, &built.version);
        if dest.exists() {
            match read_record(&dest) {
                Some(old) if old.digest == built.digest => return Ok(old),
                _ if force => std::fs::remove_dir_all(&dest).with_context(|| format!("replacing {}", dest.display()))?,
                old => bail!(
                    "{} is already built{}; a version is immutable, so bump metadata.version, or pass --force to replace it",
                    built.reference(),
                    old.map_or(String::new(), |o| format!(" with different content ({})", short(&o.digest)))
                ),
            }
        }
        let parent = dest.parent().context("capsule store path")?;
        std::fs::create_dir_all(parent)?;
        // write beside the destination and rename, so a failed build never leaves a half-written version
        let tmp = parent.join(format!(".{}.tmp", built.version));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let write = || -> Result<()> {
            std::fs::write(tmp.join(MANIFEST_FILE), &bytes)?;
            if let Some(p) = &payload {
                std::fs::write(tmp.join(PAYLOAD_FILE), p)?;
            }
            std::fs::write(tmp.join(RECORD_FILE), serde_json::to_vec_pretty(&built)?)?;
            std::fs::rename(&tmp, &dest).with_context(|| format!("moving the build into {}", dest.display()))
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        Ok(Built { path: dest, ..built })
    }

    /// Every built Capsule version, by name then version.
    pub fn list(&self) -> Vec<Built> {
        let mut out = Vec::new();
        for ns in subdirs(&self.root) {
            for name in subdirs(&ns) {
                for version in subdirs(&name) {
                    out.extend(read_record(&version));
                }
            }
        }
        out.sort_by(|a, b| (&a.name, parse_version(&a.version).unwrap_or_default()).cmp(&(&b.name, parse_version(&b.version).unwrap_or_default())));
        out
    }

    /// `NAME@VERSION`, or `NAME` for the highest built version.
    pub fn find(&self, spec: &str) -> Result<Built> {
        let (name, version) = match spec.split_once('@') {
            Some((n, v)) => (n, Some(v)),
            None => (spec, None),
        };
        validate_capsule_name(name).map_err(|_| anyhow::anyhow!("{spec:?} isn't a Capsule reference; use namespace/name or namespace/name@1.2.0"))?;
        match version {
            Some(v) => {
                parse_version(v)?;
                read_record(&self.version_dir(name, v)).with_context(|| format!("{name}@{v} isn't built; run `elpis-run capsule build` in its folder, or `capsule ls` to see what is"))
            }
            None => self
                .list()
                .into_iter()
                .rfind(|b| b.name == name)
                .with_context(|| format!("no built Capsule named {name}; run `elpis-run capsule build` in its folder, or `capsule ls` to see what is")),
        }
    }

    /// The stored manifest, after checking the stored files still match the digests from the build.
    pub fn load(&self, built: &Built) -> Result<Capsule> {
        let manifest = std::fs::read(built.path.join(MANIFEST_FILE)).with_context(|| format!("reading {}", built.reference()))?;
        let changed = || format!("{} has been modified since it was built; rebuild it", built.reference());
        if sha256(&manifest) != built.manifest_digest {
            bail!("{}", changed());
        }
        if let Some(want) = &built.payload_digest {
            let payload = std::fs::read(built.path.join(PAYLOAD_FILE)).with_context(changed)?;
            if &sha256(&payload) != want {
                bail!("{}", changed());
            }
        }
        Capsule::parse(std::str::from_utf8(&manifest)?)
    }

    /// The payload as an erofs layer, made once per payload digest and shared by every box from that Capsule.
    /// None when the Capsule has no payload. Call after [`Store::load`], which has checked the archive.
    pub fn payload_layer(&self, built: &Built) -> Result<Option<PathBuf>> {
        let (Some(digest), Some(archive)) = (&built.payload_digest, built.payload_path()) else { return Ok(None) };
        let layers = self.root.join(".layers");
        let out = layers.join(format!("{}.erofs", digest.trim_start_matches("sha256:")));
        if out.exists() {
            return Ok(Some(out));
        }
        std::fs::create_dir_all(&layers)?;
        // mkfs.erofs wants a plain tar
        let tar = out.with_extension("tar.part");
        let part = out.with_extension("erofs.part");
        let result = (|| -> Result<()> {
            let mut src = flate2::read::GzDecoder::new(std::fs::File::open(&archive).with_context(|| format!("reading {}", archive.display()))?);
            let mut dst = std::io::BufWriter::new(std::fs::File::create(&tar)?);
            std::io::copy(&mut src, &mut dst)?;
            std::io::Write::flush(&mut dst)?;
            crate::mkfs::erofs_from_tar(&tar, &part).context("converting the payload to a layer")?;
            std::fs::rename(&part, &out)?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&tar);
        if let Err(e) = result {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        Ok(Some(out))
    }

    /// Delete one built version (`NAME@VERSION`; a bare name is refused so nothing is removed by accident).
    pub fn remove(&self, spec: &str) -> Result<Built> {
        if !spec.contains('@') {
            bail!("name the version to remove, e.g. {spec}@1.0.0 (`capsule ls` lists them)");
        }
        let built = self.find(spec)?;
        std::fs::remove_dir_all(&built.path).with_context(|| format!("removing {}", built.path.display()))?;
        // tidy empty NAME and NAMESPACE folders
        let mut dir = built.path.parent().map(Path::to_owned);
        for _ in 0..2 {
            let Some(d) = dir else { break };
            if std::fs::remove_dir(&d).is_err() {
                break;
            }
            dir = d.parent().map(Path::to_owned);
        }
        Ok(built)
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn read_record(dir: &Path) -> Option<Built> {
    let mut b: Built = serde_json::from_slice(&std::fs::read(dir.join(RECORD_FILE)).ok()?).ok()?;
    b.path = dir.to_owned();
    Some(b)
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn combined_digest(manifest: &str, payload: Option<&str>) -> String {
    sha256(format!("manifest:{manifest}\npayload:{}\n", payload.unwrap_or("none")).as_bytes())
}

/// `sha256:` and the first 12 hex digits.
pub fn short(digest: &str) -> &str {
    &digest[..digest.len().min(19)]
}

enum Item {
    Dir,
    File { src: PathBuf, size: u64, exec: bool },
}

/// Pack the payload entries into one gzipped tar, with fixed owner, times and modes so the same files always
/// hash the same. Returns it and the number of files.
fn build_payload(folder: &Path, entries: &[PayloadEntry]) -> Result<(Vec<u8>, u64)> {
    let root = folder.canonicalize().with_context(|| format!("opening {}", folder.display()))?;
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    let mut total = 0u64;
    for e in entries {
        let src = root.join(&e.from);
        collect(&root, &src, e.to.trim_matches('/'), &mut items, &mut total).with_context(|| format!("payload {} → {}", e.from, e.to))?;
    }

    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut files = 0u64;
    for (name, item) in &items {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        match item {
            Item::Dir => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                h.set_size(0);
                tar.append_data(&mut h, format!("{name}/"), std::io::empty())?;
            }
            Item::File { src, size, exec } => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(if *exec { 0o755 } else { 0o644 });
                h.set_size(*size);
                let f = std::fs::File::open(src).with_context(|| format!("reading {}", src.display()))?;
                // a file that grew since it was measured would corrupt the archive; take exactly what was measured
                tar.append_data(&mut h, name, f.take(*size))?;
                files += 1;
            }
        }
    }
    let gz = tar.into_inner()?.finish()?;
    Ok((gz, files))
}

fn collect(root: &Path, src: &Path, tar_name: &str, items: &mut BTreeMap<String, Item>, total: &mut u64) -> Result<()> {
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("{} doesn't exist", src.display()))?;
    if meta.file_type().is_symlink() {
        bail!("{} is a symbolic link; payloads don't follow them. Copy the real file into the Capsule folder", src.display());
    }
    // junctions and mount points aren't reported as symlinks, so also check where the path really goes
    if !src.canonicalize()?.starts_with(root) {
        bail!("{} leads outside the Capsule folder", src.display());
    }
    let mut put = |name: &str, item: Item| -> Result<()> {
        if items.insert(name.to_owned(), item).is_some() {
            bail!("two payload entries both write /{name}");
        }
        Ok(())
    };
    if meta.is_file() {
        let (exec, size) = inspect_file(src, meta.len())?;
        *total += size;
        if *total > MAX_PAYLOAD_BYTES {
            bail!("the payload is over {} MB; ship large data some other way (an image layer, a volume)", MAX_PAYLOAD_BYTES >> 20);
        }
        return put(tar_name, Item::File { src: src.to_owned(), size, exec });
    }
    if !meta.is_dir() {
        bail!("{} is neither a file nor a folder", src.display());
    }
    put(tar_name, Item::Dir)?;
    let mut children: Vec<_> = std::fs::read_dir(src)?.collect::<std::io::Result<_>>()?;
    children.sort_by_key(|c| c.file_name());
    for child in children {
        let name = child.file_name();
        let name = name.to_str().with_context(|| format!("{} has a name that isn't valid Unicode", child.path().display()))?;
        if name == ".git" {
            continue;
        }
        collect(root, &child.path(), &format!("{tar_name}/{name}"), items, total)?;
    }
    Ok(())
}

/// Whether a file should be executable in the box (a `#!` script or `.sh`: Windows has no exec bit to read), and
/// its size. Refuses files that look like they hold a secret.
fn inspect_file(path: &Path, size: u64) -> Result<(bool, u64)> {
    let mut head = Vec::new();
    std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?.take(SCAN_LIMIT).read_to_end(&mut head)?;
    let exec = head.starts_with(b"#!") || path.extension().is_some_and(|e| e.eq_ignore_ascii_case("sh"));
    if size <= SCAN_LIMIT && let Ok(text) = std::str::from_utf8(&head) && let Some(why) = scan_text(text) {
        bail!("{} {why}; Capsules never carry secrets. Remove it, and rotate it if it was ever committed", path.display());
    }
    Ok((exec, size))
}

/// Looks at the whole text and at each word of it, since a key sits inside a line of code or config.
fn scan_text(text: &str) -> Option<&'static str> {
    if let Some(why) = looks_like_secret("", text) {
        return Some(why);
    }
    text.split(|c: char| c.is_whitespace() || "\"'`,;:=()[]{}<>".contains(c)).find_map(|w| looks_like_secret("", w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;

    const DIGEST: &str = "docker.io/library/node@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("elpis-capsule-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn manifest(version: &str, payload: &str) -> String {
        format!("apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: myteam/tool, version: {version}, description: a tool }}\nspec:\n  runtime: {{ image: {DIGEST}, entrypoint: [run] }}\n  payload: {payload}\n")
    }

    /// A Capsule folder with a script, a data file and a `.git` folder that must not be shipped.
    fn folder(tag: &str, version: &str) -> PathBuf {
        let d = temp(tag);
        write(&d.join(MANIFEST_FILE), &manifest(version, "[{ from: ./agent, to: /opt/agent }]"));
        write(&d.join("agent/run.sh"), "#!/bin/sh\necho hi\n");
        write(&d.join("agent/sub/data.txt"), "data\n");
        write(&d.join("agent/.git/config"), "x\n");
        d
    }

    fn entries(p: &Path) -> Vec<(String, u32)> {
        let gz = std::fs::read(p).unwrap();
        let mut a = tar::Archive::new(GzDecoder::new(&gz[..]));
        a.entries().unwrap().map(|e| e.unwrap()).map(|e| (e.path().unwrap().to_string_lossy().into_owned(), e.header().mode().unwrap())).collect()
    }

    #[test]
    fn builds_stores_lists_loads_and_removes() {
        let src = folder("build-src", "1.0.0");
        let store = Store::at(temp("build-store"));
        let b = store.build(&src, false).unwrap();
        assert_eq!(b.reference(), "myteam/tool@1.0.0");
        assert_eq!(b.payload_files, 2);
        assert!(b.digest.starts_with("sha256:") && b.payload_digest.is_some());

        // the archive: sorted, no .git, scripts executable, files not
        let e = entries(&b.payload_path().unwrap());
        let names: Vec<&str> = e.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["opt/agent/", "opt/agent/run.sh", "opt/agent/sub/", "opt/agent/sub/data.txt"]);
        assert_eq!(e[1].1, 0o755);
        assert_eq!(e[3].1, 0o644);

        assert_eq!(store.list().len(), 1);
        assert_eq!(store.find("myteam/tool").unwrap().digest, b.digest);
        assert_eq!(store.find("myteam/tool@1.0.0").unwrap().digest, b.digest);
        assert_eq!(store.load(&b).unwrap().reference(), "myteam/tool@1.0.0");

        assert!(store.remove("myteam/tool").is_err(), "a bare name must not remove anything");
        store.remove("myteam/tool@1.0.0").unwrap();
        assert!(store.list().is_empty());
        assert!(store.find("myteam/tool").is_err());
    }

    #[test]
    fn same_inputs_give_the_same_digest() {
        let (a, b) = (folder("det-a", "1.0.0"), folder("det-b", "1.0.0"));
        let one = Store::at(temp("det-s1")).build(&a, false).unwrap();
        let two = Store::at(temp("det-s2")).build(&b, false).unwrap();
        assert_eq!(one.digest, two.digest);
        assert_eq!(one.payload_digest, two.payload_digest);
    }

    #[test]
    fn a_built_version_is_immutable_unless_forced() {
        let src = folder("imm-src", "1.0.0");
        let store = Store::at(temp("imm-store"));
        let first = store.build(&src, false).unwrap();
        assert_eq!(store.build(&src, false).unwrap().digest, first.digest, "rebuilding the same content is a no-op");

        write(&src.join("agent/sub/data.txt"), "changed\n");
        let e = format!("{:#}", store.build(&src, false).unwrap_err());
        assert!(e.contains("immutable") && e.contains("--force"), "{e}");
        assert_eq!(store.find("myteam/tool@1.0.0").unwrap().digest, first.digest, "the stored version is untouched");

        let forced = store.build(&src, true).unwrap();
        assert_ne!(forced.digest, first.digest);
    }

    #[test]
    fn find_without_a_version_takes_the_highest() {
        let store = Store::at(temp("hi-store"));
        for v in ["1.2.0", "1.10.0", "1.9.0"] {
            store.build(&folder(&format!("hi-{v}"), v), false).unwrap();
        }
        assert_eq!(store.find("myteam/tool").unwrap().version, "1.10.0");
        let order: Vec<String> = store.list().into_iter().map(|b| b.version).collect();
        assert_eq!(order, ["1.2.0", "1.9.0", "1.10.0"]);
    }

    #[test]
    fn a_modified_store_is_refused_on_load() {
        let store = Store::at(temp("tamper-store"));
        let b = store.build(&folder("tamper-src", "1.0.0"), false).unwrap();
        let payload = b.payload_path().unwrap();
        std::fs::write(&payload, b"not the payload").unwrap();
        assert!(format!("{:#}", store.load(&b).unwrap_err()).contains("modified"));
        let b = store.build(&folder("tamper-src2", "1.0.1"), false).unwrap();
        let manifest = b.path.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&manifest).unwrap().replace("a tool", "a worse tool");
        std::fs::write(&manifest, text).unwrap();
        assert!(format!("{:#}", store.load(&b).unwrap_err()).contains("modified"));
    }

    #[test]
    fn payload_with_a_secret_is_refused() {
        let src = folder("secret-src", "1.0.0");
        write(&src.join("agent/config.json"), "{ \"token\": \"sk-abcdefghijklmnopqrstuv\" }");
        let store = Store::at(temp("secret-store"));
        let e = format!("{:#}", store.build(&src, false).unwrap_err());
        assert!(e.contains("config.json") && e.contains("secrets"), "{e}");
        assert!(store.list().is_empty(), "a refused build stores nothing");

        let src = folder("secret-src2", "1.0.0");
        write(&src.join("agent/id"), "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n");
        assert!(store.build(&src, false).is_err());
    }

    #[test]
    fn missing_payload_paths_and_bad_manifests_say_what_is_wrong() {
        let d = temp("bad-src");
        write(&d.join(MANIFEST_FILE), &manifest("1.0.0", "[{ from: ./nope, to: /opt/x }]"));
        let store = Store::at(temp("bad-store"));
        assert!(format!("{:#}", store.build(&d, false).unwrap_err()).contains("nope"));

        write(&d.join(MANIFEST_FILE), &manifest("1.0.0", "[]").replace("run]", "run], bogus: 1"));
        assert!(format!("{:#}", store.build(&d, false).unwrap_err()).contains("capsule.yaml"));

        assert!(format!("{:#}", store.build(&temp("empty-src"), false).unwrap_err()).contains("capsule.yaml"));
    }

    #[test]
    fn a_payload_file_can_be_mapped_to_a_path() {
        let d = temp("file-src");
        write(&d.join(MANIFEST_FILE), &manifest("1.0.0", "[{ from: ./prompt.md, to: /home/agent/prompt.md }]"));
        write(&d.join("prompt.md"), "be nice\n");
        let b = Store::at(temp("file-store")).build(&d, false).unwrap();
        let names: Vec<String> = entries(&b.payload_path().unwrap()).into_iter().map(|e| e.0).collect();
        assert_eq!(names, ["home/agent/prompt.md"]);
    }

    #[test]
    fn two_entries_cannot_write_the_same_path() {
        let d = temp("dup-src");
        // identical targets are caught by the manifest check; overlapping ones only show up when the folders are walked
        write(&d.join(MANIFEST_FILE), &manifest("1.0.0", "[{ from: ./a, to: /opt/x }, { from: ./b, to: /opt/x/f }]"));
        write(&d.join("a/f"), "1");
        write(&d.join("b"), "2");
        assert!(format!("{:#}", Store::at(temp("dup-store")).build(&d, false).unwrap_err()).contains("both write"));
    }

    #[test]
    fn references_are_checked_before_touching_the_disk() {
        let store = Store::at(temp("ref-store"));
        assert!(store.find("../../etc").is_err());
        assert!(store.find("myteam/tool@../x").is_err());
        assert!(store.find("myteam/tool@1.0.0").is_err());
    }

    #[test]
    fn a_capsule_without_a_payload_has_no_archive() {
        let d = temp("nop-src");
        write(&d.join(MANIFEST_FILE), &manifest("1.0.0", "[]"));
        let b = Store::at(temp("nop-store")).build(&d, false).unwrap();
        assert!(b.payload_digest.is_none() && b.payload_path().is_none() && !b.path.join(PAYLOAD_FILE).exists());
    }
}
