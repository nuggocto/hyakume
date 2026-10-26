//! Fills server b's Scale workspace: 3 projects of 50 tasks and about 90
//! links, as a real team's tracker would have. Links follow fixed patterns
//! rather than random picks, so every run makes the same data and no
//! duplicates. `mise run demo-data` runs it after building the CLI.

use std::env::consts::EXE_SUFFIX;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const SUBJECTS: [&str; 20] = [
    "Login", "Billing", "Search", "Export", "Import", "Cache", "Queue", "Report", "Invoice",
    "Profile", "Settings", "Upload", "Webhook", "Audit", "Theme", "Email", "Sync", "Backup",
    "Alerts", "Metrics",
];
const CHORES: [&str; 7] = [
    "Fix crash in",
    "Add tests for",
    "Speed up",
    "Redesign",
    "Document",
    "Clean up",
    "Add logging to",
];
const STAGES: [&str; 5] = ["Backlog", "To do", "In progress", "Review", "Done"];
const PROJECTS: [&str; 3] = ["APP", "SRV", "OPS"];

/// Runs the CLI as Alice on the Scale workspace, like the other demo steps.
struct Scale {
    cli: PathBuf,
    profile: PathBuf,
}

impl Scale {
    fn run(&self, args: &[&str]) -> Result<(), String> {
        let status = Command::new(&self.cli)
            .env("HYAKUME_DATA_DIR", &self.profile)
            .args(["-w", "b/scale"])
            .args(args)
            .stdout(Stdio::null())
            .status()
            .map_err(|e| format!("can't run {}: {e}", self.cli.display()))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("`hyakume {}` failed ({status})", args.join(" ")))
        }
    }

    fn link(&self, from: String, kind: &str, to: String) -> Result<(), String> {
        self.run(&["relation", "create", &from, kind, &to])
    }
}

fn fill(scale: &Scale) -> Result<(), String> {
    for project in PROJECTS {
        for i in 1..=50 {
            let title = format!("{} {}", CHORES[i % 7], SUBJECTS[(i * 3) % 20]);
            let owner = if i % 3 == 0 { "bob" } else { "alice" };
            scale.run(&[
                "task",
                "create",
                "--project",
                project,
                &title,
                "--stage",
                STAGES[(i * 7) % 5],
                "--assignee",
                owner,
            ])?;
        }
    }

    // Chains of blockers and loose relations inside each project.
    for project in PROJECTS {
        for i in 1..=50 {
            if i % 4 == 1 && i + 3 <= 50 {
                scale.link(
                    format!("{project}-{i}"),
                    "blocks",
                    format!("{project}-{}", i + 3),
                )?;
            }
            if i % 6 == 2 && i + 7 <= 50 {
                let to = format!("{project}-{}", i + 7);
                scale.link(format!("{project}-{i}"), "related_to", to)?;
            }
        }
    }
    // Links between projects, the ones the overview bundles.
    for i in 1..=50 {
        if i % 5 == 0 {
            scale.link(format!("APP-{i}"), "blocks", format!("SRV-{i}"))?;
        }
        if i % 7 == 3 {
            scale.link(format!("SRV-{i}"), "blocks", format!("OPS-{}", i + 1))?;
        }
        if i % 9 == 4 {
            scale.link(format!("OPS-{i}"), "related_to", format!("APP-{}", 51 - i))?;
        }
    }
    // One busy task, to see a lot light up at once.
    for i in [2, 4, 6, 8, 10, 12] {
        scale.link("SRV-1".into(), "blocks", format!("APP-{i}"))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    // Cargo puts examples in target/<profile>/examples, next to the CLI's
    // own directory, so this finds the build `mise` just made.
    let cli = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join(format!("hyakume{EXE_SUFFIX}"))));
    let Some(cli) = cli else {
        eprintln!("can't tell where the hyakume binary was built");
        return ExitCode::FAILURE;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let scale = Scale {
        cli,
        profile: root.join("dev/data/profiles/alice"),
    };
    match fill(&scale) {
        Ok(()) => ExitCode::SUCCESS,
        Err(problem) => {
            eprintln!("{problem}");
            ExitCode::FAILURE
        }
    }
}
