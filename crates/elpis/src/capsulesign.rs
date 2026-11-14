//! Capsule signatures and verification.
//!
//! A signer has an Ed25519 key pair. The private key lives in Windows Credential Manager (`elpis/capsule-key.NAME`,
//! like the other host secrets) and never leaves the machine; the public key is shared as `ed25519:<base64>`.
//! Signing covers a *statement*: the Capsule's name, version and digest (which covers the manifest and the
//! payload), the build's git provenance, and the signing time. The exact statement text is stored with the
//! signature and is what gets verified, so nothing depends on how JSON is serialized.
//!
//! A signature proves that the holder of a key vouched for those bytes. Whether that matters is the consumer's
//! policy: `capsule-trust.json` lists the public keys they trust, each for named namespaces only (a key trusted
//! for `myteam` can't vouch for `othercorp`), and can require a trusted signature before `capsule run`.
//! A key is never trusted because the registry or the signature itself says so.
//!
//! Signatures are stored beside the Capsule (`signatures/KEYID.json`) and in the registry under the tag
//! `sha256-<capsule digest>.sig` (see `capsuleremote.rs`).

use crate::b64;
use crate::capsule::validate_capsule_name;
use crate::capsulestore::{Built, Provenance, Store};
use crate::session::debug_root;
use anyhow::{Context, Result, bail};
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const STATEMENT_KIND: &str = "elpis.capsule.signature/v1";
/// Prefixed to every signed message, so a signature made here can't be replayed as something else
const DOMAIN: &[u8] = b"elpis-capsule-signature-v1\n";
const KEY_SERVICE_PREFIX: &str = "capsule-key.";
const PUBLIC_KEY_PREFIX: &str = "ed25519:";

/// What a signer vouches for. Serialized once, at signing; verifiers parse the stored text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Statement {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub version: String,
    /// The Capsule's digest (manifest and payload)
    pub digest: String,
    /// Where the signer built it from, as recorded by `capsule build`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Provenance>,
    /// Seconds since the Unix epoch; informational
    pub signed_at: u64,
}

/// One signature: the statement text, the signer's public key and the signature over `DOMAIN + statement`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SignatureRecord {
    pub statement: String,
    /// First 8 bytes of the public key's SHA-256, in hex; names the file and the registry layer
    pub key_id: String,
    pub public_key: String,
    pub signature: String,
}

// ---------------------------------------------------------------- keys

pub fn public_key_string(public: &[u8]) -> String {
    format!("{PUBLIC_KEY_PREFIX}{}", b64::encode(public))
}

/// Parse `ed25519:<base64 of 32 bytes>`.
pub fn parse_public_key(text: &str) -> Result<Vec<u8>> {
    let b = text.trim().strip_prefix(PUBLIC_KEY_PREFIX).with_context(|| format!("a public key looks like {PUBLIC_KEY_PREFIX}<base64>"))?;
    let bytes = b64::decode(b).context("the public key isn't base64")?;
    if bytes.len() != 32 {
        bail!("an Ed25519 public key is 32 bytes; this one is {}", bytes.len());
    }
    Ok(bytes)
}

pub fn key_id(public: &[u8]) -> String {
    Sha256::digest(public)[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// A key id is 16 lowercase hex digits. It becomes a file name, so anything else is refused.
pub fn is_key_id(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A new key pair: (PKCS#8 private key, public key).
pub fn generate() -> Result<(Vec<u8>, Vec<u8>)> {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| anyhow::anyhow!("the system couldn't generate random bytes"))?;
    let public = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|e| anyhow::anyhow!("{e}"))?.public_key().as_ref().to_vec();
    Ok((pkcs8.as_ref().to_vec(), public))
}

fn key_pair(pkcs8: &[u8]) -> Result<Ed25519KeyPair> {
    Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| anyhow::anyhow!("the stored signing key is damaged: {e}"))
}

fn check_key_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 40 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("key names are letters, digits, '-' and '_' (max 40); got {name:?}");
    }
    Ok(())
}

