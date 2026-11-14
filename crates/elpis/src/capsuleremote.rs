//! Capsules in OCI registries: `capsule push` and `capsule pull`.
//!
//! A Capsule is stored as an OCI image manifest with its own artifact type, so any OCI registry holds it:
//! the config blob is `capsule.yaml` exactly as built, and the single layer is `payload.tar.gz` (a Capsule
//! without a payload carries the standard empty layer). Because both blobs are stored byte for byte, a pulled
//! Capsule has the same digest as the one that was pushed. The repository is `REGISTRY[/PREFIX]/NAMESPACE/NAME`
//! and the tag is the version.
//!
//! Nothing here is signed yet: whoever can write the tag can replace it, and the registry's own claims are never
//! trusted (the manifest is re-validated, the payload archive is checked entry by entry, digests are recomputed).
//! Pull by digest (`REF@sha256:…`) to pin exactly what you reviewed. The media types are Elpis's own, not the sbx
//! kit type.

use crate::capsule::Capsule;
use crate::capsulestore::{Built, Store, capsule_digest};
use crate::registry::{Reference, Registry};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const ARTIFACT_TYPE: &str = "application/vnd.elpis.capsule.v1";
pub const CONFIG_MEDIA: &str = "application/vnd.elpis.capsule.config.v1+yaml";
pub const PAYLOAD_MEDIA: &str = "application/vnd.elpis.capsule.payload.v1.tar+gzip";
const MANIFEST_MEDIA: &str = "application/vnd.oci.image.manifest.v1+json";
const EMPTY_MEDIA: &str = "application/vnd.oci.empty.v1+json";
/// sha256 of `{}`, the OCI empty descriptor's blob
const EMPTY_DIGEST: &str = "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
/// The Capsule's own digest, repeated on the manifest so `push` can tell "already there" from "different"
const ANNOTATION_DIGEST: &str = "dev.elpis.capsule.digest";
const MAX_CONFIG_BYTES: u64 = 1 << 20;
const MAX_PAYLOAD_BYTES: u64 = 256 << 20;

#[derive(Debug)]
pub struct Pushed {
    /// `registry/repository:tag`
    pub reference: String,
    /// The registry manifest's digest: pull `reference@digest` to get exactly this
    pub digest: String,
    /// False when the registry already held this exact Capsule
    pub uploaded: bool,
}

/// Where `built` goes under `dest` (`REGISTRY` or `REGISTRY/PREFIX`): `dest/NAMESPACE/NAME:VERSION`.
pub fn reference_for(dest: &str, built: &Built) -> Result<Reference> {
    let dest = dest.trim().trim_end_matches('/');
    if dest.is_empty() || dest.contains("://") || dest.contains('@') {
        bail!("{dest:?} should be a registry like registry.example.com, optionally with a path (registry.example.com/team)");
    }
    Reference::parse(&format!("{dest}/{}:{}", built.name, built.version))
}

/// Push a built Capsule (`NAME@VERSION`) to `dest`.
pub fn push(store: &Store, spec: &str, dest: &str, force: bool, progress: &dyn Fn(&str)) -> Result<Pushed> {
    let built = store.find(spec)?;
    let reference = reference_for(dest, &built)?;
    push_to(&Registry::new(reference), store, &built, force, progress)
}

