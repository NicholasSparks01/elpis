//! The VM: libkrun (WHP) in this process, booting the elpis guest kernel straight into vminitd.
//!
//! libkrun's `run()` never returns: when the guest powers off it ends the whole process. That's why there is
//! one shim process per VM, as with sbx.

use anyhow::{Context, Result, bail};
use krun::api::device_builders::{TsiFlags, VsockDevice};
use krun::api::{
    BalloonDevice, BlockDevice, CachePolicy, ConsoleDevice, DiskFormat, FsDevice, KernelFormat, LogLevel, LogOptions, LogStyle,
    MmioDeviceManager, NetDevice, NetFlags, Payload, PciDeviceManager, PermissionSemantics, RngDevice, RtcDevice, VmmBuilder,
    init_log,
};
use socket2::Socket;
use std::os::windows::io::AsHandle;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

pub const RPC_PORT: u32 = 1025;
pub const STREAM_PORT: u32 = 1026;

#[derive(Clone, Debug)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
    pub vmdk: bool,
}

#[derive(Clone, Debug)]
pub struct Share {
    pub tag: String,
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct Nic {
    pub socket: PathBuf,
    pub mac: [u8; 6],
}

pub struct VmSpec {
    pub cpus: u8,
    pub mem_mib: u32,
    pub disks: Vec<Disk>,
    pub shares: Vec<Share>,
    pub nics: Vec<Nic>,
    pub init_args: Vec<String>,
    /// Sockets, console and libkrun logs (`<bundle>\vm`)
    pub state: PathBuf,
}

/// The guest kernel and VM rootfs (vminitd + crun): `ELPIS_KERNEL` / `ELPIS_ROOTFS`, else beside this
/// program, else Docker Sandboxes' copies.
pub fn guest_files() -> Result<(PathBuf, PathBuf)> {
    // Ours are named elpis-*; Docker Sandboxes' copies keep containerd/nerdbox's names
    let find = |var: &str, name: &str, sbx_name: &str| -> Result<PathBuf> {
        if let Some(p) = std::env::var_os(var) {
            return Ok(PathBuf::from(p));
        }
        let beside = std::env::current_exe()?.with_file_name(name);
        if beside.exists() {
            return Ok(beside);
        }
        let sbx = elpis::session::sbx_libexec().join(sbx_name);
        if sbx.exists() {
            return Ok(sbx);
        }
        bail!("can't find {name}: set {var} or put it beside elpis-shim.exe (scripts/build-guest.sh --install)")
    };
    Ok((
        find("ELPIS_KERNEL", "elpis-kernel-x86_64", "nerdbox-kernel-x86_64")?,
        find("ELPIS_ROOTFS", "elpis-rootfs-x86_64.erofs", "nerdbox-rootfs-x86_64.erofs")?,
    ))
}

/// Boots the VM on its own thread and returns once vminitd has connected back on vsock 1025.
pub fn boot(spec: VmSpec, log: &crate::Logger) -> Result<Socket> {
    let (kernel, rootfs) = guest_files()?;
    std::fs::create_dir_all(&spec.state)?;
    let rpc = spec.state.join("run_vminitd.sock");
    let stream = spec.state.join("streaming.sock");
    let _ = std::fs::remove_file(&stream);

    // vminitd dials vsock 1025; libkrun connects that to this socket, where we accept it
    let _ = std::fs::remove_file(&rpc);
    let listener = Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    listener.bind(&socket2::SockAddr::unix(&rpc)?).with_context(|| format!("binding {}", rpc.display()))?;
    listener.listen(1)?;
    let (conn_tx, conn_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = conn_tx.send(listener.accept().map(|(s, _)| s));
    });

