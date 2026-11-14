//! Making filesystems: erofs images from layer tars, and ext4 disks.
//!
//! With elpis's own guest installed (`elpis-rootfs-x86_64.erofs` beside `elpis-shim.exe`, from
//! `scripts/build-guest.sh --install`) they're made in a tools VM (`elpis-shim mkfs`), which needs nothing
//! else on the host. Otherwise Docker Sandboxes' `mkfs.erofs.exe` and `mkfs.ext4.exe` do it.
//! `ELPIS_MKFS=vm|sbx` picks one explicitly.

use anyhow::{Context, Result, bail};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

enum Tool {
    /// elpis-shim.exe
    Vm(PathBuf),
    /// Docker Sandboxes' libexec folder
    Sbx(PathBuf),
}

fn tool() -> Tool {
    let shim = crate::session::elpis_shim();
    match std::env::var("ELPIS_MKFS").as_deref() {
        Ok("vm") => Tool::Vm(shim),
        Ok("sbx") => Tool::Sbx(crate::session::sbx_libexec()),
        _ if crate::session::own_guest_installed() => Tool::Vm(shim),
        _ => Tool::Sbx(crate::session::sbx_libexec()),
    }
}

fn run(mut cmd: Command, what: &str) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.creation_flags(CREATE_NO_WINDOW).output().with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{what} failed: {}{}", String::from_utf8_lossy(&out.stderr).trim(), String::from_utf8_lossy(&out.stdout).trim());
    }
    Ok(())
}

/// An OCI layer tar → an erofs image at `out`, with `.wh.` whiteouts turned into overlayfs ones.
pub fn erofs_from_tar(tar: &Path, out: &Path) -> Result<()> {
    match tool() {
        Tool::Vm(shim) => {
            let mut cmd = Command::new(shim);
            cmd.args(["mkfs", "erofs"]).arg(tar).arg(out);
            run(cmd, "mkfs.erofs (tools VM)")
        }
        Tool::Sbx(libexec) => {
            let mut cmd = Command::new(libexec.join("mkfs.erofs.exe"));
            cmd.args(["--tar=f", "--aufs", "--quiet", "-Enoinline_data"]).arg(out).arg(tar);
            run(cmd, "mkfs.erofs")
        }
    }
}

/// Formats `disk` (an existing, sized file) as journaled ext4.
pub fn ext4(disk: &Path, label: &str) -> Result<()> {
    match tool() {
        Tool::Vm(shim) => {
            let mut cmd = Command::new(shim);
            cmd.args(["mkfs", "ext4"]).arg(disk).arg(label);
            run(cmd, "mkfs.ext4 (tools VM)")
        }
        Tool::Sbx(libexec) => {
            let mut cmd = Command::new(libexec.join("mkfs.ext4.exe"));
            cmd.args(["-F", "-q", "-j", "-L", label]).arg(disk);
            run(cmd, "mkfs.ext4")
        }
    }
}