/// Make a signing key and keep its private half in Credential Manager. Returns the public key to share.
pub fn create_key(name: &str) -> Result<String> {
    check_key_name(name)?;
    let service = format!("{KEY_SERVICE_PREFIX}{name}");
    if crate::secrets::get(&service).is_some() {
        bail!("a signing key named {name} already exists; pick another name, or remove it with `elpis-run secret rm {service}`");
    }
    let (pkcs8, public) = generate()?;
    crate::secrets::set(&service, &b64::encode(&pkcs8))?;
    Ok(public_key_string(&public))
}

/// The private key named `name`, from Credential Manager.
pub fn load_key(name: &str) -> Result<Vec<u8>> {
    check_key_name(name)?;
    let stored = crate::secrets::get(&format!("{KEY_SERVICE_PREFIX}{name}")).with_context(|| format!("no signing key named {name}; `elpis-run capsule keygen {name}` makes one"))?;
    b64::decode(stored.trim()).context("the stored signing key is damaged")
}

/// Signing keys on this machine with their public keys.
pub fn keys() -> Vec<(String, String)> {
    crate::secrets::list()
        .into_iter()
        .filter_map(|s| s.strip_prefix(KEY_SERVICE_PREFIX).map(str::to_owned))
        .filter_map(|n| {
            let pkcs8 = load_key(&n).ok()?;
            Some((n, public_key_string(key_pair(&pkcs8).ok()?.public_key().as_ref())))
        })
        .collect()
}

// ---------------------------------------------------------------- signing and checking one signature

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Sign the built Capsule with a private key.
pub fn sign(pkcs8: &[u8], built: &Built) -> Result<SignatureRecord> {
    sign_at(pkcs8, built, now())
}

fn sign_at(pkcs8: &[u8], built: &Built, signed_at: u64) -> Result<SignatureRecord> {
    let kp = key_pair(pkcs8)?;
    let statement = serde_json::to_string(&Statement {
        kind: STATEMENT_KIND.into(),
        name: built.name.clone(),
        version: built.version.clone(),
        digest: built.digest.clone(),
        source: built.provenance.clone(),
        signed_at,
    })?;
    let public = kp.public_key().as_ref();
    let signature = kp.sign(&message(&statement));
    Ok(SignatureRecord { statement, key_id: key_id(public), public_key: public_key_string(public), signature: b64::encode(signature.as_ref()) })
}

fn message(statement: &str) -> Vec<u8> {
    [DOMAIN, statement.as_bytes()].concat()
}

/// Check one signature against the Capsule it is supposed to cover: the key id and public key agree, the
/// signature is valid over the stored statement, and the statement names exactly this name, version and digest.
/// Says nothing about whether the key is trusted.
pub fn verify_record(rec: &SignatureRecord, name: &str, version: &str, digest: &str) -> Result<Statement> {
    let public = parse_public_key(&rec.public_key)?;
    if key_id(&public) != rec.key_id {
        bail!("the key id doesn't match the public key");
    }
    let signature = b64::decode(&rec.signature).context("the signature isn't base64")?;
    UnparsedPublicKey::new(&ED25519, &public).verify(&message(&rec.statement), &signature).map_err(|_| anyhow::anyhow!("the signature doesn't match its statement"))?;
    let statement: Statement = serde_json::from_str(&rec.statement).context("the signed statement isn't understood")?;
    if statement.kind != STATEMENT_KIND {
        bail!("the signed statement is a {:?}, not a Capsule signature", statement.kind);
    }
    if (statement.name.as_str(), statement.version.as_str(), statement.digest.as_str()) != (name, version, digest) {
        bail!("it signs {}@{} ({}), not this Capsule", statement.name, statement.version, crate::capsulestore::short(&statement.digest));
    }
    Ok(statement)
}

// ---------------------------------------------------------------- the consumer's trust

/// A public key the consumer trusts, for named namespaces only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TrustedKey {
    /// A label for reports
    pub name: String,
    pub public_key: String,
    /// Capsule namespaces this key may vouch for; `*` is every namespace
    pub namespaces: Vec<String>,
}

impl TrustedKey {
    pub fn covers(&self, capsule_name: &str) -> bool {
        let ns = capsule_name.split('/').next().unwrap_or_default();
        self.namespaces.iter().any(|n| n == "*" || n == ns)
    }
}

