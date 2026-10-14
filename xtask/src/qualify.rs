//! Qualifies a Linux package on a clean host: a fresh container that never
//! ran Hyakume, with no network beyond loopback, runs the packaged binary
//! through the whole journey of the `clean_host_journey` test. Its /tmp is
//! a tmpfs, a filesystem Hyakume publishes into; the container's own
//! overlay filesystem is refused, as it should be.

use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;

use crate::{Result, cargo, host_target, notices, output, package_name, root, run, sha256_hex};

pub fn qualify(package: Option<PathBuf>, image: &str) -> Result<()> {
    let package = match package {
        Some(path) => path,
        None => {
            let target = host_target()?;
            let version = notices::metadata(&target)?
                .package("hyakume")?
                .version
                .clone();
            root().join(format!("dist/{}.tar.gz", package_name(&version, &target)))
        }
    };
    let file = package
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("the package path names no file")?;
    let top = file
        .strip_suffix(".tar.gz")
        .ok_or("a package is a .tar.gz file")?;
    let archive = std::fs::read(&package).map_err(|e| format!("{}: {e}", package.display()))?;
    let sum = std::fs::read_to_string(format!("{}.sha256", package.display()))?;
    check_sum(file, &archive, &sum)?;

    let work = Work::new()?;
    unpack(&archive, &work.0, top)?;
    // The journey lives in the end-to-end tests; it drives whatever binary
    // HYAKUME_BIN names.
    std::fs::copy(journey()?, work.0.join("journey"))?;
    run(Command::new("docker").args([
        "run",
        "--rm",
        "--network",
        "none",
        "--tmpfs",
        "/tmp:rw,exec,mode=1777",
        "--volume",
        &format!("{}:/work:ro", work.0.display()),
        "--env",
        &format!("HYAKUME_BIN=/work/{top}/bin/hyakume"),
        image,
        "/work/journey",
        "--ignored",
        "--exact",
        "clean_host_journey",
    ]))?;
    println!("qualified: {file} on {image}");
    Ok(())
}

/// Checks `archive`, named `file`, against its checksum line, in the
/// format `sha256sum` writes.
fn check_sum(file: &str, archive: &[u8], sum: &str) -> Result<()> {
    let (digest, named) = sum
        .trim_end()
        .split_once("  ")
        .ok_or("the checksum file is not in sha256sum format")?;
    if named != file {
        return Err(format!("the checksum is for {named}, not {file}").into());
    }
    if digest != sha256_hex(archive) {
        return Err(format!("{file} does not match its checksum").into());
    }
    Ok(())
}

/// Unpacks the package into `into` and checks that it holds one folder,
/// `top`, with an executable binary and the files that ship beside it.
fn unpack(archive: &[u8], into: &Path, top: &str) -> Result<()> {
    let mut tar = tar::Archive::new(GzDecoder::new(archive));
    tar.set_preserve_permissions(true);
    tar.unpack(into)?;
    let entries: Vec<_> = std::fs::read_dir(into)?.collect::<std::io::Result<_>>()?;
    if entries.len() != 1 || entries[0].file_name() != top {
        return Err(format!("the package must hold only the folder {top}").into());
    }
    let folder = into.join(top);
    use std::os::unix::fs::PermissionsExt as _;
    let binary = std::fs::metadata(folder.join("bin/hyakume"))?;
    if !binary.is_file() || binary.permissions().mode() & 0o777 != 0o755 {
        return Err("bin/hyakume must be a file with mode 0755".into());
    }
    for file in [
        "LICENSE",
        "INSTALL.md",
        "THIRD-PARTY-NOTICES.txt",
        "BUILD-INFO",
        "share/hyakume/hyakume.service",
    ] {
        if !folder.join(file).is_file() {
            return Err(format!("the package has no {file}").into());
        }
    }
    Ok(())
}

/// Builds the end-to-end tests and returns their executable.
fn journey() -> Result<PathBuf> {
    let messages = output(cargo().args([
        "test",
        "--locked",
        "--package",
        "hyakume",
        "--test",
        "service",
        "--no-run",
        "--message-format=json",
    ]))?;
    for line in messages.lines() {
        let message: serde_json::Value = serde_json::from_str(line)?;
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == "service"
            && let Some(executable) = message["executable"].as_str()
        {
            return Ok(executable.into());
        }
    }
    Err("cargo built no service test executable".into())
}

/// A private working folder, removed when the task ends.
struct Work(PathBuf);

impl Work {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("hyakume-qualify-{}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_must_match_the_checksum_written_for_it() {
        let file = "hyakume-1-x.tar.gz";
        let sum = format!("{}  {file}\n", sha256_hex(b"package"));
        assert!(check_sum(file, b"package", &sum).is_ok());
        assert!(check_sum(file, b"changed", &sum).is_err());
        assert!(check_sum("hyakume-2-x.tar.gz", b"package", &sum).is_err());
    }
}
