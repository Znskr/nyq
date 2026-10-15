//! `nyq extensions list|install|remove|path` (alias `nyq ext`).

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::protocol::Manifest;
use super::{discover, project_dir, user_dir, MANIFEST};
use crate::skill_install::{clone_url, copy_tree, expand_tilde, git_clone, remove_path, Staging};

#[derive(clap::Subcommand)]
pub enum ExtCommand {
    /// List profile and project extensions (without starting them).
    List,
    /// Install from a local directory, a git URL or GitHub owner/repo into the profile.
    Install {
        source: String,
        /// Run the extension's install command without asking.
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove an extension installed in the profile.
    Remove { name: String },
    /// Print the extension directories.
    Path,
}

pub async fn run(home: &Path, cwd: &Path, cmd: ExtCommand) -> Result<()> {
    match cmd {
        ExtCommand::List => {
            let (found, warnings) = discover(home, cwd);
            warnings.iter().for_each(|w| eprintln!("{w}"));
            if found.is_empty() {
                println!("No extensions in {} or {}.", user_dir(home).display(), project_dir(cwd).display());
            }
            for f in found {
                let m = &f.manifest;
                let state = match (f.enabled, f.trusted) {
                    (false, _) => "disabled",
                    (_, false) => "untrusted",
                    _ => "enabled",
                };
                let scope = if f.project { "project" } else { "user" };
                println!("{} {} · {scope} · {state} · {}", m.name, m.version, m.command.join(" "));
                if !m.description.is_empty() {
                    println!("  {}", m.description);
                }
                println!("  {}", f.dir.display());
            }
            Ok(())
        }
        ExtCommand::Install { source, yes } => {
            for name in install(home, cwd, &source, yes).await? {
                println!("Installed {name} into {}", user_dir(home).join(&name).display());
            }
            Ok(())
        }
        ExtCommand::Remove { name } => {
            anyhow::ensure!(super::protocol::valid_name(&name), "`{name}` is not an extension name");
            let dir = user_dir(home).join(&name);
            anyhow::ensure!(dir.join(MANIFEST).is_file(), "no extension `{name}` in {}", user_dir(home).display());
            remove_path(&dir).with_context(|| format!("removing {}", dir.display()))?;
            println!("Removed {name}");
            Ok(())
        }
        ExtCommand::Path => {
            println!("{}", user_dir(home).display());
            println!("{}", project_dir(cwd).display());
            Ok(())
        }
    }
}

/// Extension directories in `root`: itself, or its direct subdirectories.
fn manifests(root: &Path) -> Result<Vec<(PathBuf, Manifest)>> {
    let read = |dir: &Path| -> Result<Manifest> {
        let file = dir.join(MANIFEST);
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        Manifest::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))
    };
    if root.join(MANIFEST).is_file() {
        return Ok(vec![(root.to_path_buf(), read(root)?)]);
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(MANIFEST).is_file())
        .collect();
    dirs.sort();
    if dirs.is_empty() {
        bail!("no {MANIFEST} in {} or its subdirectories", root.display());
    }
    dirs.into_iter().map(|d| read(&d).map(|m| (d, m))).collect()
}

/// Copy (or clone) `source` into the profile's extensions directory, then
/// run each extension's `install` command once the user agrees.
pub async fn install(home: &Path, cwd: &Path, source: &str, yes: bool) -> Result<Vec<String>> {
    let dest = user_dir(home);
    std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let local = {
        let p = expand_tilde(source.trim());
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    let _clone;
    let root = if local.is_dir() {
        local
    } else {
        let Some(url) = clone_url(source.trim()) else {
            bail!("`{source}` is neither a directory nor a git URL");
        };
        _clone = Staging(dest.join(format!(".clone-{}", uuid::Uuid::new_v4().simple())));
        git_clone(&url, &_clone.0).await?;
        _clone.0.clone()
    };
    let mut names = Vec::new();
    for (dir, manifest) in manifests(&root)? {
        let target = dest.join(&manifest.name);
        let stage = Staging(dest.join(format!(".install-{}", uuid::Uuid::new_v4().simple())));
        copy_tree(&dir, &stage.0)?;
        if std::fs::symlink_metadata(&target).is_ok() {
            remove_path(&target).with_context(|| format!("replacing {}", target.display()))?;
        }
        std::fs::rename(&stage.0, &target).with_context(|| format!("installing {}", target.display()))?;
        if let Some(cmd) = &manifest.install {
            run_install(&target, &manifest.name, cmd, yes)?;
        }
        names.push(manifest.name);
    }
    Ok(names)
}

/// Show the install command and run it in `dir` once confirmed.
fn run_install(dir: &Path, name: &str, cmd: &[String], yes: bool) -> Result<()> {
    let shown = cmd.join(" ");
    eprintln!("Extension {name} has an install command:\n  {shown}\n  (in {})", dir.display());
    if !yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("Skipped it: no terminal to confirm. Run it yourself there, or install with --yes.");
            return Ok(());
        }
        eprint!("Run it now? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            eprintln!("Skipped. Run it yourself in {} before using the extension.", dir.display());
            return Ok(());
        }
    }
    let program = if cmd[0].starts_with("./") || cmd[0].starts_with("../") { dir.join(&cmd[0]) } else { PathBuf::from(&cmd[0]) };
    let status = std::process::Command::new(program)
        .args(&cmd[1..])
        .current_dir(dir)
        .status()
        .with_context(|| format!("running `{shown}`"))?;
    anyhow::ensure!(status.success(), "`{shown}` failed ({status}); the extension is installed but may not work");
    Ok(())
}
