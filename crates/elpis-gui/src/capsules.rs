//! Capsule support for the GUI: everything the Capsules view does, as Tauri commands over the shared library
//! (`elpis::capsule*`). The logic is in plain functions returning `Result<T, String>` so it can be tested
//! without a window; the `#[tauri::command]` wrappers below only move it onto a blocking thread (hashing a
//! payload or talking to a registry shouldn't stall the UI) and forward progress lines as `capsule-progress` events.
//!
//! Text that came from a manifest or a registry is passed through [`display_safe`] before it reaches the page;
//! the page also never puts it in `innerHTML`.

use elpis::capsule::{Capsule, display_safe};
use elpis::capsulesign::{self, Report, SignatureRecord, Standing, Trust, badge, verify};
use elpis::capsulestore::{Built, Store, short};
use elpis::{capsulerun, capsuleremote};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};

type Res<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn store() -> Store {
    Store::open()
}

// ---------------------------------------------------------------- what the page receives

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Badge {
    pub text: String,
    /// "good", "warn", "bad" or "dim"
    pub level: &'static str,
}

fn badge_of(report: Option<&Report>) -> Badge {
    let (text, level) = badge(report);
    Badge { text, level: level.name() }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CapsuleRow {
    pub reference: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub digest: String,
    pub short_digest: String,
    pub payload_files: u64,
    /// The registry reference it was pulled from; null for a local build
    pub origin: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CheckDto {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SignatureDto {
    pub key_id: String,
    pub public_key: String,
    /// "trusted", "out-of-scope", "unknown" or "invalid"
    pub standing: &'static str,
    /// The trusted key's label, or why a signature is invalid
    pub detail: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ReportDto {
    pub checks: Vec<CheckDto>,
    pub provenance: Option<String>,
    pub signatures: Vec<SignatureDto>,
    pub verdict: String,
    pub badge: Badge,
    pub intact: bool,
}

fn report_dto(r: &Report) -> ReportDto {
    ReportDto {
        checks: r
            .checks
            .iter()
            .map(|c| match &c.result {
                Ok(d) => CheckDto { name: c.name.to_owned(), ok: true, detail: display_safe(d) },
                Err(e) => CheckDto { name: c.name.to_owned(), ok: false, detail: display_safe(e) },
            })
            .collect(),
        provenance: r.provenance.as_ref().map(|p| {
            format!(
                "{} @ {}{}",
                display_safe(p.git.as_deref().unwrap_or("(no remote)")),
                display_safe(p.commit.as_deref().unwrap_or("?")),
                if p.dirty { " (built with uncommitted changes)" } else { "" }
            )
        }),
        signatures: r
            .signatures
            .iter()
            .map(|s| {
                let (standing, detail) = match &s.standing {
                    Standing::Trusted(n) => ("trusted", display_safe(n)),
                    Standing::OutOfScope(n) => ("out-of-scope", display_safe(n)),
                    Standing::Unknown => ("unknown", String::new()),
                    Standing::Invalid(why) => ("invalid", display_safe(why)),
                };
                SignatureDto { key_id: s.key_id.clone(), public_key: s.public_key.clone(), standing, detail }
            })
            .collect(),
        verdict: r.verdict(),
        badge: badge_of(Some(r)),
        intact: r.intact(),
    }
}

fn row(b: &Built) -> CapsuleRow {
    CapsuleRow {
        reference: b.reference(),
        name: b.name.clone(),
        version: b.version.clone(),
        description: display_safe(&b.description),
        digest: b.digest.clone(),
        short_digest: short(&b.digest).to_owned(),
        payload_files: b.payload_files,
        origin: b.origin.as_deref().map(display_safe),
    }
}

// ---------------------------------------------------------------- the store

/// The built Capsules, without checking them (checking reads whole payloads; see [`verify_all`]).
pub fn list() -> Res<Vec<CapsuleRow>> {
    Ok(store().list().iter().map(row).collect())
}

/// Check every built Capsule against the consumer's trust policy: digest → badge.
pub fn verify_all() -> Res<Vec<(String, Badge)>> {
    let (store, trust) = (store(), Trust::load().map_err(err)?);
    Ok(store.list().into_iter().map(|b| (b.digest.clone(), badge_of(Some(&verify(&store, &b, &trust))))).collect())
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Detail {
    pub reference: String,
    pub digest: String,
    /// The manifest's summary, line by line (what it runs, what it asks for)
    pub summary: Vec<String>,
    pub report: ReportDto,
}

pub fn detail(reference: &str) -> Res<Detail> {
    let (store, trust) = (store(), Trust::load().map_err(err)?);
    let built = store.find(reference).map_err(err)?;
    let report = verify(&store, &built, &trust);
    // the manifest only, for display; whether the stored files are intact is the report's job
    let summary = match std::fs::read_to_string(built.path.join(elpis::capsulestore::MANIFEST_FILE)).map_err(|e| e.to_string()).and_then(|t| Capsule::parse(&t).map_err(err)) {
        Ok(c) => c.summary(Some(&built.digest)).lines().map(str::to_owned).collect(),
        Err(e) => vec![format!("the manifest can't be read: {}", display_safe(&e))],
    };
    Ok(Detail { reference: built.reference(), digest: built.digest.clone(), summary, report: report_dto(&report) })
}

pub fn remove(reference: &str) -> Res<String> {
    store().remove(reference).map(|b| b.reference()).map_err(err)
}

/// Build the Capsule in a folder (validating its manifest) and store it.
pub fn build(dir: &str, force: bool) -> Res<CapsuleRow> {
    if dir.trim().is_empty() {
        return Err("choose the Capsule's folder (the one with capsule.yaml)".into());
    }
    store().build(Path::new(dir.trim()), force).map(|b| row(&b)).map_err(err)
}

// ---------------------------------------------------------------- running

/// What the Run dialog shows before anything is created: the Capsule's standing and exactly what it asks for.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RunPrompt {
    pub reference: String,
    /// Pass back to [`run`]: it refuses if the Capsule changed since the dialog showed it
    pub digest: String,
    pub verdict: String,
    pub badge: Badge,
    pub network: Vec<String>,
    pub host_access: bool,
    /// Services it asks credentials for (not injected yet)
    pub credentials: Vec<String>,
    pub declares_workspace: bool,
    pub box_name: String,
}

/// Load and check a Capsule under the trust policy. Fails with the policy's message when it refuses (the
/// page can then offer `allow_unsigned` for one run); a Capsule whose files don't match its build is always refused.
pub fn prepare_run(reference: &str, allow_unsigned: bool) -> Res<RunPrompt> {
    let store = store();
    let built = store.find(reference).map_err(err)?;
    let prepared = capsulerun::prepare(&store, &built, allow_unsigned).map_err(err)?;
    let grants = prepared.requested();
    let capsule = &prepared.capsule;
    Ok(RunPrompt {
        reference: built.reference(),
        digest: built.digest.clone(),
        verdict: prepared.report.verdict(),
        badge: badge_of(Some(&prepared.report)),
        network: grants.network.iter().map(|h| display_safe(h)).collect(),
        host_access: grants.host_access,
        credentials: capsule.spec.requests.credentials.iter().map(|c| display_safe(&c.service)).collect(),
        declares_workspace: capsule.spec.interfaces.workspace.is_some(),
        box_name: capsulerun::default_box_name(capsule, None),
    })
}

/// Make the box. `digest` is the one the Run dialog showed; the grants are the Capsule's requests as the dialog
/// listed them, and become the box's whole network policy. Returns the new box's name.
pub fn run(reference: &str, digest: &str, name: &str, workspace: &str, allow_unsigned: bool, progress: &dyn Fn(&str)) -> Res<String> {
    let store = store();
    let built = store.find(reference).map_err(err)?;
    if built.digest != digest {
        return Err(format!("{} changed since the dialog showed it; open Run again", built.reference()));
    }
    let prepared = capsulerun::prepare(&store, &built, allow_unsigned).map_err(err)?;
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err("give the box a name".into());
    }
    let workspace = workspace.trim();
    let declares = prepared.capsule.spec.interfaces.workspace.is_some();
    if declares && workspace.is_empty() {
        return Err("this Capsule works on a folder: choose the workspace".into());
    }
    let workspace = capsulerun::workspace_for(&prepared.capsule, (!workspace.is_empty()).then(|| Path::new(workspace)), Path::new("")).map_err(err)?;
    let grants = prepared.requested();
    let opts = capsulerun::Options { name: name.clone(), workspace, ports: Vec::new(), cpus: None, memory_mb: None, auto_stop: None, grants };
    capsulerun::create_box(&store, prepared, opts, progress).map_err(err)?;
    Ok(name)
}

// ---------------------------------------------------------------- sharing

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    /// What Pull takes (a registry reference), or the built NAME@VERSION for a local hit
    pub reference: String,
    pub local: bool,
    pub name: String,
    pub version: String,
    pub description: String,
    pub signatures_published: bool,
}

/// Search your store, or a registry (`REGISTRY[/PREFIX]`) when one is given.
pub fn search(query: &str, registry: &str, all_versions: bool, progress: &dyn Fn(&str)) -> Res<Vec<Hit>> {
    let registry = registry.trim();
    if registry.is_empty() {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        return Ok(store()
            .list()
            .iter()
            .filter(|b| {
                let text = format!("{} {}", b.name, b.description).to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
            })
            .map(|b| Hit { reference: b.reference(), local: true, name: b.name.clone(), version: b.version.clone(), description: display_safe(&b.description), signatures_published: false })
            .collect());
    }
    capsuleremote::search(registry, query, all_versions, progress)
        .map(|found| {
            found
                .into_iter()
                .map(|f| Hit { reference: f.reference, local: false, name: f.name, version: f.version, description: display_safe(&f.description), signatures_published: f.signatures_published })
                .collect()
        })
        .map_err(err)
}

/// Read a registry Capsule's manifest without its payload: (summary lines, registry manifest digest).
pub fn inspect_remote(reference: &str) -> Res<(Vec<String>, String)> {
    let (capsule, digest) = capsuleremote::inspect(reference.trim()).map_err(err)?;
    Ok((capsule.summary(None).lines().map(str::to_owned).collect(), digest))
}

pub fn pull(reference: &str, force: bool, progress: &dyn Fn(&str)) -> Res<CapsuleRow> {
    capsuleremote::pull(&store(), reference.trim(), force, progress).map(|b| row(&b)).map_err(err)
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PushResult {
    pub reference: String,
    pub digest: String,
    pub uploaded: bool,
    pub signatures: usize,
}

pub fn push(reference: &str, to: &str, force: bool, progress: &dyn Fn(&str)) -> Res<PushResult> {
    capsuleremote::push(&store(), reference, to.trim(), force, progress)
        .map(|p| PushResult { reference: p.reference, digest: p.digest, uploaded: p.uploaded, signatures: p.signatures })
        .map_err(err)
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ForkResult {
    pub dir: String,
    pub files: u64,
    pub name: String,
    pub version: String,
    pub parent: String,
}

/// Start a fork. `source` is a built NAME[@VERSION], or a registry reference (pulled first).
pub fn fork(source: &str, name: &str, dir: &str, version: &str, progress: &dyn Fn(&str)) -> Res<ForkResult> {
    let store = store();
    let source = source.trim();
    let built = if source.matches('/').count() >= 2 { capsuleremote::pull(&store, source, false, progress).map_err(err)? } else { store.find(source).map_err(err)? };
    let dir = dir.trim();
    if dir.is_empty() {
        return Err("choose a folder for the fork".into());
    }
    let version = version.trim();
    let f = store.fork(&built, name.trim(), (!version.is_empty()).then_some(version), Path::new(dir)).map_err(err)?;
    Ok(ForkResult { dir: f.dir.to_string_lossy().into_owned(), files: f.files, name: f.name, version: f.version, parent: f.parent })
}

/// Where a fork of `name` goes by default: a folder under the Elpis data folder, so it isn't mistaken for a box.
pub fn default_fork_dir(name: &str) -> String {
    let leaf = name.rsplit('/').next().unwrap_or("capsule");
    let dir: PathBuf = elpis::session::debug_root().join("capsule-src").join(leaf);
    dir.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------- signing and trust

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct KeyDto {
    pub name: String,
    pub public_key: String,
}

pub fn keys() -> Vec<KeyDto> {
    capsulesign::keys().into_iter().map(|(name, public_key)| KeyDto { name, public_key }).collect()
}

/// A new signing key; returns its public key.
pub fn keygen(name: &str) -> Res<String> {
    capsulesign::create_key(name.trim()).map_err(err)
}

/// Sign a built Capsule with a private key (checked against its files first: no vouching for a modified Capsule).
pub fn sign(reference: &str, key_name: &str) -> Res<String> {
    let store = store();
    let built = store.find(reference).map_err(err)?;
    store.load(&built).map_err(err)?;
    let pkcs8 = capsulesign::load_key(key_name.trim()).map_err(err)?;
    sign_with(&store, &built, &pkcs8)
}

fn sign_with(store: &Store, built: &Built, pkcs8: &[u8]) -> Res<String> {
    let record: SignatureRecord = capsulesign::sign(pkcs8, built).map_err(err)?;
    store.add_signature(built, &record).map_err(err)?;
    Ok(record.key_id)
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrustDto {
    pub require_signed: bool,
    pub keys: Vec<TrustedKeyDto>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrustedKeyDto {
    pub name: String,
    pub public_key: String,
    pub namespaces: Vec<String>,
}

fn trust_dto(t: &Trust) -> TrustDto {
    TrustDto {
        require_signed: t.require_signed,
        keys: t.keys.iter().map(|k| TrustedKeyDto { name: display_safe(&k.name), public_key: k.public_key.clone(), namespaces: k.namespaces.clone() }).collect(),
    }
}

pub fn trust_get() -> Res<TrustDto> {
    Trust::load().map(|t| trust_dto(&t)).map_err(err)
}

/// Trust a key for the given namespaces (comma- or space-separated; `*` for all).
pub fn trust_add(name: &str, public_key: &str, namespaces: &str) -> Res<TrustDto> {
    let spaces: Vec<String> = namespaces.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect();
    let mut trust = Trust::load().map_err(err)?;
    trust.add(name.trim(), public_key.trim(), &spaces).map_err(err)?;
    trust.save().map_err(err)?;
    Ok(trust_dto(&trust))
}

pub fn trust_remove(name: &str) -> Res<TrustDto> {
    let mut trust = Trust::load().map_err(err)?;
    if !trust.remove(name) {
        return Err(format!("no trusted key named {name}"));
    }
    trust.save().map_err(err)?;
    Ok(trust_dto(&trust))
}

pub fn trust_require(on: bool) -> Res<TrustDto> {
    let mut trust = Trust::load().map_err(err)?;
    trust.require_signed = on;
    trust.save().map_err(err)?;
    Ok(trust_dto(&trust))
}

// ---------------------------------------------------------------- Tauri commands

/// Run blocking library work off the UI thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

/// Progress lines from a long operation, as `capsule-progress` events.
fn progress_to(app: &AppHandle) -> impl Fn(&str) + Send + 'static {
    let app = app.clone();
    move |line: &str| {
        let _ = app.emit("capsule-progress", line.to_owned());
    }
}

#[tauri::command]
pub async fn capsule_list() -> Res<Vec<CapsuleRow>> {
    blocking(list).await
}

#[tauri::command]
pub async fn capsule_verify_all() -> Res<Vec<(String, Badge)>> {
    blocking(verify_all).await
}

#[tauri::command]
pub async fn capsule_detail(reference: String) -> Res<Detail> {
    blocking(move || detail(&reference)).await
}

#[tauri::command]
pub async fn capsule_remove(reference: String) -> Res<String> {
    blocking(move || remove(&reference)).await
}

#[tauri::command]
pub async fn capsule_build(dir: String, force: bool) -> Res<CapsuleRow> {
    blocking(move || build(&dir, force)).await
}

#[tauri::command]
pub async fn capsule_prepare_run(reference: String, allow_unsigned: bool) -> Res<RunPrompt> {
    blocking(move || prepare_run(&reference, allow_unsigned)).await
}

#[tauri::command]
pub async fn capsule_run(app: AppHandle, reference: String, digest: String, name: String, workspace: String, allow_unsigned: bool) -> Res<String> {
    let progress = progress_to(&app);
    blocking(move || run(&reference, &digest, &name, &workspace, allow_unsigned, &progress)).await
}

#[tauri::command]
pub async fn capsule_search(app: AppHandle, query: String, registry: String, all_versions: bool) -> Res<Vec<Hit>> {
    let progress = progress_to(&app);
    blocking(move || search(&query, &registry, all_versions, &progress)).await
}

#[tauri::command]
pub async fn capsule_inspect_remote(reference: String) -> Res<(Vec<String>, String)> {
    blocking(move || inspect_remote(&reference)).await
}

#[tauri::command]
pub async fn capsule_pull(app: AppHandle, reference: String, force: bool) -> Res<CapsuleRow> {
    let progress = progress_to(&app);
    blocking(move || pull(&reference, force, &progress)).await
}

#[tauri::command]
pub async fn capsule_push(app: AppHandle, reference: String, to: String, force: bool) -> Res<PushResult> {
    let progress = progress_to(&app);
    blocking(move || push(&reference, &to, force, &progress)).await
}

#[tauri::command]
pub async fn capsule_fork(app: AppHandle, source: String, name: String, dir: String, version: String) -> Res<ForkResult> {
    let progress = progress_to(&app);
    blocking(move || fork(&source, &name, &dir, &version, &progress)).await
}

#[tauri::command]
pub fn capsule_default_fork_dir(name: String) -> String {
    default_fork_dir(&name)
}

/// The registry `capsule search` defaults to (`ELPIS_CAPSULE_REGISTRY`), or an empty string.
#[tauri::command]
pub fn capsule_default_registry() -> String {
    std::env::var("ELPIS_CAPSULE_REGISTRY").unwrap_or_default().trim().to_owned()
}

#[tauri::command]
pub async fn capsule_keys() -> Res<Vec<KeyDto>> {
    blocking(|| Ok(keys())).await
}

#[tauri::command]
pub async fn capsule_keygen(name: String) -> Res<String> {
    blocking(move || keygen(&name)).await
}

#[tauri::command]
pub async fn capsule_sign(reference: String, key: String) -> Res<String> {
    blocking(move || sign(&reference, &key)).await
}

#[tauri::command]
pub async fn capsule_trust() -> Res<TrustDto> {
    blocking(trust_get).await
}

#[tauri::command]
pub async fn capsule_trust_add(name: String, public_key: String, namespaces: String) -> Res<TrustDto> {
    blocking(move || trust_add(&name, &public_key, &namespaces)).await
}

#[tauri::command]
pub async fn capsule_trust_remove(name: String) -> Res<TrustDto> {
    blocking(move || trust_remove(&name)).await
}

#[tauri::command]
pub async fn capsule_trust_require(on: bool) -> Res<TrustDto> {
    blocking(move || trust_require(on)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, Once};

    const DIGEST: &str = "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// One temporary data folder (ELPIS_HOME) shared by every test, so none touches the real one; the lock
    /// keeps tests that change shared state (the store, the trust policy) from overlapping.
    static LOCK: Mutex<()> = Mutex::new(());

    fn home() -> PathBuf {
        static ONCE: Once = Once::new();
        let dir = std::env::temp_dir().join(format!("elpis-gui-test-{}", std::process::id()));
        ONCE.call_once(|| {
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: every test calls this first, under LOCK, before anything else reads the environment
            unsafe { std::env::set_var("ELPIS_HOME", &dir) };
        });
        dir
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = std::fs::remove_file(home().join("capsule-trust.json")); // each test starts from the default policy
        guard
    }

    /// Build `guitest/NAME` into the shared store.
    fn capsule(name: &str, meta_extra: &str, spec_extra: &str) -> Built {
        let src = home().join(format!("src-{name}"));
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(src.join("agent")).unwrap();
        std::fs::write(src.join("agent/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(
            src.join("capsule.yaml"),
            format!("apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: guitest/{name}, version: 1.0.0{meta_extra} }}\nspec:\n  runtime: {{ image: {DIGEST}, entrypoint: [/opt/agent/run.sh] }}\n  payload: [{{ from: ./agent, to: /opt/agent }}]\n{spec_extra}"),
        )
        .unwrap();
        store().build(&src, true).unwrap()
    }

    fn quiet(_: &str) {}

    #[test]
    fn the_list_and_badges_reflect_signatures_and_trust() {
        let _g = lock();
        let built = capsule("badged", ", description: \"Badge test\"", "");
        let rows = list().unwrap();
        let r = rows.iter().find(|r| r.reference == "guitest/badged@1.0.0").unwrap();
        assert_eq!((r.description.as_str(), r.payload_files, r.origin.as_deref()), ("Badge test", 1, None));
        assert_eq!(r.short_digest.len(), 19);
        let badge_of = |digest: &str| verify_all().unwrap().into_iter().find(|(d, _)| d == digest).unwrap().1;
        assert_eq!(badge_of(&built.digest), Badge { text: "unsigned".into(), level: "dim" });

        // signed by a key the policy doesn't know, then by one it trusts
        let (pkcs8, public) = capsulesign::generate().unwrap();
        let key_id = sign_with(&store(), &built, &pkcs8).unwrap();
        assert_eq!(key_id.len(), 16);
        assert_eq!(badge_of(&built.digest), Badge { text: "signed, untrusted".into(), level: "warn" });
        trust_add("alice", &capsulesign::public_key_string(&public), "guitest").unwrap();
        assert_eq!(badge_of(&built.digest), Badge { text: "✓ alice".into(), level: "good" });

        // tampering is caught whoever signed it
        std::fs::write(built.payload_path().unwrap(), b"swapped").unwrap();
        assert_eq!(badge_of(&built.digest), Badge { text: "✗ modified".into(), level: "bad" });
    }

    #[test]
    fn the_detail_has_the_summary_and_the_whole_verification() {
        let _g = lock();
        let built = capsule("detailed", "", "  requests: { network: { allow: [example.com] } }\n");
        let (pkcs8, _) = capsulesign::generate().unwrap();
        sign_with(&store(), &built, &pkcs8).unwrap();
        let d = detail("guitest/detailed@1.0.0").unwrap();
        assert!(d.summary[0] == "guitest/detailed@1.0.0" && d.summary.iter().any(|l| l.contains("example.com")), "{:?}", d.summary);
        assert!(d.report.intact && d.report.checks.len() == 2 && d.report.checks.iter().all(|c| c.ok));
        assert_eq!((d.report.signatures.len(), d.report.signatures[0].standing), (1, "unknown"));
        assert_eq!(d.report.provenance, None);
        assert!(d.report.verdict.starts_with("signed, but not by a key you trust"));
        assert!(detail("guitest/nope@9.9.9").unwrap_err().contains("isn't built"));
    }

    #[test]
    fn the_run_dialog_shows_requests_and_the_policy_can_refuse() {
        let _g = lock();
        let built = capsule("runner", "", "  requests:\n    network: { allow: [example.com] }\n    hostAccess: true\n    credentials: [{ service: anthropic, auth: [apiKey] }]\n  interfaces: { workspace: { path: /work } }\n");
        let p = prepare_run("guitest/runner@1.0.0", false).unwrap();
        assert_eq!((p.network.as_slice(), p.host_access, p.credentials.as_slice()), (["example.com".to_owned()].as_slice(), true, ["anthropic".to_owned()].as_slice()));
        assert!(p.declares_workspace && p.box_name == "runner" && p.digest == built.digest);
        assert_eq!(p.badge.level, "dim");

        // a policy that requires signatures refuses, and says so; the one-run override passes it
        trust_require(true).unwrap();
        let e = prepare_run("guitest/runner@1.0.0", false).unwrap_err();
        assert!(e.contains("requires that"), "{e}");
        assert!(prepare_run("guitest/runner@1.0.0", true).is_ok());

        // a modified Capsule is refused even with the override
        std::fs::write(built.payload_path().unwrap(), b"swapped").unwrap();
        let e = prepare_run("guitest/runner@1.0.0", true).unwrap_err();
        assert!(e.contains("modified") || e.contains("integrity"), "{e}");
    }

    #[test]
    fn run_checks_what_the_dialog_showed_before_it_creates_anything() {
        let _g = lock();
        let built = capsule("guarded", "", "  interfaces: { workspace: { path: /work } }\n");
        let plain = capsule("plain", "", "");
        let before = std::fs::read_dir(home()).unwrap().count();
        // the digest must be the one that was shown
        let e = run("guitest/guarded@1.0.0", "sha256:other", "b", "C:/x", false, &quiet).unwrap_err();
        assert!(e.contains("changed since the dialog showed it"), "{e}");
        let e = run("guitest/guarded@1.0.0", &built.digest, "  ", "C:/x", false, &quiet).unwrap_err();
        assert!(e.contains("name"), "{e}");
        let e = run("guitest/guarded@1.0.0", &built.digest, "b", " ", false, &quiet).unwrap_err();
        assert!(e.contains("choose the workspace"), "{e}");
        // a folder for a Capsule that declares none is refused too
        let e = run("guitest/plain@1.0.0", &plain.digest, "b", "C:/x", false, &quiet).unwrap_err();
        assert!(e.contains("doesn't declare a workspace"), "{e}");
        assert_eq!(std::fs::read_dir(home()).unwrap().count(), before, "no refusal left anything behind");
    }

    #[test]
    fn trust_is_managed_with_scoped_keys() {
        let _g = lock();
        let (_, public) = capsulesign::generate().unwrap();
        let public = capsulesign::public_key_string(&public);
        assert_eq!(trust_get().unwrap(), TrustDto { require_signed: false, keys: vec![] });
        assert!(trust_add("bob", &public, "").unwrap_err().contains("namespaces"), "a namespace is required");
        let t = trust_add("bob", &public, "team-a, team-b").unwrap();
        assert_eq!(t.keys, vec![TrustedKeyDto { name: "bob".into(), public_key: public.clone(), namespaces: vec!["team-a".into(), "team-b".into()] }]);
        assert!(trust_add("bob2", &public, "x").unwrap_err().contains("already trusted"));
        assert!(trust_require(true).unwrap().require_signed);
        assert!(trust_get().unwrap().require_signed, "it was saved");
        assert!(trust_remove("nobody").is_err());
        assert!(trust_remove("bob").unwrap().keys.is_empty());
    }

    #[test]
    fn local_search_matches_every_word_and_removal_works() {
        let _g = lock();
        capsule("finder-one", ", description: \"Reviews pull requests\"", "");
        capsule("finder-two", ", description: \"Drafts release notes\"", "");
        let found = |q: &str| search(q, "", false, &quiet).unwrap().into_iter().filter(|h| h.name.starts_with("guitest/finder")).map(|h| h.name).collect::<Vec<_>>();
        assert_eq!(found("pull requests"), ["guitest/finder-one"]);
        assert_eq!(found("FINDER notes"), ["guitest/finder-two"]);
        assert_eq!(found("finder").len(), 2);
        assert!(found("finder banana").is_empty());
        assert!(search("x", "", false, &quiet).unwrap().iter().all(|h| h.local));

        assert_eq!(remove("guitest/finder-one@1.0.0").unwrap(), "guitest/finder-one@1.0.0");
        assert!(found("finder").len() == 1 && remove("guitest/finder-one@1.0.0").is_err());
    }

    #[test]
    fn fork_and_build_work_from_the_page_inputs() {
        let _g = lock();
        let original = capsule("forkable", "", "");
        let dir = home().join("forks-out").join("mine");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(fork("guitest/forkable@1.0.0", "other/mine", "  ", "", &quiet).unwrap_err().contains("choose a folder"));
        let f = fork("guitest/forkable@1.0.0", "other/mine", &dir.to_string_lossy(), "", &quiet).unwrap();
        assert_eq!((f.name.as_str(), f.version.as_str(), f.parent.as_str(), f.files), ("other/mine", "0.1.0", "guitest/forkable@1.0.0", 1));
        assert!(dir.join("capsule.yaml").is_file() && dir.join("agent/run.sh").is_file());
        // the fork builds, with the same payload and its own identity
        let r = build(&dir.to_string_lossy(), false).unwrap();
        assert_eq!(r.reference, "other/mine@0.1.0");
        assert_ne!(r.digest, original.digest);
        assert!(build(" ", false).unwrap_err().contains("choose the Capsule's folder"));
        assert!(fork("guitest/forkable@1.0.0", "guitest/forkable", &home().join("x").to_string_lossy(), "", &quiet).unwrap_err().contains("name of its own"));
        assert!(default_fork_dir("other/mine").ends_with(r"capsule-src\mine"));
    }

    /// `cargo test -p elpis-gui -- --ignored print_fixtures` with GUI_FIXTURE_OUT set writes the JSON the page
    /// receives, for trying the page against a mock backend in a browser.
    #[test]
    #[ignore]
    fn print_fixtures() {
        let _g = lock();
        let a = capsule("reviewer", ", description: \"Reviews a diff and writes comments\"", "  requests:\n    network: { allow: [api.anthropic.com, \"*.npmjs.org\"] }\n    credentials: [{ service: anthropic, auth: [apiKey, oauth] }]\n  interfaces: { workspace: { path: /work, access: rw } }\n");
        let b = capsule("notes", ", description: \"Drafts release notes from commits\"", "");
        capsule("hostile", ", description: \"<img src=x onerror=alert(1)> \\e[2J\"", "");
        let (alice, public) = capsulesign::generate().unwrap();
        sign_with(&store(), &a, &alice).unwrap();
        let (mallory, _) = capsulesign::generate().unwrap();
        sign_with(&store(), &b, &mallory).unwrap();
        trust_add("alice", &capsulesign::public_key_string(&public), "guitest").unwrap();
        let fixture = serde_json::json!({
            "list": list().unwrap(),
            "badges": verify_all().unwrap(),
            "detailReviewer": detail("guitest/reviewer@1.0.0").unwrap(),
            "detailNotes": detail("guitest/notes@1.0.0").unwrap(),
            "detailHostile": detail("guitest/hostile@1.0.0").unwrap(),
            "prompt": prepare_run("guitest/reviewer@1.0.0", false).unwrap(),
            "trust": trust_get().unwrap(),
            "keys": [{ "name": "alice", "publicKey": capsulesign::public_key_string(&capsulesign::generate().unwrap().1) }],
        });
        if let Ok(path) = std::env::var("GUI_FIXTURE_OUT") {
            std::fs::write(path, serde_json::to_string_pretty(&fixture).unwrap()).unwrap();
        }
    }
}
