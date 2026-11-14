//! `elpis-shim mkfs`: make filesystems in a tools VM (our guest's mkfs.erofs and mke2fs), so the host needs
//! no Windows ports of erofs-utils or e2fsprogs.
//!
//!   elpis-shim mkfs erofs LAYER.tar OUT.erofs     an OCI layer tar → an erofs image (whiteouts as overlayfs)
//!   elpis-shim mkfs ext4 DISK.img LABEL           format an existing (sparse) disk file as journaled ext4

use crate::vm::{self, Disk};
use anyhow::{Context, Result, bail};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const EROFS_MAGIC: u32 = 0xE0F5_E1E2;

pub fn main(args: &[String]) -> Result<()> {
    match args {
        [mode, tar, out] if mode == "erofs" => erofs(Path::new(tar), Path::new(out)),
        [mode, disk, label] if mode == "ext4" => {
            let disk = std::path::absolute(disk)?;
            in_vm(&["ext4".into(), label.clone()], &[Disk { path: disk.clone(), read_only: false, vmdk: false }], &disk)
        }
        _ => bail!("usage: elpis-shim mkfs erofs LAYER.tar OUT.erofs | mkfs ext4 DISK.img LABEL"),
    }
}

fn erofs(tar: &Path, out: &Path) -> Result<()> {
    let (tar, out) = (std::path::absolute(tar)?, std::path::absolute(out)?);
    let tar_len = std::fs::metadata(&tar).with_context(|| format!("reading {}", tar.display()))?.len();
    // Room for the image: erofs pads file data to blocks, so it can outgrow the tar; sparse, trimmed after
    sparse_file(&out, tar_len.saturating_mul(4) + (256 << 20))?;
    let disks = [Disk { path: tar, read_only: true, vmdk: false }, Disk { path: out.clone(), read_only: false, vmdk: false }];
    in_vm(&["erofs".into()], &disks, &out)?;
    let size = erofs_size(&out)?;
    std::fs::OpenOptions::new().write(true).open(&out)?.set_len(size)?;
    Ok(())
}

/// The image's size from its superblock (at byte 1024: magic, …, blkszbits at +12, blocks at +36).
fn erofs_size(path: &Path) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let mut sb = [0u8; 128];
    f.seek(SeekFrom::Start(1024))?;
    f.read_exact(&mut sb)?;
    if u32::from_le_bytes(sb[0..4].try_into()?) != EROFS_MAGIC {
        bail!("{} has no erofs superblock", path.display());
    }
    let (bits, blocks) = (u32::from(sb[12]), u64::from(u32::from_le_bytes(sb[36..40].try_into()?)));
    Ok(blocks << bits)
}

fn sparse_file(path: &Path, len: u64) -> Result<()> {
    std::fs::File::create(path)?;
    let _ = std::process::Command::new("fsutil").args(["sparse", "setflag"]).arg(path).creation_flags(CREATE_NO_WINDOW).output();
    std::fs::OpenOptions::new().write(true).open(path)?.set_len(len)?;
    Ok(())
}

/// Boots the tools VM in a child process (`mkfs-vm`) and checks the result line it prints on the console.
fn in_vm(args: &[String], disks: &[Disk], near: &Path) -> Result<()> {
    let console = PathBuf::from(format!("{}.mkfs-console.log", near.display()));
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("mkfs-vm").arg(&console).arg(disks.len().to_string());
    for d in disks {
        cmd.arg(if d.read_only { "ro" } else { "rw" }).arg(&d.path);
    }
    cmd.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn().context("starting the tools VM")?;
    let clock = std::time::Instant::now();
    while child.try_wait()?.is_none() {
        if clock.elapsed() > Duration::from_secs(30 * 60) {
            let _ = child.kill();
            bail!("the tools VM didn't finish in 30 minutes");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = std::fs::read_to_string(&console).unwrap_or_default();
    let _ = std::fs::remove_file(&console);
    match text.lines().rev().find_map(|l| l.split_once("ELPIS-MKFS: ").map(|(_, rest)| rest.trim())) {
        Some("OK") => Ok(()),
        Some(failed) => bail!("{failed}"),
        None => {
            let tail: String = text.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
            bail!("the tools VM gave no result (console ends: {tail:?})")
        }
    }
}

/// `elpis-shim mkfs-vm CONSOLE N ro|rw DISK… ARGS…` (internal): runs the tools VM in this process.
pub fn vm_main(args: &[String]) -> Result<()> {
    let console = PathBuf::from(args.first().context("no console path")?);
    let n: usize = args.get(1).context("no disk count")?.parse()?;
    let mut disks = Vec::new();
    for i in 0..n {
        let mode = args.get(2 + 2 * i).context("missing disk mode")?;
        let path = args.get(3 + 2 * i).context("missing disk path")?;
        disks.push(Disk { path: PathBuf::from(path), read_only: mode == "ro", vmdk: false });
    }
    let rest = args.get(2 + 2 * n..).unwrap_or_default().to_vec();
    vm::run_tool(&rest, &disks, &console)?;
    Ok(())
}
