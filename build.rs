//! Embeds the application icon, version information and manifest into the executable.
//!
//! No resource compiler is required: the .rsrc section is written directly as a COFF object
//! (the same thing `windres`/`rc.exe` would produce) and handed to the linker.

#[path = "src/logo.rs"]
mod logo;

use std::{env, fs, path::PathBuf};

const LANG: u32 = 0x0409;

fn u16le(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn u32le(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn pad(v: &mut Vec<u8>, align: usize) {
    while v.len() % align != 0 {
        v.push(0);
    }
}
fn wide(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect()
}

/// 32-bit DIB icon image (BITMAPINFOHEADER + bottom-up BGRA + AND mask).
fn icon_dib(size: u32) -> Vec<u8> {
    let rgba = logo::rgba(size);
    let mut v = Vec::new();
    u32le(&mut v, 40);
    u32le(&mut v, size);
    u32le(&mut v, size * 2);
    u16le(&mut v, 1);
    u16le(&mut v, 32);
    for _ in 0..6 {
        u32le(&mut v, 0);
    }
    for y in (0..size as usize).rev() {
        for x in 0..size as usize {
            let p = &rgba[(y * size as usize + x) * 4..][..4];
            v.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    let row = ((size + 31) / 32 * 4) as usize;
    v.extend(std::iter::repeat(0u8).take(row * size as usize));
    v
}

/// One node of a VS_VERSIONINFO tree.
fn ver_node(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0, 0];
    u16le(&mut b, value_len);
    u16le(&mut b, text as u16);
    b.extend(wide(key));
    pad(&mut b, 4);
    b.extend_from_slice(value);
    for c in children {
        pad(&mut b, 4);
        b.extend_from_slice(c);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b
}

fn version_info(major: u16, minor: u16, patch: u16) -> Vec<u8> {
    let ver = format!("{major}.{minor}.{patch}.0");
    let strings: Vec<Vec<u8>> = [
        ("CompanyName", "NexMirror"),
        ("FileDescription", "NexMirror - Low-latency screen mirroring"),
        ("FileVersion", ver.as_str()),
        ("InternalName", "nexmirror"),
        ("LegalCopyright", "Copyright (C) NexMirror"),
        ("OriginalFilename", "NexMirror.exe"),
        ("ProductName", "NexMirror"),
        ("ProductVersion", ver.as_str()),
    ]
    .iter()
    .map(|(k, v)| {
        let w = wide(v);
        ver_node(k, &w, (w.len() / 2) as u16, true, &[])
    })
    .collect();
    let table = ver_node("040904B0", &[], 0, true, &strings);
    let sfi = ver_node("StringFileInfo", &[], 0, true, &[table]);
    let var = ver_node("Translation", &0x04B0_0409u32.to_le_bytes(), 4, false, &[]);
    let vfi = ver_node("VarFileInfo", &[], 0, true, &[var]);
    let ms = ((major as u32) << 16) | minor as u32;
    let ls = (patch as u32) << 16;
    let mut ffi = Vec::new();
    for x in [0xFEEF_04BDu32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0] {
        u32le(&mut ffi, x);
    }
    ver_node("VS_VERSION_INFO", &ffi, ffi.len() as u16, false, &[sfi, vfi])
}

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="NexMirror" version="1.0.0.0"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

/// Builds a COFF object holding a `.rsrc` section for the given (type, [(id, data)]) resources.
fn rsrc_object(mut types: Vec<(u32, Vec<(u32, Vec<u8>)>)>) -> Vec<u8> {
    types.sort_by_key(|t| t.0);
    for t in &mut types {
        t.1.sort_by_key(|n| n.0);
    }
    let n_types = types.len();
    let n_names: usize = types.iter().map(|t| t.1.len()).sum();
    let root = 16 + 8 * n_types;
    let type_dirs = types.iter().map(|t| 16 + 8 * t.1.len()).sum::<usize>();
    let lang_dirs = n_names * (16 + 8);
    let entries_at = root + type_dirs + lang_dirs;
    let mut blobs_at = entries_at + 16 * n_names;
    blobs_at = (blobs_at + 7) & !7;

    let dir_header = |v: &mut Vec<u8>, n: usize| {
        u32le(v, 0);
        u32le(v, 0);
        u32le(v, 0);
        u16le(v, 0);
        u16le(v, n as u16);
    };
    let mut s = Vec::new();
    let mut relocs = Vec::new();
    // Root.
    dir_header(&mut s, n_types);
    let mut type_off = root;
    for t in &types {
        u32le(&mut s, t.0);
        u32le(&mut s, 0x8000_0000 | type_off as u32);
        type_off += 16 + 8 * t.1.len();
    }
    // Type directories -> language directories.
    let mut lang_off = root + type_dirs;
    for t in &types {
        dir_header(&mut s, t.1.len());
        for n in &t.1 {
            u32le(&mut s, n.0);
            u32le(&mut s, 0x8000_0000 | lang_off as u32);
            lang_off += 24;
        }
    }
    // Language directories -> data entries.
    let mut entry_off = entries_at;
    for t in &types {
        for _ in &t.1 {
            dir_header(&mut s, 1);
            u32le(&mut s, LANG);
            u32le(&mut s, entry_off as u32);
            entry_off += 16;
        }
    }
    // Data entries.
    let mut blob_off = blobs_at;
    for t in &types {
        for n in &t.1 {
            relocs.push(s.len() as u32);
            u32le(&mut s, blob_off as u32);
            u32le(&mut s, n.1.len() as u32);
            u32le(&mut s, 0);
            u32le(&mut s, 0);
            blob_off = (blob_off + n.1.len() + 7) & !7;
        }
    }
    pad(&mut s, 8);
    for t in &types {
        for n in &t.1 {
            s.extend_from_slice(&n.1);
            pad(&mut s, 8);
        }
    }

    let raw_at = 20 + 40;
    let reloc_at = raw_at + s.len();
    let sym_at = reloc_at + relocs.len() * 10;
    let mut o = Vec::new();
    u16le(&mut o, 0x8664);
    u16le(&mut o, 1);
    u32le(&mut o, 0);
    u32le(&mut o, sym_at as u32);
    u32le(&mut o, 1);
    u16le(&mut o, 0);
    u16le(&mut o, 0);
    o.extend_from_slice(b".rsrc\0\0\0");
    u32le(&mut o, 0);
    u32le(&mut o, 0);
    u32le(&mut o, s.len() as u32);
    u32le(&mut o, raw_at as u32);
    u32le(&mut o, reloc_at as u32);
    u32le(&mut o, 0);
    u16le(&mut o, relocs.len() as u16);
    u16le(&mut o, 0);
    u32le(&mut o, 0xC030_0040);
    o.extend_from_slice(&s);
    for r in relocs {
        u32le(&mut o, r);
        u32le(&mut o, 0);
        u16le(&mut o, 3); // IMAGE_REL_AMD64_ADDR32NB
    }
    o.extend_from_slice(b".rsrc\0\0\0");
    u32le(&mut o, 0);
    u16le(&mut o, 1);
    u16le(&mut o, 0);
    o.push(3); // IMAGE_SYM_CLASS_STATIC
    o.push(0);
    u32le(&mut o, 4); // empty string table
    o
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/logo.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ver = |k: &str| env::var(k).ok().and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
    let (major, minor, patch) = (ver("CARGO_PKG_VERSION_MAJOR"), ver("CARGO_PKG_VERSION_MINOR"), ver("CARGO_PKG_VERSION_PATCH"));

    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 256];
    let mut icons = Vec::new();
    let mut group = Vec::new();
    u16le(&mut group, 0);
    u16le(&mut group, 1);
    u16le(&mut group, sizes.len() as u16);
    for (i, &sz) in sizes.iter().enumerate() {
        let dib = icon_dib(sz);
        let b = if sz >= 256 { 0 } else { sz as u8 };
        group.extend_from_slice(&[b, b, 0, 0]);
        u16le(&mut group, 1);
        u16le(&mut group, 32);
        u32le(&mut group, dib.len() as u32);
        u16le(&mut group, i as u16 + 1);
        icons.push((i as u32 + 1, dib));
    }
    let obj = rsrc_object(vec![
        (3, icons),                                         // RT_ICON
        (14, vec![(1, group)]),                             // RT_GROUP_ICON
        (16, vec![(1, version_info(major, minor, patch))]), // RT_VERSION
        (24, vec![(1, MANIFEST.as_bytes().to_vec())]),      // RT_MANIFEST
    ]);
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("nexmirror_resources.o");
    fs::write(&out, obj).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
