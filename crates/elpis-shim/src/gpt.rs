//! Many erofs layers as one virtio disk: a GPT partition table whose partitions are the layer files, described
//! by a VMDK `twoGbMaxExtentFlat` descriptor (FLAT extents for the header and each layer, ZERO extents for the
//! alignment gaps). The guest sees `/dev/vdX1…N`, one partition per layer, without any data being copied.
//!
//! A port of containerd/nerdbox `internal/erofs/{gpt,vmdk}.go` (Apache-2.0), byte-compatible with it.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const SECTOR: u64 = 512;
const ALIGN: u64 = 2048; // partitions start on 1 MiB boundaries
const ENTRIES: usize = 128;
const ENTRY_SIZE: usize = 128;
const ARRAY_SECTORS: u64 = (ENTRIES * ENTRY_SIZE) as u64 / SECTOR;
const RESERVED: u64 = 1 + 1 + ARRAY_SECTORS; // protective MBR + header + entry array
const LINUX_FS: [u8; 16] = [0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4];

struct Partition {
    guid: [u8; 16],
    first: u64,
    last: u64,
    source: PathBuf,
}

/// Writes `<dir>\merged_fs_gpt.vmdk` and its `merged_fs_gpt_header.bin` for `layers` (in partition order).
pub fn write(dir: &Path, layers: &[PathBuf]) -> Result<PathBuf> {
    if layers.len() > ENTRIES {
        bail!("{} layers: a GPT table holds at most {ENTRIES}", layers.len());
    }
    let mut sizes = Vec::new();
    for l in layers {
        let size = std::fs::metadata(l).with_context(|| format!("layer {}", l.display()))?.len();
        if size == 0 || size % SECTOR != 0 {
            bail!("layer {} is {size} bytes, not a whole number of sectors", l.display());
        }
        sizes.push(size);
    }

    let mut parts = Vec::new();
    let mut cursor = ALIGN;
    for (i, (l, &size)) in layers.iter().zip(&sizes).enumerate() {
        let first = cursor;
        let last = first + size / SECTOR - 1;
        parts.push(Partition { guid: part_guid(l, size, i), first, last, source: l.clone() });
        cursor = (last + 1).div_ceil(ALIGN) * ALIGN;
    }
    let total = cursor;

    // Protective MBR, GPT header, entry array
    let mut mbr = vec![0u8; SECTOR as usize];
    mbr[446 + 2] = 0x02;
    mbr[446 + 4] = 0xEE;
    mbr[446 + 5..446 + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&((total - 1).min(0xFFFF_FFFF) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let mut array = vec![0u8; ENTRIES * ENTRY_SIZE];
    for (i, p) in parts.iter().enumerate() {
        let e = &mut array[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE];
        e[0..16].copy_from_slice(&LINUX_FS);
        e[16..32].copy_from_slice(&p.guid);
        e[32..40].copy_from_slice(&p.first.to_le_bytes());
        e[40..48].copy_from_slice(&p.last.to_le_bytes());
        for (j, unit) in format!("erofs-layer-{i}").encode_utf16().take(36).enumerate() {
            e[56 + j * 2..58 + j * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }

    let mut header = vec![0u8; SECTOR as usize];
    header[0..8].copy_from_slice(b"EFI PART");
    header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    header[12..16].copy_from_slice(&92u32.to_le_bytes());
    header[24..32].copy_from_slice(&1u64.to_le_bytes());
    header[40..48].copy_from_slice(&RESERVED.to_le_bytes());
    header[48..56].copy_from_slice(&(total - 1).to_le_bytes());
    header[56..72].copy_from_slice(&disk_guid(layers, &sizes));
    header[72..80].copy_from_slice(&2u64.to_le_bytes());
    header[80..84].copy_from_slice(&(ENTRIES as u32).to_le_bytes());
    header[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
    header[88..92].copy_from_slice(&crc32(&array).to_le_bytes());
    let crc = crc32(&header[..92]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());

    let header_path = dir.join("merged_fs_gpt_header.bin");
    std::fs::write(&header_path, [mbr, header, array].concat())?;

    // The descriptor
    let mut d = String::new();
    let _ = write!(
        d,
        "# Disk DescriptorFile\nversion=1\nCID=fffffffe\nparentCID=ffffffff\ncreateType=\"twoGbMaxExtentFlat\"\n\n# Extent description\n"
    );
    let _ = writeln!(d, "RW {RESERVED} FLAT \"{}\" 0", header_path.display());
    let mut end = RESERVED;
    for p in &parts {
        if p.first > end {
            let _ = writeln!(d, "RW {} ZERO", p.first - end);
        }
        let _ = writeln!(d, "RW {} FLAT \"{}\" 0", p.last - p.first + 1, p.source.display());
        end = p.last + 1;
    }
    if total > end {
        let _ = writeln!(d, "RW {} ZERO", total - end);
    }
    let cylinders = total.div_ceil(63 * 255);
    let _ = write!(
        d,
        "\n\n# The Disk Data Base\n#DDB\n\nddb.virtualHWVersion = \"4\"\nddb.geometry.cylinders = \"{cylinders}\"\nddb.geometry.heads = \"255\"\nddb.geometry.sectors = \"63\"\nddb.adapterType = \"ide\"\n"
    );
    let vmdk = dir.join("merged_fs_gpt.vmdk");
    std::fs::write(&vmdk, d)?;
    Ok(vmdk)
}

fn guid_from(hash: &[u8]) -> [u8; 16] {
    let mut g = [0u8; 16];
    g.copy_from_slice(&hash[..16]);
    g[7] = (g[7] & 0x0F) | 0x40;
    g[8] = (g[8] & 0x3F) | 0x80;
    g
}

fn disk_guid(paths: &[PathBuf], sizes: &[u64]) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(b"nerdbox/erofs/disk");
    for (p, s) in paths.iter().zip(sizes) {
        h.update(format!("\n{}:{s}", p.display()).as_bytes());
    }
    guid_from(&h.finalize())
}

fn part_guid(path: &Path, size: u64, index: usize) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(b"nerdbox/erofs/part");
    h.update(format!("\n{index}:{}:{size}", path.display()).as_bytes());
    guid_from(&h.finalize())
}

/// CRC-32 (IEEE), as GPT uses.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_ieee() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn layout_matches_elpis() {
        let dir = std::env::temp_dir().join(format!("elpis-gpt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.erofs");
        let b = dir.join("b.erofs");
        std::fs::write(&a, vec![0u8; 3 * 512]).unwrap();
        std::fs::write(&b, vec![0u8; 4096]).unwrap();
        let vmdk = write(&dir, &[a.clone(), b.clone()]).unwrap();
        let text = std::fs::read_to_string(vmdk).unwrap();
        // header 34 sectors, gap to 2048, layer a (3), gap to 4096, layer b (8), gap to 6144
        let extents: Vec<&str> = text.lines().filter(|l| l.starts_with("RW ")).collect();
        assert_eq!(extents.len(), 6, "{text}");
        assert!(extents[0].starts_with("RW 34 FLAT"));
        assert_eq!(extents[1], "RW 2014 ZERO");
        assert!(extents[2].starts_with("RW 3 FLAT"));
        assert_eq!(extents[3], "RW 2045 ZERO");
        assert!(extents[4].starts_with("RW 8 FLAT"));
        assert_eq!(extents[5], "RW 2040 ZERO");
        let header = std::fs::read(dir.join("merged_fs_gpt_header.bin")).unwrap();
        assert_eq!(header.len(), 34 * 512);
        assert_eq!(&header[512..520], b"EFI PART");
        let _ = std::fs::remove_dir_all(dir);
    }
}
