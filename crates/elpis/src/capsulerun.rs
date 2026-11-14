//! Making a box from a built Capsule: shared by `elpis-run capsule run` and the TUI.
//!
//! The steps are the same everywhere: load the Capsule (checking its files against the build), verify it against
//! the consumer's trust policy, show what it asks for and let the user decide what to grant, then pull its
//! image, turn its payload into a layer and create the box with the grants as its whole network policy. This
//! module does the work around the decision; the decision itself (and any prompt) belongs to the caller.

use crate::boxfile::{BoxFile, PortMap};
use crate::capsule::{Capsule, Grants};
use crate::capsulesign::{Report, Trust, verify};
use crate::capsulestore::{Built, Store};
use crate::workdir::{self, CapsuleLaunch, NewBox, Source};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// A Capsule that is loaded, intact and admitted by the consumer's trust policy.
pub struct Prepared {
    pub built: Built,
    pub capsule: Capsule,
    /// How it verified: for showing who vouches for it
    pub report: Report,
}

impl Prepared {
    /// Everything it asks for that can be granted: what "yes, grant these" accepts.
    pub fn requested(&self) -> Grants {
        self.capsule.spec.requests.grantable()
    }
}

/// Load `built`, check its files against the build, verify it under the trust policy on this machine, and check
/// it is compatible with this Elpis. Fails if the policy refuses it (see [`Trust::admit`]).
pub fn prepare(store: &Store, built: &Built, allow_unsigned: bool) -> Result<Prepared> {
    let trust = Trust::load()?;
    prepare_with(store, built, &trust, allow_unsigned)
}

pub fn prepare_with(store: &Store, built: &Built, trust: &Trust, allow_unsigned: bool) -> Result<Prepared> {
    let capsule = store.load(built)?;
    let report = verify(store, built, trust);
    trust.admit(&report, allow_unsigned)?;
    if !capsule.compatible_with(env!("CARGO_PKG_VERSION"))? {
        bail!("{} needs a newer Elpis ({})", capsule.reference(), capsule.spec.compatibility.elpis.as_deref().unwrap_or_default());
    }
    Ok(Prepared { built: built.clone(), capsule, report })
}

/// The folder a Capsule's workspace is shared from: the given one, else `default` (the current folder), for a
/// Capsule that declares a workspace; None for one that doesn't, which refuses a folder.
pub fn workspace_for(capsule: &Capsule, given: Option<&Path>, default: &Path) -> Result<Option<PathBuf>> {
    match (given, capsule.spec.interfaces.workspace.is_some()) {
        (Some(w), true) => Ok(Some(std::path::absolute(w)?)),
        (None, true) => Ok(Some(default.to_owned())),
        (Some(_), false) => bail!("{} doesn't declare a workspace, so there is no folder to share; leave the folder out", capsule.reference()),
        (None, false) => Ok(None),
    }
}

/// A box name from the parts: letters, digits and hyphens, at most 40 characters.
pub fn box_name(parts: &[&str]) -> String {
    let joined: String = parts.join("-").chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    joined.chars().take(40).collect::<String>().trim_matches('-').to_owned()
}

/// `CAPSULE-FOLDER`, like `elpis-run run` names its boxes.
pub fn default_box_name(capsule: &Capsule, workspace: Option<&Path>) -> String {
    let short = capsule.metadata.name.rsplit('/').next().unwrap_or(&capsule.metadata.name);
    let folder = workspace.and_then(|w| w.file_name()).map(|f| f.to_string_lossy().into_owned());
    box_name(&[short, folder.as_deref().unwrap_or("")])
}

/// What the user chose for the new box.
pub struct Options {
    pub name: String,
    pub workspace: Option<PathBuf>,
    /// Host ports for the Capsule's declared ports
    pub ports: Vec<PortMap>,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u64>,
    pub auto_stop: Option<u64>,
    /// What the user granted: the box's network policy is exactly this
    pub grants: Grants,
}

/// Pull the image, make the payload layer and create the box. Returns its folder. Blocking: callers run it
/// off their UI thread.
pub fn create_box(store: &Store, prepared: Prepared, opts: Options, progress: &dyn Fn(&str)) -> Result<PathBuf> {
    let Prepared { built, capsule, .. } = prepared;
    capsule.to_boxfile(&opts.ports)?; // a published port has to be one the Capsule declares
    let image = crate::registry::pull(&capsule.spec.runtime.image, progress)?;
    let payload_layer = store.payload_layer(&built)?;
    progress(&format!("creating box {} from {}", opts.name, capsule.reference()));
    let launch = CapsuleLaunch { capsule, digest: built.digest.clone(), image, payload_layer, grants: opts.grants };
    let new = NewBox { workspace: opts.workspace, cpus: opts.cpus, memory_mb: opts.memory_mb, ports: opts.ports, ..NewBox::new(opts.name, Source::Capsule(Box::new(launch))) };
    let work = workdir::create_box(new)?;
    if let Some(secs) = opts.auto_stop {
        let mut file = BoxFile::load_or_default(&work);
        file.auto_stop = Some(secs);
        file.save(&work)?;
    }
    Ok(work)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn capsule(extra: &str) -> Capsule {
        Capsule::parse(&format!("apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: myteam/code-reviewer, version: 1.0.0 }}\nspec:\n  runtime: {{ image: {DIGEST}, entrypoint: [run] }}\n{extra}")).unwrap()
    }

    #[test]
    fn box_names_are_safe_and_bounded() {
        assert_eq!(box_name(&["code-reviewer", "my project"]), "code-reviewer-my-project");
        assert_eq!(box_name(&["a", ""]), "a");
        assert_eq!(box_name(&["x".repeat(60).as_str()]).len(), 40);
        assert_eq!(box_name(&["!!", "??"]), "");
        let c = capsule("");
        assert_eq!(default_box_name(&c, Some(Path::new(r"C:\code\My App"))), "code-reviewer-My-App");
        assert_eq!(default_box_name(&c, None), "code-reviewer");
    }

    #[test]
    fn a_workspace_goes_only_to_a_capsule_that_declares_one() {
        let with = capsule("  interfaces:\n    workspace: { path: /work }\n");
        let without = capsule("");
        let cwd = Path::new(r"C:\here");
        assert_eq!(workspace_for(&with, None, cwd).unwrap(), Some(PathBuf::from(r"C:\here")), "defaults to the current folder");
        assert!(workspace_for(&with, Some(Path::new(r"C:\code")), cwd).unwrap().is_some());
        assert_eq!(workspace_for(&without, None, cwd).unwrap(), None);
        assert!(workspace_for(&without, Some(Path::new(r"C:\code")), cwd).unwrap_err().to_string().contains("doesn't declare a workspace"));
    }

    #[test]
    fn what_can_be_granted_is_the_network_and_host_access() {
        let c = capsule("  requests: { network: { allow: [example.com] }, hostAccess: true, credentials: [{ service: anthropic, auth: [apiKey] }] }\n");
        let g = c.spec.requests.grantable();
        assert_eq!((g.network.as_slice(), g.host_access), (["example.com".to_owned()].as_slice(), true));
    }
}