    // elpis's own command line; ELPIS_KERNEL_ARGS adds to it (for experiments)
    let mut cmdline = String::from("console=hvc0 root=/dev/vda rootfstype=erofs ro init=/sbin/vminitd");
    if let Ok(extra) = std::env::var("ELPIS_KERNEL_ARGS") {
        cmdline.push(' ');
        cmdline.push_str(&extra);
    }
    cmdline.push_str(&format!(" -- -vsock-rpc-port={RPC_PORT} -vsock-stream-port={STREAM_PORT} -vsock-cid=3"));
    for a in &spec.init_args {
        cmdline.push(' ');
        cmdline.push_str(a);
    }
    log.info("booting the VM", &[
        ("kernel", kernel.display().to_string()),
        ("cpus", spec.cpus.to_string()),
        ("memory_mib", spec.mem_mib.to_string()),
        ("disks", spec.disks.len().to_string()),
        ("shares", spec.shares.len().to_string()),
        ("nics", spec.nics.len().to_string()),
    ]);
    for (i, d) in spec.disks.iter().enumerate() {
        log.debug("disk", &[("dev", format!("vd{}", (b'b' + i as u8) as char)), ("path", d.path.display().to_string()), ("ro", d.read_only.to_string())]);
    }

    let (built_tx, built_rx) = mpsc::channel::<Result<(), String>>();
    let state = spec.state.clone();
    std::thread::Builder::new().name("vmm".into()).spawn(move || {
        let result = build(spec, &kernel, &rootfs, &cmdline, &rpc, &stream, &state);
        match result {
            Ok(vmm) => {
                let _ = built_tx.send(Ok(()));
                vmm.run(); // never returns; ends the process when the guest powers off
            }
            Err(e) => {
                let _ = built_tx.send(Err(format!("{e:#}")));
            }
        }
    })?;
    match built_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => bail!("building the VM: {e}"),
        Err(_) => bail!("building the VM timed out"),
    }
    let clock = std::time::Instant::now();
    let conn = conn_rx
        .recv_timeout(Duration::from_secs(60))
        .context("vminitd didn't connect within 60 s (the guest's console is in vm\\krun.log)")??;
    log.info("vminitd connected", &[("t_boot_ms", clock.elapsed().as_millis().to_string())]);
    Ok(conn)
}

fn leak<T>(v: T) -> &'static T {
    Box::leak(Box::new(v))
}

