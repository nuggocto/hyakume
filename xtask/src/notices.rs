//! The license notices of every crate compiled into the release binary.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Deserialize;

use crate::{Result, cargo, output};

/// The parts of `cargo metadata` the tasks read.
#[derive(Deserialize)]
pub struct Metadata {
    packages: Vec<Package>,
    resolve: Resolve,
    pub target_directory: PathBuf,
}

#[derive(Deserialize)]
pub struct Package {
    id: String,
    pub name: String,
    pub version: String,
    license: Option<String>,
    repository: Option<String>,
    manifest_path: PathBuf,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<Dep>,
}

#[derive(Deserialize)]
struct Dep {
    pkg: String,
    dep_kinds: Vec<DepKind>,
}

#[derive(Deserialize)]
struct DepKind {
    /// `None` for a normal dependency, else `dev` or `build`.
    kind: Option<String>,
}

/// The dependency graph as resolved for `target`.
pub fn metadata(target: &str) -> Result<Metadata> {
    let json = output(cargo().args([
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--filter-platform",
        target,
    ]))?;
    Ok(serde_json::from_str(&json)?)
}

impl Metadata {
    pub fn package(&self, name: &str) -> Result<&Package> {
        self.packages
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("cargo metadata has no package {name}").into())
    }

    /// The crates `root` reaches through normal dependencies, without
    /// itself, ordered by name and version. Development and build
    /// dependencies are not compiled into the binary, and neither are
    /// packages `root` does not depend on, such as these tasks.
    fn shipped(&self, root: &Package) -> Vec<&Package> {
        let nodes: HashMap<&str, &Node> = self
            .resolve
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n))
            .collect();
        let mut seen = BTreeSet::new();
        let mut todo = vec![root.id.as_str()];
        while let Some(id) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(node) = nodes.get(id) else { continue };
            for dep in &node.deps {
                if dep.dep_kinds.iter().any(|k| k.kind.is_none()) {
                    todo.push(&dep.pkg);
                }
            }
        }
        let mut shipped: Vec<&Package> = self
            .packages
            .iter()
            .filter(|p| p.id != root.id && seen.contains(p.id.as_str()))
            .collect();
        shipped.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
        shipped
    }
}

/// Each shipped crate's name, version, and license, then the license and
/// notice files it ships.
pub fn text(meta: &Metadata, root: &Package) -> Result<String> {
    let mut out = String::from(
        "Hyakume is licensed under Apache-2.0; see LICENSE.\n\
         It includes the following crates, under the licenses below.\n\n",
    );
    for p in meta.shipped(root) {
        let license = p.license.as_deref().unwrap_or("see files below");
        writeln!(out, "{}", "=".repeat(78))?;
        writeln!(out, "{} {}: {license}", p.name, p.version)?;
        if let Some(repository) = &p.repository {
            writeln!(out, "{repository}")?;
        }
        let folder = p
            .manifest_path
            .parent()
            .ok_or("a manifest path has no folder")?;
        let mut files: Vec<PathBuf> = std::fs::read_dir(folder)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        files.sort();
        for path in files {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let upper = name.to_ascii_uppercase();
            let notice = ["LICENSE", "LICENCE", "COPYING", "NOTICE"]
                .iter()
                .any(|prefix| upper.starts_with(prefix));
            if notice && path.is_file() {
                let content = std::fs::read(&path)?;
                writeln!(out, "\n--- {name} ---")?;
                // Some crates ship Windows line endings; the notices use one.
                let content = String::from_utf8_lossy(&content).replace("\r\n", "\n");
                writeln!(out, "{}", content.trim_end())?;
            }
        }
        writeln!(out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_crates_reached_through_normal_dependencies_ship() {
        let package = |name: &str| {
            format!(
                r#"{{"id": "{name}", "name": "{name}", "version": "1.0.0", "license": "MIT",
                    "repository": null, "manifest_path": "/{name}/Cargo.toml"}}"#
            )
        };
        let dep = |pkg: &str, kinds: &str| format!(r#"{{"pkg": "{pkg}", "dep_kinds": [{kinds}]}}"#);
        let normal = r#"{"kind": null}"#;
        let dev = r#"{"kind": "dev"}"#;
        let build = r#"{"kind": "build"}"#;
        let names = [
            "app",
            "lib",
            "deep",
            "both",
            "test-only",
            "build-only",
            "tasks",
        ];
        let json = format!(
            r#"{{"packages": [{}], "target_directory": "/target", "resolve": {{"nodes": [
                {{"id": "app", "deps": [{}, {}, {}, {}]}},
                {{"id": "lib", "deps": [{}]}},
                {{"id": "tasks", "deps": [{}]}},
                {{"id": "deep", "deps": []}}, {{"id": "both", "deps": []}},
                {{"id": "test-only", "deps": []}}, {{"id": "build-only", "deps": []}}
            ]}}}}"#,
            names.map(package).join(","),
            dep("lib", normal),
            dep("test-only", dev),
            dep("build-only", build),
            dep("both", &format!("{dev}, {normal}")),
            dep("deep", normal),
            dep("app", normal),
        );
        let meta: Metadata = serde_json::from_str(&json).unwrap();
        let root = meta.package("app").unwrap();
        let shipped: Vec<&str> = meta.shipped(root).iter().map(|p| p.name.as_str()).collect();
        assert_eq!(shipped, ["both", "deep", "lib"]);
    }
}