/// `capsule-trust.json` in the data folder: the consumer's signing policy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Trust {
    /// `capsule run` refuses a Capsule without a signature from a trusted key (override with --allow-unsigned)
    #[serde(default)]
    pub require_signed: bool,
    #[serde(default)]
    pub keys: Vec<TrustedKey>,
}

impl Trust {
    pub fn path() -> PathBuf {
        debug_root().join("capsule-trust.json")
    }

    /// The policy on this machine. A missing file is the default (nothing trusted, signatures not required);
    /// a damaged one is an error rather than a silent reset, so a broken file can't quietly relax the policy.
    pub fn load() -> Result<Trust> {
        Trust::load_from(&Trust::path())
    }

    pub fn load_from(path: &Path) -> Result<Trust> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("{} is damaged; fix or delete it (deleting it resets your trust policy)", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Trust::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Trust::path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }

    /// Trust `public_key` for `namespaces`.
    pub fn add(&mut self, name: &str, public_key: &str, namespaces: &[String]) -> Result<()> {
        check_key_name(name)?;
        let canonical = public_key_string(&parse_public_key(public_key)?);
        if namespaces.is_empty() {
            bail!("say which namespaces this key may vouch for (--namespace myteam), or --namespace '*' for all of them");
        }
        for ns in namespaces {
            if ns != "*" {
                validate_capsule_name(&format!("{ns}/x")).map_err(|_| anyhow::anyhow!("{ns:?} isn't a namespace (lowercase letters, digits and hyphens) or *"))?;
            }
        }
        if self.keys.iter().any(|k| k.name == name) {
            bail!("a trusted key named {name} exists; remove it first (`capsule trust rm {name}`)");
        }
        if let Some(k) = self.keys.iter().find(|k| k.public_key == canonical) {
            bail!("that key is already trusted as {}", k.name);
        }
        self.keys.push(TrustedKey { name: name.to_owned(), public_key: canonical, namespaces: namespaces.to_vec() });
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.keys.len();
        self.keys.retain(|k| k.name != name);
        self.keys.len() != before
    }

    fn find(&self, public_key: &str) -> Option<&TrustedKey> {
        let canonical = parse_public_key(public_key).ok().map(|p| public_key_string(&p))?;
        self.keys.iter().find(|k| k.public_key == canonical)
    }
}

// ---------------------------------------------------------------- verifying a stored Capsule

#[derive(Clone, Debug, PartialEq)]
pub enum Standing {
    /// Valid, and by a key the consumer trusts for this namespace (the key's label)
    Trusted(String),
    /// Valid, by a trusted key, but not for this Capsule's namespace
    OutOfScope(String),
    /// Valid, by a key the consumer hasn't chosen to trust
    Unknown,
    /// Not a valid signature for this Capsule (why)
    Invalid(String),
}

#[derive(Clone, Debug)]
pub struct SignatureReport {
    pub key_id: String,
    pub public_key: String,
    pub standing: Standing,
    pub statement: Option<Statement>,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: &'static str,
    /// What was found, or what is wrong
    pub result: Result<String, String>,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub reference: String,
    pub digest: String,
    pub checks: Vec<Check>,
    pub signatures: Vec<SignatureReport>,
    /// Where it was built from: the local build's record, else the first valid signature's
    pub provenance: Option<Provenance>,
}

impl Report {
    /// The stored files match what was built and pass the payload checks.
    pub fn intact(&self) -> bool {
        self.checks.iter().all(|c| c.result.is_ok())
    }

    pub fn trusted_signers(&self) -> Vec<&str> {
        self.signatures.iter().filter_map(|s| if let Standing::Trusted(n) = &s.standing { Some(n.as_str()) } else { None }).collect()
    }

    pub fn is_trusted(&self) -> bool {
        self.intact() && !self.trusted_signers().is_empty()
    }
}