pub fn push_to(registry: &Registry, store: &Store, built: &Built, force: bool, progress: &dyn Fn(&str)) -> Result<Pushed> {
    let (manifest_yaml, payload) = store.files(built)?; // checks the stored files against their digests first
    let tag = registry.reference.tag.clone();
    let reference = registry.reference.to_string();

    // A release is immutable: the same content is fine, different content needs --force
    if let Some((body, digest)) = registry.manifest_bytes(&tag, MANIFEST_MEDIA)? {
        let remote: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if remote["annotations"][ANNOTATION_DIGEST].as_str() == Some(built.digest.as_str()) {
            progress(&format!("{reference} already has this Capsule"));
            return Ok(Pushed { reference, digest, uploaded: false });
        }
        if !force {
            bail!("{reference} already exists with different content; a release is immutable, so bump metadata.version, or pass --force to overwrite it");
        }
    }

    progress(&format!("uploading the manifest ({} bytes)", manifest_yaml.len()));
    registry.push_blob(&built.manifest_digest, &manifest_yaml)?;
    let layer = match (&built.payload_digest, &payload) {
        (Some(digest), Some(bytes)) => {
            progress(&format!("uploading the payload ({:.1} KB)", bytes.len() as f64 / 1024.0));
            registry.push_blob(digest, bytes)?;
            json!({ "mediaType": PAYLOAD_MEDIA, "digest": digest, "size": bytes.len() })
        }
        _ => {
            registry.push_blob(EMPTY_DIGEST, b"{}")?;
            json!({ "mediaType": EMPTY_MEDIA, "digest": EMPTY_DIGEST, "size": 2 })
        }
    };
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_MEDIA,
        "artifactType": ARTIFACT_TYPE,
        "config": { "mediaType": CONFIG_MEDIA, "digest": built.manifest_digest, "size": manifest_yaml.len() },
        "layers": [layer],
        "annotations": {
            "org.opencontainers.image.version": built.version,
            "org.opencontainers.image.description": built.description,
            ANNOTATION_DIGEST: built.digest,
        },
    });
    let digest = registry.push_manifest(&tag, MANIFEST_MEDIA, &serde_json::to_vec(&manifest)?)?;
    Ok(Pushed { reference, digest, uploaded: true })
}

/// Pull `reference` (`REGISTRY/NAMESPACE/NAME:VERSION`, or `@sha256:…` to pin) into the local store.
pub fn pull(store: &Store, reference: &str, force: bool, progress: &dyn Fn(&str)) -> Result<Built> {
    pull_from(&Registry::new(Reference::parse(reference)?), store, force, progress)
}

/// What a registry holds for a reference, read without the payload: the Capsule, and its manifest digest.
pub fn inspect(reference: &str) -> Result<(Capsule, String)> {
    let (_, capsule, digest, _) = fetch_description(&Registry::new(Reference::parse(reference)?))?;
    Ok((capsule, digest))
}

/// The manifest and config of a Capsule in a registry, checked: (manifest JSON, Capsule, manifest digest, config bytes).
fn fetch_description(registry: &Registry) -> Result<(Value, Capsule, String, Vec<u8>)> {
    let tag = &registry.reference.tag;
    let (body, digest) = registry.manifest_bytes(tag, MANIFEST_MEDIA)?.with_context(|| format!("{} isn't in the registry", registry.reference))?;
    if tag.starts_with("sha256:") && *tag != digest {
        bail!("the registry returned a manifest with digest {digest} for {tag}");
    }
    let manifest: Value = serde_json::from_slice(&body).context("the registry's manifest isn't JSON")?;
    if manifest["artifactType"] != ARTIFACT_TYPE || manifest["config"]["mediaType"] != CONFIG_MEDIA {
        bail!("{} isn't an Elpis Capsule (artifact type {})", registry.reference, manifest["artifactType"].as_str().unwrap_or("none"));
    }
    let config_digest = manifest["config"]["digest"].as_str().context("the manifest has no config digest")?;
    if manifest["config"]["size"].as_u64().is_none_or(|s| s > MAX_CONFIG_BYTES) {
        bail!("the Capsule manifest is missing its size or is over {} KB", MAX_CONFIG_BYTES >> 10);
    }
    let config = registry.blob_limited(config_digest, MAX_CONFIG_BYTES)?;
    let capsule = Capsule::parse(std::str::from_utf8(&config).context("the Capsule manifest isn't UTF-8")?)?;

    // The reference has to say what the Capsule says it is, or a tag could pass one Capsule off as another
    let (repo, name) = (&registry.reference.repository, &capsule.metadata.name);
    if repo != name && !repo.ends_with(&format!("/{name}")) {
        bail!("{} holds the Capsule {}, which doesn't belong in the repository {repo}", registry.reference, capsule.reference());
    }
    if !tag.starts_with("sha256:") && *tag != capsule.metadata.version {
        bail!("tag {tag} holds {}; a tag must be the Capsule's version", capsule.reference());
    }
    Ok((manifest, capsule, digest, config))
}