fn build(
    spec: VmSpec,
    kernel: &Path,
    rootfs: &Path,
    cmdline: &str,
    rpc: &Path,
    stream: &Path,
    state: &Path,
) -> Result<krun::api::Vmm<'static>> {
    let level = match std::env::var("ELPIS_KRUN_LOG").as_deref() {
        Ok("debug") => LogLevel::Debug,
        Ok("info") => LogLevel::Info,
        _ => LogLevel::Warn,
    };
    let krun_log: &'static std::fs::File = leak(std::fs::File::create(state.join("krun.log"))?);
    init_log(Some(krun_log.as_handle()), level, LogStyle::Never, LogOptions::empty()).map_err(|e| anyhow::anyhow!("libkrun log: {e:?}"))?;

    let payload = Payload::load_external(&kernel.to_string_lossy(), KernelFormat::Elf, None, cmdline)
        .map_err(|e| anyhow::anyhow!("loading {}: {e:?}", kernel.display()))?;

    // hvc0: with no terminal, libkrun writes the guest's console (kernel and vminitd) into its log, krun.log
    let mut console = ConsoleDevice::builder();
    console.add_default_console(None, None, None).map_err(|e| anyhow::anyhow!("console: {e:?}"))?;
    let console = console.build().map_err(|e| anyhow::anyhow!("console: {e:?}"))?;

    let mut devices: Vec<Box<dyn FnOnce(&mut dyn Adder)>> = Vec::new();
    devices.push(Box::new(move |m| m.add_console(console)));
    let mut root = BlockDevice::new("vmrootfs", &rootfs.to_string_lossy(), DiskFormat::Raw).map_err(|e| anyhow::anyhow!("rootfs: {e:?}"))?;
    root.set_read_only(true);
    devices.push(Box::new(move |m| m.add_block(root)));
    for (i, d) in spec.disks.iter().enumerate() {
        let format = if d.vmdk { DiskFormat::Vmdk } else { DiskFormat::Raw };
        let mut b = BlockDevice::new(&format!("disk{i}"), &d.path.to_string_lossy(), format)
            .map_err(|e| anyhow::anyhow!("disk {}: {e:?}", d.path.display()))?;
        b.set_read_only(d.read_only);
        devices.push(Box::new(move |m| m.add_block(b)));
    }
    for s in &spec.shares {
        // libkrun opens the root with an NT path, so it must be absolute
        let path = std::path::absolute(&s.path)?;
        let mut fs = if s.read_only { FsDevice::new_read_only(&s.tag, &path.to_string_lossy()) } else { FsDevice::new(&s.tag, &path.to_string_lossy()) }
            .map_err(|e| anyhow::anyhow!("share {}: {e:?}", s.path.display()))?;
        // A shared Windows folder: files look owned by whoever in the box asks (the agent user can write its
        // workspace), and edits made on Windows show up in the box (no guest page cache for file data)
        fs.set_permission_semantics(PermissionSemantics::LinuxSimplified).map_err(|e| anyhow::anyhow!("share permissions: {e:?}"))?;
        fs.set_cache_policy(CachePolicy::Never).map_err(|e| anyhow::anyhow!("share cache policy: {e:?}"))?;
        devices.push(Box::new(move |m| m.add_fs(fs)));
    }
    for (i, n) in spec.nics.iter().enumerate() {
        let net = NetDevice::new_unixstream_path(&format!("net{i}"), &n.socket.to_string_lossy(), &n.mac, 0, NetFlags::empty())
            .map_err(|e| anyhow::anyhow!("nic {}: {e:?}", n.socket.display()))?;
        devices.push(Box::new(move |m| m.add_net(net)));
    }
    let mut vsock = VsockDevice::new(3, TsiFlags::empty()).map_err(|e| anyhow::anyhow!("vsock: {e:?}"))?;
    vsock.add_unix_port(RPC_PORT, &rpc.to_string_lossy(), false);
    vsock.add_unix_port(STREAM_PORT, &stream.to_string_lossy(), true);
    devices.push(Box::new(move |m| m.add_vsock(vsock)));
    let rng = RngDevice::new().map_err(|e| anyhow::anyhow!("rng: {e:?}"))?;
    devices.push(Box::new(move |m| m.add_rng(rng)));
    // virtio-rtc: the host's clocks as a PTP clock in the guest, which vminitd's clock service syncs from
    let rtc = RtcDevice::new().map_err(|e| anyhow::anyhow!("rtc: {e:?}"))?;
    devices.push(Box::new(move |m| m.add_rtc(rtc)));
    let balloon = BalloonDevice::new().map_err(|e| anyhow::anyhow!("balloon: {e:?}"))?;
    devices.push(Box::new(move |m| m.add_balloon(balloon)));

    // ACPI always: without it the guest kernel stalls during early boot on WHP (it never routes the ACPI SCI)
    let builder = VmmBuilder::new()
        .vcpus(spec.cpus)
        .map_err(|e| anyhow::anyhow!("vcpus: {e:?}"))?
        .ram_mib(spec.mem_mib)
        .map_err(|e| anyhow::anyhow!("memory: {e:?}"))?
        .acpi(true)
        .map_err(|e| anyhow::anyhow!("acpi: {e:?}"))?;
    // MMIO is the default transport; ELPIS_VIRTIO=pci selects virtio-pci (relies on patch 0003 on WHP)
    let builder = if std::env::var("ELPIS_VIRTIO").as_deref() == Ok("pci") {
        let mut m = PciDeviceManager::new();
        for d in devices {
            d(&mut m);
        }
        builder.payload(payload).devices(m)
    } else {
        let mut m = MmioDeviceManager::new();
        for d in devices {
            d(&mut m);
        }
        builder.payload(payload).devices(m)
    };
    builder.build().map_err(|e| anyhow::anyhow!("{e:?}"))
}

