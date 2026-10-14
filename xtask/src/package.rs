//! Builds the Linux package: the release binary, the license and the
//! notices of every crate compiled into it, the user service unit, and the
//! install notes, with a checksum and the identity of the build.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::{Result, cargo, host_target, notices, output, package_name, root, run, sha256_hex};

/// Builds `dist/hyakume-VERSION-TARGET.tar.gz` and its `.sha256`, and
/// returns the archive's path.
pub fn package() -> Result<PathBuf> {
    let target = host_target()?;
    let meta = notices::metadata(&target)?;
    let app = meta.package("hyakume")?;
    run(cargo().args(["build", "--release", "--locked", "--package", "hyakume"]))?;

    let read = |path: PathBuf| std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()));
    let linux = root().join("packaging/linux");
    let files = BTreeMap::from([
        (
            "bin/hyakume",
            (0o755, read(meta.target_directory.join("release/hyakume"))?),
        ),
        ("LICENSE", (0o644, read(root().join("LICENSE"))?)),
        ("INSTALL.md", (0o644, read(linux.join("INSTALL.md"))?)),
        (
            "share/hyakume/hyakume.service",
            (0o644, read(linux.join("hyakume.service"))?),
        ),
        (
            "THIRD-PARTY-NOTICES.txt",
            (0o644, notices::text(&meta, app)?.into_bytes()),
        ),
        (
            "BUILD-INFO",
            (0o644, build_info(&app.version, &target)?.into_bytes()),
        ),
    ]);

    let name = package_name(&app.version, &target);
    let archive = archive(&name, &files, commit_time()?)?;
    let dist = root().join("dist");
    std::fs::create_dir_all(&dist)?;
    let path = dist.join(format!("{name}.tar.gz"));
    std::fs::write(&path, &archive)?;
    // The format `sha256sum -c` reads, as INSTALL.md says to check it.
    let sum = format!("{}  {name}.tar.gz\n", sha256_hex(&archive));
    std::fs::write(dist.join(format!("{name}.tar.gz.sha256")), sum)?;
    Ok(path)
}

/// What was built, from which commit, with which toolchain.
fn build_info(version: &str, target: &str) -> Result<String> {
    let commit = output(Command::new("git").args(["rev-parse", "HEAD"]))?;
    let clean = Command::new("git")
        .args(["diff", "--quiet", "HEAD", "--"])
        .current_dir(root())
        .status()?
        .success();
    let toolchain = output(Command::new("rustc").arg("-V"))?;
    let lock = std::fs::read(root().join("Cargo.lock"))?;
    Ok(format!(
        "version: {version}\n\
         commit: {}{}\n\
         toolchain: {}\n\
         target: {target}\n\
         profile: release\n\
         Cargo.lock sha256: {}\n",
        commit.trim(),
        if clean { "" } else { " (uncommitted changes)" },
        toolchain.trim(),
        sha256_hex(&lock),
    ))
}

/// The commit's time, so the same commit gives the same archive.
fn commit_time() -> Result<u64> {
    let time = output(Command::new("git").args(["log", "-1", "--format=%ct"]))?;
    Ok(time.trim().parse()?)
}

/// A gzipped tar of `files` under one folder named `top`, with the
/// folders they need. Entries are in name order and owned by root, and
/// every one carries `mtime`.
fn archive(top: &str, files: &BTreeMap<&str, (u32, Vec<u8>)>, mtime: u64) -> Result<Vec<u8>> {
    let mut folders = BTreeSet::from([top.to_owned()]);
    for path in files.keys() {
        let mut parent = std::path::Path::new(path).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            folders.insert(format!("{top}/{}", p.display()));
            parent = p.parent();
        }
    }
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::best()));
    let header = |kind, mode, size| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(size);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(mtime);
        h
    };
    for folder in &folders {
        let mut h = header(tar::EntryType::Directory, 0o755, 0);
        tar.append_data(&mut h, folder, std::io::empty())?;
    }
    for (path, (mode, data)) in files {
        let mut h = header(tar::EntryType::Regular, *mode, data.len() as u64);
        tar.append_data(&mut h, format!("{top}/{path}"), data.as_slice())?;
    }
    Ok(tar.into_inner()?.finish()?)
}