/// Check a stored Capsule: its files against the build record and the payload rules, and every signature
/// against the consumer's trust.
pub fn verify(store: &Store, built: &Built, trust: &Trust) -> Report {
    let mut checks = Vec::new();
    match store.load(built) {
        Ok(capsule) => {
            checks.push(Check { name: "manifest and payload match the build record", result: Ok(crate::capsulestore::short(&built.digest).to_owned()) });
            let archive = match built.payload_path() {
                None => Ok("no payload".to_owned()),
                Some(path) => std::fs::read(&path)
                    .map_err(|e| format!("reading {}: {e}", path.display()))
                    .and_then(|gz| crate::capsulestore::validate_payload_archive(&capsule, &gz).map_err(|e| format!("{e:#}")))
                    .map(|files| format!("{files} file(s), all inside the declared targets")),
            };
            checks.push(Check { name: "payload archive is plain files inside its targets", result: archive });
        }
        Err(e) => checks.push(Check { name: "manifest and payload match the build record", result: Err(format!("{e:#}")) }),
    }

    let signatures: Vec<SignatureReport> = store
        .signatures(built)
        .into_iter()
        .map(|rec| match verify_record(&rec, &built.name, &built.version, &built.digest) {
            Err(e) => SignatureReport { key_id: rec.key_id.clone(), public_key: rec.public_key.clone(), standing: Standing::Invalid(format!("{e:#}")), statement: None },
            Ok(statement) => {
                let standing = match trust.find(&rec.public_key) {
                    None => Standing::Unknown,
                    Some(k) if k.covers(&built.name) => Standing::Trusted(k.name.clone()),
                    Some(k) => Standing::OutOfScope(k.name.clone()),
                };
                SignatureReport { key_id: rec.key_id.clone(), public_key: rec.public_key.clone(), standing, statement: Some(statement) }
            }
        })
        .collect();
    let provenance = built.provenance.clone().or_else(|| signatures.iter().find_map(|s| s.statement.as_ref().and_then(|st| st.source.clone())));
    Report { reference: built.reference(), digest: built.digest.clone(), checks, signatures, provenance }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("elpis-sign-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn built_capsule(tag: &str, version: &str, ns: &str) -> (Store, Built) {
        let src = temp(&format!("{tag}-src"));
        std::fs::create_dir_all(src.join("agent")).unwrap();
        std::fs::write(src.join("agent/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(
            src.join("capsule.yaml"),
            format!("apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: {ns}/tool, version: {version} }}\nspec:\n  runtime: {{ image: {DIGEST}, entrypoint: [run] }}\n  payload: [{{ from: ./agent, to: /opt/agent }}]\n"),
        )
        .unwrap();
        let store = Store::at(temp(&format!("{tag}-store")));
        let built = store.build(&src, false).unwrap();
        (store, built)
    }

    fn signer() -> (Vec<u8>, String) {
        let (pkcs8, public) = generate().unwrap();
        (pkcs8, public_key_string(&public))
    }

    #[test]
    fn a_signature_verifies_for_exactly_its_capsule() {
        let (_, built) = built_capsule("ok", "1.0.0", "myteam");
        let (key, public) = signer();
        let rec = sign(&key, &built).unwrap();
        assert_eq!(rec.public_key, public);
        assert!(is_key_id(&rec.key_id));
        let st = verify_record(&rec, &built.name, &built.version, &built.digest).unwrap();
        assert_eq!((st.name.as_str(), st.version.as_str(), st.kind.as_str()), ("myteam/tool", "1.0.0", STATEMENT_KIND));

        // not another version, digest or name
        assert!(verify_record(&rec, &built.name, "1.0.1", &built.digest).unwrap_err().to_string().contains("not this Capsule"));
        assert!(verify_record(&rec, &built.name, &built.version, "sha256:other").is_err());
        assert!(verify_record(&rec, "other/tool", &built.version, &built.digest).is_err());
    }

    #[test]
    fn a_tampered_signature_is_invalid() {
        let (_, built) = built_capsule("tamper", "1.0.0", "myteam");
        let (key, _) = signer();
        let good = sign(&key, &built).unwrap();

        let mut edited = good.clone();
        edited.statement = edited.statement.replace("1.0.0", "1.0.1");
        assert!(verify_record(&edited, &built.name, "1.0.1", &built.digest).unwrap_err().to_string().contains("doesn't match its statement"));

        let mut other_key = good.clone();
        let (_, other_public) = signer();
        other_key.public_key = other_public;
        assert!(verify_record(&other_key, &built.name, &built.version, &built.digest).is_err(), "a different key can't claim the signature");

        let mut wrong_id = good.clone();
        wrong_id.key_id = "0000000000000000".into();
        assert!(verify_record(&wrong_id, &built.name, &built.version, &built.digest).unwrap_err().to_string().contains("key id"));

        let mut bad_sig = good.clone();
        bad_sig.signature = b64::encode(&[7u8; 64]);
        assert!(verify_record(&bad_sig, &built.name, &built.version, &built.digest).is_err());
        let mut junk = good;
        junk.signature = "!!".into();
        assert!(verify_record(&junk, &built.name, &built.version, &built.digest).is_err());
    }

    #[test]
    fn a_signature_is_bound_to_this_programs_domain() {
        // the same bytes signed without the domain prefix don't verify
        let (_, built) = built_capsule("domain", "1.0.0", "myteam");
        let (pkcs8, _) = signer();
        let kp = Ed25519KeyPair::from_pkcs8(&pkcs8).unwrap();
        let mut rec = sign(&pkcs8, &built).unwrap();
        rec.signature = b64::encode(kp.sign(rec.statement.as_bytes()).as_ref());
        assert!(verify_record(&rec, &built.name, &built.version, &built.digest).is_err());
    }

    #[test]
    fn public_keys_parse_strictly() {
        let (_, public) = signer();
        assert_eq!(public_key_string(&parse_public_key(&public).unwrap()), public);
        for bad in ["", "ed25519:", "rsa:AAAA", "ed25519:AAAA", &format!("ed25519:{}", b64::encode(&[0u8; 33]))] {
            assert!(parse_public_key(bad).is_err(), "{bad:?}");
        }
        assert!(is_key_id("0123456789abcdef"));
        for bad in ["", "0123456789ABCDEF", "../../etc/passwd", "0123456789abcde", "0123456789abcdeg"] {
            assert!(!is_key_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn trust_is_scoped_to_namespaces() {
        let (_, public) = signer();
        let mut trust = Trust::default();
        assert!(trust.add("alice", &public, &[]).is_err(), "a key needs explicit namespaces");
        assert!(trust.add("alice", &public, &["My Team".into()]).is_err());
        assert!(trust.add("alice", "ed25519:nope", &["myteam".into()]).is_err());
        trust.add("alice", &public, &["myteam".into()]).unwrap();
        assert!(trust.add("alice", &public, &["myteam".into()]).is_err());
        assert!(trust.add("alice2", &public, &["myteam".into()]).unwrap_err().to_string().contains("already trusted as alice"));

        let k = &trust.keys[0];
        assert!(k.covers("myteam/tool") && !k.covers("othercorp/tool") && !k.covers("myteamx/tool"));
        let all = TrustedKey { namespaces: vec!["*".into()], ..k.clone() };
        assert!(all.covers("anything/at-all"));
        assert!(trust.remove("alice") && !trust.remove("alice"));
    }

    #[test]
    fn the_trust_file_round_trips_and_a_damaged_one_fails_closed() {
        let dir = temp("trustfile");
        let path = dir.join("capsule-trust.json");
        assert_eq!(Trust::load_from(&path).unwrap(), Trust::default(), "missing means the default policy");
        let (_, public) = signer();
        let mut t = Trust { require_signed: true, ..Default::default() };
        t.add("alice", &public, &["myteam".into()]).unwrap();
        t.save_to(&path).unwrap();
        assert_eq!(Trust::load_from(&path).unwrap(), t);
        std::fs::write(&path, "{ not json").unwrap();
        let e = format!("{:#}", Trust::load_from(&path).unwrap_err());
        assert!(e.contains("damaged"), "{e}");
        std::fs::write(&path, r#"{"requireSigned":true,"bogus":1}"#).unwrap();
        assert!(Trust::load_from(&path).is_err(), "unknown fields are refused too");
    }

    #[test]
    fn verify_reports_each_signatures_standing() {
        let (store, built) = built_capsule("report", "1.0.0", "myteam");
        let ((alice, alice_pub), (mallory, _), (bob, bob_pub), (carol, _)) = (signer(), signer(), signer(), signer());
        for key in [&alice, &mallory, &bob] {
            store.add_signature(&built, &sign(key, &built).unwrap()).unwrap();
        }
        // a signature made for a different Capsule, stored under this one
        let (_, other) = built_capsule("report-other", "1.0.0", "otherns");
        let foreign = sign(&carol, &other).unwrap();
        store.add_signature(&built, &foreign).unwrap();

        let mut trust = Trust::default();
        trust.add("alice", &alice_pub, &["myteam".into()]).unwrap();
        trust.add("bob", &bob_pub, &["othercorp".into()]).unwrap();
        let report = verify(&store, &built, &trust);

        assert!(report.intact());
        let standing = |id: &str| report.signatures.iter().find(|s| s.key_id == id).unwrap().standing.clone();
        assert_eq!(standing(&key_id(&parse_public_key(&alice_pub).unwrap())), Standing::Trusted("alice".into()));
        assert_eq!(standing(&key_id(&parse_public_key(&bob_pub).unwrap())), Standing::OutOfScope("bob".into()));
        assert!(matches!(standing(&foreign.key_id), Standing::Invalid(_)));
        assert_eq!(report.signatures.iter().filter(|s| s.standing == Standing::Unknown).count(), 1, "mallory is valid but not trusted");
        assert_eq!(report.trusted_signers(), ["alice"]);
        assert!(report.is_trusted());
    }

    #[test]
    fn unsigned_and_untrusted_capsules_are_not_trusted() {
        let (store, built) = built_capsule("unsigned", "1.0.0", "myteam");
        let report = verify(&store, &built, &Trust::default());
        assert!(report.intact() && report.signatures.is_empty() && !report.is_trusted());
        let (key, _) = signer();
        store.add_signature(&built, &sign(&key, &built).unwrap()).unwrap();
        assert!(!verify(&store, &built, &Trust::default()).is_trusted(), "a valid signature from an unknown key proves nothing");
    }

    #[test]
    fn a_modified_store_fails_the_integrity_check_even_when_signed_and_trusted() {
        let (store, built) = built_capsule("modified", "1.0.0", "myteam");
        let (key, public) = signer();
        store.add_signature(&built, &sign(&key, &built).unwrap()).unwrap();
        let mut trust = Trust::default();
        trust.add("alice", &public, &["myteam".into()]).unwrap();
        assert!(verify(&store, &built, &trust).is_trusted());

        std::fs::write(built.payload_path().unwrap(), b"swapped").unwrap();
        let report = verify(&store, &built, &trust);
        assert!(!report.intact() && !report.is_trusted());
        assert!(report.checks[0].result.as_ref().unwrap_err().contains("modified"));
    }

    #[test]
    fn stored_signatures_are_deduplicated_and_names_are_checked() {
        let (store, built) = built_capsule("store", "1.0.0", "myteam");
        let (key, _) = signer();
        let rec = sign(&key, &built).unwrap();
        store.add_signature(&built, &rec).unwrap();
        store.add_signature(&built, &rec).unwrap();
        assert_eq!(store.signatures(&built), vec![rec.clone()]);
        // a key id becomes a file name: nothing but 16 hex digits is accepted
        let evil = SignatureRecord { key_id: "../../evil".into(), ..rec };
        assert!(store.add_signature(&built, &evil).is_err());
        assert_eq!(store.signatures(&built).len(), 1);
    }

    #[test]
    fn provenance_is_carried_in_the_signed_statement() {
        let (_, mut built) = built_capsule("prov", "1.0.0", "myteam");
        built.provenance = Some(Provenance { git: Some("https://example.com/team/tool.git".into()), commit: Some("abc123".into()), dirty: true });
        let (key, _) = signer();
        let rec = sign(&key, &built).unwrap();
        let st = verify_record(&rec, &built.name, &built.version, &built.digest).unwrap();
        assert_eq!(st.source, built.provenance);
        assert!(rec.statement.contains("\"dirty\":true"));
    }
}