/// The two device managers have the same `add` but no common trait we can name; this bridges them.
trait Adder {
    fn add_console(&mut self, d: ConsoleDevice<'static>);
    fn add_block(&mut self, d: BlockDevice);
    fn add_fs(&mut self, d: FsDevice<'static>);
    fn add_net(&mut self, d: NetDevice);
    fn add_vsock(&mut self, d: VsockDevice);
    fn add_rng(&mut self, d: RngDevice);
    fn add_rtc(&mut self, d: RtcDevice);
    fn add_balloon(&mut self, d: BalloonDevice);
}

macro_rules! adder {
    ($t:ty) => {
        impl Adder for $t {
            fn add_console(&mut self, d: ConsoleDevice<'static>) {
                self.add(d);
            }
            fn add_block(&mut self, d: BlockDevice) {
                self.add(d);
            }
            fn add_fs(&mut self, d: FsDevice<'static>) {
                self.add(d);
            }
            fn add_net(&mut self, d: NetDevice) {
                self.add(d);
            }
            fn add_vsock(&mut self, d: VsockDevice) {
                self.add(d);
            }
            fn add_rng(&mut self, d: RngDevice) {
                self.add(d);
            }
            fn add_rtc(&mut self, d: RtcDevice) {
                self.add(d);
            }
            fn add_balloon(&mut self, d: BalloonDevice) {
                self.add(d);
            }
        }
    };
}
adder!(MmioDeviceManager<'static>);
adder!(PciDeviceManager<'static>);

/// A short-lived "tools VM" (guest/tools/elpis-mkfs): our rootfs as /dev/vda, `disks` as /dev/vdb…, the
/// console into `console`, and `init=/sbin/elpis-mkfs -- ARGS`. Never returns: libkrun ends the process when
/// the guest powers off, so the caller runs this in a child process and reads the console afterwards.
pub fn run_tool(args: &[String], disks: &[Disk], console: &Path) -> Result<std::convert::Infallible> {
    let (kernel, rootfs) = guest_files()?;
    // libkrun writes hvc0's output (port 0 of a default console with no terminal) to its log, so the log
    // file is the console
    let log_file: &'static std::fs::File = leak(std::fs::File::create(console)?);
    init_log(Some(log_file.as_handle()), LogLevel::Warn, LogStyle::Never, LogOptions::empty()).map_err(|e| anyhow::anyhow!("libkrun log: {e:?}"))?;
    let cmdline = format!("console=hvc0 root=/dev/vda rootfstype=erofs ro init=/sbin/elpis-mkfs -- {}", args.join(" "));
    let payload = Payload::load_external(&kernel.to_string_lossy(), KernelFormat::Elf, None, &cmdline)
        .map_err(|e| anyhow::anyhow!("loading {}: {e:?}", kernel.display()))?;
    let mut con = ConsoleDevice::builder();
    con.add_default_console(None, None, None).map_err(|e| anyhow::anyhow!("console: {e:?}"))?;
    let mut m = MmioDeviceManager::new();
    m.add(con.build().map_err(|e| anyhow::anyhow!("console: {e:?}"))?);
    let mut root = BlockDevice::new("vmrootfs", &rootfs.to_string_lossy(), DiskFormat::Raw).map_err(|e| anyhow::anyhow!("rootfs: {e:?}"))?;
    root.set_read_only(true);
    m.add(root);
    for (i, d) in disks.iter().enumerate() {
        let mut b = BlockDevice::new(&format!("disk{i}"), &d.path.to_string_lossy(), DiskFormat::Raw)
            .map_err(|e| anyhow::anyhow!("disk {}: {e:?}", d.path.display()))?;
        b.set_read_only(d.read_only);
        m.add(b);
    }
    m.add(RngDevice::new().map_err(|e| anyhow::anyhow!("rng: {e:?}"))?);
    let vmm = VmmBuilder::new()
        .vcpus(2)
        .map_err(|e| anyhow::anyhow!("vcpus: {e:?}"))?
        .ram_mib(2048)
        .map_err(|e| anyhow::anyhow!("memory: {e:?}"))?
        .acpi(true)
        .map_err(|e| anyhow::anyhow!("acpi: {e:?}"))?
        .payload(payload)
        .devices(m)
        .build()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    vmm.run();
    unreachable!("libkrun's run() returned")
}