pub fn pull_from(registry: &Registry, store: &Store, force: bool, progress: &dyn Fn(&str)) -> Result<Built> {
    let (manifest, capsule, manifest_digest, config) = fetch_description(registry)?;
    progress(&format!("{} ({})", capsule.reference(), crate::capsulestore::short(&manifest_digest)));

    let mut payload = None;
    for layer in manifest["layers"].as_array().into_iter().flatten() {
        match layer["mediaType"].as_str() {
            Some(PAYLOAD_MEDIA) if payload.is_none() => {
                let digest = layer["digest"].as_str().context("the payload layer has no digest")?;
                if layer["size"].as_u64().is_none_or(|s| s > MAX_PAYLOAD_BYTES) {
                    bail!("the payload layer is missing its size or is over {} MB", MAX_PAYLOAD_BYTES >> 20);
                }
                progress(&format!("downloading the payload ({:.1} KB)", layer["size"].as_u64().unwrap_or(0) as f64 / 1024.0));
                payload = Some(registry.blob_limited(digest, MAX_PAYLOAD_BYTES)?);
            }
            Some(EMPTY_MEDIA) => {}
            other => bail!("the Capsule has an unexpected layer ({})", other.unwrap_or("no media type")),
        }
    }

    // Our own digest from the bytes we hold; the registry's annotation is only a cross-check
    let digest = capsule_digest(&config, payload.as_deref());
    if let Some(claimed) = manifest["annotations"][ANNOTATION_DIGEST].as_str()
        && claimed != digest
    {
        bail!("the registry's annotation says this Capsule is {claimed}, but its contents hash to {digest}");
    }
    store.import(&config, payload.as_deref(), &registry.reference.to_string(), force)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    const DIGEST: &str = "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // ------------------------------------------------------------ a registry that speaks just enough OCI

    #[derive(Clone)]
    enum Auth {
        Open,
        /// The exact Authorization header required
        Basic(String),
        /// A token service that wants this Basic login, at `realm` (default: this server's /token)
        Bearer { login: String, realm: Option<String> },
    }

    #[derive(Default)]
    struct State {
        blobs: HashMap<String, Vec<u8>>,
        /// "repo:ref" → manifest bytes (by tag and by digest)
        manifests: HashMap<String, Vec<u8>>,
        /// Serve these bytes instead of the blob with this digest
        corrupt: Option<String>,
    }

    struct Mock {
        addr: String,
        state: Arc<Mutex<State>>,
    }

    fn mock(auth: Auth) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(State::default()));
        let (st, a) = (state.clone(), addr.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (st, auth, a) = (st.clone(), auth.clone(), a.clone());
                std::thread::spawn(move || serve(stream, &st, &auth, &a));
            }
        });
        Mock { addr, state }
    }

    fn respond(stream: &mut TcpStream, status: &str, headers: &[(&str, String)], body: &[u8], declared: usize, send_body: bool) {
        let mut out = format!("HTTP/1.1 {status}\r\nContent-Length: {declared}\r\nConnection: close\r\n");
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let _ = stream.write_all(out.as_bytes());
        if send_body {
            let _ = stream.write_all(body);
        }
    }

    fn serve(mut stream: TcpStream, state: &Mutex<State>, auth: &Auth, addr: &str) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
        let mut headers = HashMap::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.to_lowercase(), v.trim().to_owned());
            }
        }
        let mut body = vec![0; headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0)];
        reader.read_exact(&mut body).unwrap();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let given = headers.get("authorization").cloned().unwrap_or_default();

        if path == "/token" {
            return match auth {
                Auth::Bearer { login, .. } if given == *login => respond(&mut stream, "200 OK", &[], br#"{"token":"T"}"#, 13, true),
                _ => respond(&mut stream, "401 Unauthorized", &[], b"", 0, false),
            };
        }
        let allowed = match auth {
            Auth::Open => true,
            Auth::Basic(want) => given == *want,
            Auth::Bearer { .. } => given == "Bearer T",
        };
        if !allowed {
            let challenge = match auth {
                Auth::Basic(_) => "Basic realm=\"mock\"".to_owned(),
                Auth::Bearer { realm, .. } => {
                    format!("Bearer realm=\"{}\",service=\"mock\",scope=\"repository:r:pull,push\"", realm.clone().unwrap_or(format!("http://{addr}/token")))
                }
                Auth::Open => unreachable!(),
            };
            return respond(&mut stream, "401 Unauthorized", &[("WWW-Authenticate", challenge)], b"", 0, false);
        }

        let rest = path.strip_prefix("/v2/").unwrap_or_default();
        let mut state = state.lock().unwrap();
        if let Some((repo, _)) = rest.split_once("/blobs/uploads/") {
            if method == "POST" {
                return respond(&mut stream, "202 Accepted", &[("Location", format!("/v2/{repo}/blobs/uploads/u1"))], b"", 0, false);
            }
            let digest = query.strip_prefix("digest=").unwrap_or_default().to_owned();
            if method == "PUT" && digest == format!("sha256:{:x}", Sha256::digest(&body)) {
                state.blobs.insert(digest, body);
                return respond(&mut stream, "201 Created", &[], b"", 0, false);
            }
            return respond(&mut stream, "400 Bad Request", &[], b"bad digest", 10, true);
        }
        if let Some((_, digest)) = rest.split_once("/blobs/") {
            return match state.blobs.get(digest) {
                Some(b) => {
                    let served = if state.corrupt.as_deref() == Some(digest) { b"corrupted".to_vec() } else { b.clone() };
                    respond(&mut stream, "200 OK", &[], &served, served.len(), method != "HEAD")
                }
                None => respond(&mut stream, "404 Not Found", &[], b"", 0, false),
            };
        }
        if let Some((repo, reference)) = rest.split_once("/manifests/") {
            let key = format!("{repo}:{reference}");
            if method == "PUT" {
                let digest = format!("sha256:{:x}", Sha256::digest(&body));
                state.manifests.insert(format!("{repo}:{digest}"), body.clone());
                state.manifests.insert(key, body);
                return respond(&mut stream, "201 Created", &[], b"", 0, false);
            }
            return match state.manifests.get(&key) {
                Some(m) => respond(&mut stream, "200 OK", &[], m, m.len(), true),
                None => respond(&mut stream, "404 Not Found", &[], b"{}", 2, true),
            };
        }
        respond(&mut stream, "404 Not Found", &[], b"", 0, false);
    }

    // ------------------------------------------------------------ helpers

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("elpis-remote-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A store holding myteam/tool at `version`, with a payload (or none).
    fn store_with(tag: &str, version: &str, payload: bool) -> (Store, Built) {
        store_with_script(tag, version, payload, "echo hi")
    }

    fn store_with_script(tag: &str, version: &str, payload: bool, script: &str) -> (Store, Built) {
        let src = temp(&format!("{tag}-src"));
        let payload_yaml = if payload { "[{ from: ./agent, to: /opt/agent }]" } else { "[]" };
        write(
            &src.join("capsule.yaml"),
            &format!("apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: myteam/tool, version: {version}, description: a tool }}\nspec:\n  runtime: {{ image: {DIGEST}, entrypoint: [run] }}\n  payload: {payload_yaml}\n"),
        );
        write(&src.join("agent/run.sh"), &format!("#!/bin/sh\n{script}\n"));
        let store = Store::at(temp(&format!("{tag}-store")));
        let built = store.build(&src, false).unwrap();
        (store, built)
    }

    fn client(m: &Mock, repo_and_tag: &str, login: Option<(&str, &str)>) -> Registry {
        let r = Reference::parse(&format!("{}/{repo_and_tag}", m.addr)).unwrap();
        Registry::with_login(r, login.map(|(u, p)| (u.to_owned(), p.to_owned())))
    }

    fn quiet(_: &str) {}

    // ------------------------------------------------------------ tests

    #[test]
    fn a_pushed_capsule_pulls_back_identical() {
        let m = mock(Auth::Open);
        let (a, built) = store_with("rt-a", "1.0.0", true);
        let pushed = push_to(&client(&m, "myteam/tool:1.0.0", None), &a, &built, false, &quiet).unwrap();
        assert!(pushed.uploaded && pushed.digest.starts_with("sha256:"));
        assert!(pushed.reference.ends_with("/myteam/tool:1.0.0"));

        let b = Store::at(temp("rt-b-store"));
        let got = pull_from(&client(&m, "myteam/tool:1.0.0", None), &b, false, &quiet).unwrap();
        assert_eq!(got.digest, built.digest, "a pulled Capsule has the digest it was pushed with");
        assert_eq!((got.payload_files, got.reference()), (1, "myteam/tool@1.0.0".to_owned()));
        assert!(got.origin.unwrap().ends_with("/myteam/tool:1.0.0"));
        assert_eq!(b.load(&b.find("myteam/tool").unwrap()).unwrap().reference(), "myteam/tool@1.0.0");
        assert_eq!(std::fs::read(got.path.join("payload.tar.gz")).unwrap(), std::fs::read(built.path.join("payload.tar.gz")).unwrap());
    }

    #[test]
    fn a_capsule_without_a_payload_uses_the_empty_layer() {
        let m = mock(Auth::Open);
        let (a, built) = store_with("np-a", "1.0.0", false);
        push_to(&client(&m, "myteam/tool:1.0.0", None), &a, &built, false, &quiet).unwrap();
        let got = pull_from(&client(&m, "myteam/tool:1.0.0", None), &Store::at(temp("np-b")), false, &quiet).unwrap();
        assert!(got.payload_digest.is_none());
        assert_eq!(got.digest, built.digest);
    }

    #[test]
    fn pushing_twice_is_fine_but_a_release_is_immutable() {
        let m = mock(Auth::Open);
        let (store, built) = store_with("imm", "1.0.0", true);
        let r = client(&m, "myteam/tool:1.0.0", None);
        assert!(push_to(&r, &store, &built, false, &quiet).unwrap().uploaded);
        assert!(!push_to(&r, &store, &built, false, &quiet).unwrap().uploaded, "same content: nothing to do");

        // different content under the same version
        let (other, changed) = store_with_script("imm2", "1.0.0", true, "echo changed");
        assert_ne!(changed.digest, built.digest);
        let e = format!("{:#}", push_to(&r, &other, &changed, false, &quiet).unwrap_err());
        assert!(e.contains("immutable") && e.contains("--force"), "{e}");
        assert!(push_to(&r, &other, &changed, true, &quiet).unwrap().uploaded);
        let got = pull_from(&r, &Store::at(temp("imm-pull")), false, &quiet).unwrap();
        assert_eq!(got.digest, changed.digest);
    }

    #[test]
    fn basic_auth_needs_the_stored_login() {
        let m = mock(Auth::Basic("Basic dXNlcjpwYXNz".into())); // user:pass
        let (store, built) = store_with("basic", "1.0.0", true);
        let e = format!("{:#}", push_to(&client(&m, "myteam/tool:1.0.0", None), &store, &built, false, &quiet).unwrap_err());
        assert!(e.contains("secret set registry.127.0.0.1_"), "{e}");
        push_to(&client(&m, "myteam/tool:1.0.0", Some(("user", "pass"))), &store, &built, false, &quiet).unwrap();
        let wrong = format!("{:#}", push_to(&client(&m, "myteam/tool:1.0.1", Some(("user", "nope"))), &store, &built, false, &quiet).unwrap_err());
        assert!(wrong.contains("401") || wrong.contains("not authorized"), "{wrong}");
    }

    #[test]
    fn a_bearer_token_is_fetched_with_the_login() {
        let m = mock(Auth::Bearer { login: "Basic dXNlcjpwYXNz".into(), realm: None });
        let (store, built) = store_with("bearer", "1.0.0", true);
        push_to(&client(&m, "myteam/tool:1.0.0", Some(("user", "pass"))), &store, &built, false, &quiet).unwrap();
        // pulling needs the token too; the token service wants the login
        assert!(pull_from(&client(&m, "myteam/tool:1.0.0", None), &Store::at(temp("bearer-anon")), false, &quiet).is_err());
        pull_from(&client(&m, "myteam/tool:1.0.0", Some(("user", "pass"))), &Store::at(temp("bearer-pull")), false, &quiet).unwrap();
    }

    #[test]
    fn a_token_service_over_plain_http_on_another_host_is_refused() {
        let m = mock(Auth::Bearer { login: "Basic dXNlcjpwYXNz".into(), realm: Some("http://tokens.example.com/token".into()) });
        let (store, built) = store_with("realm", "1.0.0", true);
        let e = format!("{:#}", push_to(&client(&m, "myteam/tool:1.0.0", Some(("user", "pass"))), &store, &built, false, &quiet).unwrap_err());
        assert!(e.contains("isn't HTTPS"), "{e}");
    }

    #[test]
    fn something_that_is_not_a_capsule_is_refused() {
        let m = mock(Auth::Open);
        let image = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:x","size":1},"layers":[]}"#;
        m.state.lock().unwrap().manifests.insert("myteam/tool:1.0.0".into(), image.to_vec());
        let e = format!("{:#}", pull_from(&client(&m, "myteam/tool:1.0.0", None), &Store::at(temp("notcap")), false, &quiet).unwrap_err());
        assert!(e.contains("isn't an Elpis Capsule"), "{e}");
        let e = format!("{:#}", pull_from(&client(&m, "myteam/tool:9.9.9", None), &Store::at(temp("missing")), false, &quiet).unwrap_err());
        assert!(e.contains("isn't in the registry"), "{e}");
    }

    #[test]
    fn a_tag_or_repository_cannot_pass_one_capsule_off_as_another() {
        let m = mock(Auth::Open);
        let (store, built) = store_with("mis", "1.0.0", true);
        push_to(&client(&m, "myteam/tool:1.0.0", None), &store, &built, false, &quiet).unwrap();
        // the same manifest under another tag, and under another repository
        {
            let mut s = m.state.lock().unwrap();
            let body = s.manifests["myteam/tool:1.0.0"].clone();
            s.manifests.insert("myteam/tool:2.0.0".into(), body.clone());
            s.manifests.insert("other/thing:1.0.0".into(), body);
        }
        let e = format!("{:#}", pull_from(&client(&m, "myteam/tool:2.0.0", None), &Store::at(temp("mis-a")), false, &quiet).unwrap_err());
        assert!(e.contains("tag 2.0.0") && e.contains("version"), "{e}");
        let e = format!("{:#}", pull_from(&client(&m, "other/thing:1.0.0", None), &Store::at(temp("mis-b")), false, &quiet).unwrap_err());
        assert!(e.contains("doesn't belong in the repository"), "{e}");
    }

    #[test]
    fn pulling_by_digest_pins_the_manifest() {
        let m = mock(Auth::Open);
        let (store, built) = store_with("pin", "1.0.0", true);
        let pushed = push_to(&client(&m, "myteam/tool:1.0.0", None), &store, &built, false, &quiet).unwrap();
        let got = pull_from(&client(&m, &format!("myteam/tool@{}", pushed.digest), None), &Store::at(temp("pin-b")), false, &quiet).unwrap();
        assert_eq!(got.digest, built.digest);
        // a registry that answers a digest with a different manifest is caught
        let other = mock(Auth::Open);
        let (s2, b2) = store_with("pin2", "1.0.0", false);
        push_to(&client(&other, "myteam/tool:1.0.0", None), &s2, &b2, false, &quiet).unwrap();
        {
            let mut s = other.state.lock().unwrap();
            let body = s.manifests["myteam/tool:1.0.0"].clone();
            s.manifests.insert(format!("myteam/tool:{}", pushed.digest), body);
        }
        let e = format!("{:#}", pull_from(&client(&other, &format!("myteam/tool@{}", pushed.digest), None), &Store::at(temp("pin-c")), false, &quiet).unwrap_err());
        assert!(e.contains("returned a manifest with digest"), "{e}");
    }

    #[test]
    fn a_corrupted_blob_is_caught_by_its_digest() {
        let m = mock(Auth::Open);
        let (store, built) = store_with("cor", "1.0.0", true);
        push_to(&client(&m, "myteam/tool:1.0.0", None), &store, &built, false, &quiet).unwrap();
        m.state.lock().unwrap().corrupt = built.payload_digest.clone();
        let e = format!("{:#}", pull_from(&client(&m, "myteam/tool:1.0.0", None), &Store::at(temp("cor-b")), false, &quiet).unwrap_err());
        assert!(e.contains("digest mismatch"), "{e}");
    }

    #[test]
    fn pulling_a_different_build_of_a_stored_version_needs_force() {
        let m = mock(Auth::Open);
        let (store, built) = store_with("pf", "1.0.0", true);
        push_to(&client(&m, "myteam/tool:1.0.0", None), &store, &built, false, &quiet).unwrap();
        // the local store has its own, different 1.0.0
        let (local, _) = store_with("pf-local", "1.0.0", false);
        let e = format!("{:#}", pull_from(&client(&m, "myteam/tool:1.0.0", None), &local, false, &quiet).unwrap_err());
        assert!(e.contains("immutable") && e.contains("--force"), "{e}");
        assert_eq!(pull_from(&client(&m, "myteam/tool:1.0.0", None), &local, true, &quiet).unwrap().digest, built.digest);
    }

    #[test]
    fn destinations_become_repositories() {
        let (_, built) = store_with("dest", "1.0.0", false);
        let r = reference_for("registry.example.com/team/", &built).unwrap();
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_str()), ("registry.example.com", "team/myteam/tool", "1.0.0"));
        assert!(reference_for("https://registry.example.com", &built).is_err());
        assert!(reference_for("", &built).is_err());
    }

    // ------------------------------------------------------------ untrusted payload archives

    fn evil_tar(make: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>)) -> Vec<u8> {
        let mut t = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        make(&mut t);
        t.into_inner().unwrap().finish().unwrap()
    }

    fn file_entry(t: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>, path: &str, mode: u32) {
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(mode);
        t.append_data(&mut h, path, &b"x"[..]).unwrap();
    }

    fn manifest_for_archive() -> Capsule {
        Capsule::parse(&format!("apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: a/b, version: 1.0.0 }}\nspec:\n  runtime: {{ image: {DIGEST}, entrypoint: [run] }}\n  payload: [{{ from: ./x, to: /opt/agent }}]\n")).unwrap()
    }

    fn check(gz: &[u8]) -> String {
        match crate::capsulestore::validate_payload_archive(&manifest_for_archive(), gz) {
            Ok(n) => format!("ok {n}"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn archives_inside_the_targets_pass() {
        assert_eq!(check(&evil_tar(|t| file_entry(t, "opt/agent/run.sh", 0o755))), "ok 1");
        assert_eq!(check(&evil_tar(|_| {})), "ok 0");
    }

    #[test]
    fn archives_outside_the_targets_or_with_odd_entries_are_refused() {
        assert!(check(&evil_tar(|t| file_entry(t, "etc/passwd", 0o644))).contains("outside the manifest's payload targets"));
        assert!(check(&evil_tar(|t| file_entry(t, "opt/agentx/f", 0o644))).contains("outside"), "a sibling folder with the same prefix is outside");
        assert!(check(&evil_tar(|t| file_entry(t, "opt/agent/su", 0o4755))).contains("setuid"));
        assert!(check(b"not a gzip").contains("gzipped tar") || check(b"not a gzip").contains("payload"));

        let link = evil_tar(|t| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_link_name("/etc/shadow").unwrap();
            t.append_data(&mut h, "opt/agent/l", std::io::empty()).unwrap();
        });
        assert!(check(&link).contains("only files and folders"));

        // `..` can't be written through the tar crate's API, so put it in the header bytes directly
        let traversal = evil_tar(|t| {
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            let name = b"opt/agent/../../etc/cron.d/x";
            h.as_old_mut().name[..name.len()].copy_from_slice(name);
            h.set_cksum();
            t.append(&h, &b"x"[..]).unwrap();
        });
        assert!(check(&traversal).contains("`..`"), "{}", check(&traversal));
    }

    #[test]
    fn a_payload_the_manifest_does_not_declare_is_refused() {
        let (_, built) = store_with("undeclared", "1.0.0", false);
        let manifest = std::fs::read(built.path.join("capsule.yaml")).unwrap();
        let e = format!("{:#}", Store::at(temp("undeclared-b")).import(&manifest, Some(&evil_tar(|t| file_entry(t, "opt/agent/f", 0o644))), "x", false).unwrap_err());
        assert!(e.contains("doesn't declare"), "{e}");
        let (_, with) = store_with("declared", "1.0.0", true);
        let manifest = std::fs::read(with.path.join("capsule.yaml")).unwrap();
        let e = format!("{:#}", Store::at(temp("declared-b")).import(&manifest, None, "x", false).unwrap_err());
        assert!(e.contains("declares a payload"), "{e}");
    }

    #[test]
    fn builds_pass_their_own_validation() {
        let (_, built) = store_with("self", "1.0.0", true);
        let capsule = Capsule::parse(&std::fs::read_to_string(built.path.join("capsule.yaml")).unwrap()).unwrap();
        let gz = std::fs::read(built.path.join("payload.tar.gz")).unwrap();
        assert_eq!(crate::capsulestore::validate_payload_archive(&capsule, &gz).unwrap(), 1);
    }
}
