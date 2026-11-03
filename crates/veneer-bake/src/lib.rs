//! veneer-bake: runs the build phase into a snapshot (design spec §7).
//!
//! `oci`: a generated Containerfile with one `RUN veneer apply --phase build --only <group>`
//! per layer, so BuildKit caches each group independently. Each layer copies in only the
//! files its group reads, so editing app code doesn't invalidate the tools or deps layers.
//! `rootfs`: the OCI result exported and packed into ext4 for microVMs.

use std::path::{Path, PathBuf};
use std::process::Command;

use veneer_exec::{ExecError, Log, Result, run_logged};
use veneer_plan::{BakeInputs, Plan};

/// In-image locations.
pub const PROJECT_DIR: &str = "/opt/veneer/project";
pub const STATE_DIR: &str = "/var/lib/veneer";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Oci,
    Rootfs,
}

#[derive(Debug, Clone)]
pub struct BakeOptions {
    pub target: Target,
    pub tag: String,
    pub push: bool,
    pub platform: Option<String>,
    /// A Linux veneer binary for the image's platform.
    pub veneer_bin: PathBuf,
    /// Output path for `rootfs`.
    pub output: Option<PathBuf>,
    /// Keep the generated build context here (for inspection/debugging).
    pub context_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baked {
    pub image: String,
    pub id: Option<String>,
    pub rootfs: Option<PathBuf>,
    pub containerfile: String,
}

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", ".veneer-state"];

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if SKIP_DIRS.iter().any(|s| name == *s) {
            continue;
        }
        let ty = entry.file_type()?;
        let dest = to.join(&name);
        if ty.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if ty.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            let _ = std::os::unix::fs::symlink(target, dest);
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn stage(project: &Path, files: &[PathBuf], dir: &Path) -> Result<()> {
    for f in files {
        let rel = f.strip_prefix(project).map_err(|_| {
            ExecError::failed(format!(
                "bake needs every input inside the project root ({}); {} is outside it",
                project.display(),
                f.display()
            ))
        })?;
        let dest = dir.join(rel);
        std::fs::create_dir_all(dest.parent().unwrap())
            .map_err(|e| ExecError::failed(e.to_string()))?;
        std::fs::copy(f, &dest)
            .map_err(|e| ExecError::failed(format!("cannot copy {}: {e}", f.display())))?;
    }
    Ok(())
}

fn q(s: &str) -> String {
    serde_like_json(s)
}

fn serde_like_json(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Everything one bake needs besides the build context directory.
pub struct BakeJob<'a> {
    pub base: &'a str,
    pub plan: &'a Plan,
    pub entry: &'a Path,
    pub documents: &'a [PathBuf],
    pub inputs: &'a BakeInputs,
    pub veneer_bin: &'a Path,
    /// Overlay files (absolute, inside the project) passed to every in-image invocation.
    pub overlays: &'a [PathBuf],
    pub sets: &'a [(String, String)],
}

/// Generates the Containerfile and stages the build context in `ctx`.
pub fn prepare(job: &BakeJob<'_>, ctx: &Path) -> Result<String> {
    let BakeJob {
        base,
        plan,
        entry,
        documents,
        inputs,
        veneer_bin,
        overlays,
        sets,
    } = *job;
    let project = &plan.project_root;
    let entry_rel = entry
        .strip_prefix(project)
        .map_err(|_| ExecError::failed("the spec must live inside the project root"))?
        .display()
        .to_string();
    std::fs::create_dir_all(ctx).map_err(|e| ExecError::failed(e.to_string()))?;
    std::fs::copy(veneer_bin, ctx.join("veneer")).map_err(|e| {
        ExecError::failed(format!(
            "cannot copy veneer binary {}: {e}",
            veneer_bin.display()
        ))
    })?;

    let groups: Vec<&str> = {
        let mut g: Vec<&str> = Vec::new();
        for s in &plan.steps {
            if !g.contains(&s.group.as_str()) {
                g.push(&s.group);
            }
        }
        g
    };
    let has = |g: &str| groups.contains(&g);
    // `--overlay`/`--set` must reach every in-image invocation, or the image would be built
    // from a different spec than the one the user baked.
    let mut extra = String::new();
    for o in overlays {
        let rel = o.strip_prefix(project).map_err(|_| {
            ExecError::failed(format!(
                "overlay {} must be inside the project root to bake",
                o.display()
            ))
        })?;
        extra.push_str(&format!(
            ", \"--overlay\", {}",
            q(&format!("{PROJECT_DIR}/{}", rel.display()))
        ));
    }
    for (k, v) in sets {
        extra.push_str(&format!(", \"--set\", {}", q(&format!("{k}={v}"))));
    }
    let run = |group: &str| {
        format!(
            "RUN [\"veneer\", \"apply\", \"--phase\", \"build\", \"--only\", {}{extra}, {}]\n",
            q(group),
            q(&entry_rel)
        )
    };

    let mut cf = String::new();
    cf.push_str("# syntax=docker/dockerfile:1\n# Generated by `veneer bake`. Do not edit.\n");
    cf.push_str(&format!("FROM {base}\n"));
    cf.push_str(&format!("ENV VENEER_STATE_DIR={STATE_DIR}\n"));
    cf.push_str("COPY veneer /usr/local/bin/veneer\n");

    // Layer: spec documents + packages/tools imports.
    let mut early: Vec<PathBuf> = documents
        .iter()
        .filter(|d| d.starts_with(project))
        .cloned()
        .collect();
    early.extend(inputs.early.iter().cloned());
    early.extend(overlays.iter().cloned());
    stage(project, &early, &ctx.join("ctx-spec"))?;
    cf.push_str(&format!(
        "COPY ctx-spec/ {PROJECT_DIR}/\nWORKDIR {PROJECT_DIR}\n"
    ));
    for g in ["packages", "tools"] {
        if has(g) {
            cf.push_str(&run(g));
        }
    }
    // One layer per dependency lockfile.
    for (i, (group, manifest, lock)) in inputs.deps.iter().enumerate() {
        let mut files = vec![manifest.clone()];
        files.extend(lock.iter().cloned());
        let dir = format!("ctx-deps-{i}");
        stage(project, &files, &ctx.join(&dir))?;
        cf.push_str(&format!("COPY {dir}/ {PROJECT_DIR}/\n"));
        cf.push_str(&run(group));
    }
    // Everything else.
    copy_tree(project, &ctx.join("ctx-project"))
        .map_err(|e| ExecError::failed(format!("cannot stage project: {e}")))?;
    cf.push_str(&format!("COPY ctx-project/ {PROJECT_DIR}/\n"));
    if has("images") {
        cf.push_str(&run("images"));
    }
    // Final pass: build hooks, and re-verifies every earlier group (all skip when intact).
    cf.push_str(&format!(
        "RUN [\"veneer\", \"apply\", \"--phase\", \"build\"{extra}, {}]\n",
        q(&entry_rel)
    ));
    cf.push_str(&format!(
        "ENTRYPOINT [\"veneer\", \"entrypoint\"{extra}, {}, \"--\"]\nCMD [\"sh\"]\n",
        q(&format!("{PROJECT_DIR}/{entry_rel}"))
    ));
    std::fs::write(ctx.join("Containerfile"), &cf).map_err(|e| ExecError::failed(e.to_string()))?;
    Ok(cf)
}

fn has_cmd(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Builds the staged context with docker buildx, or rootless buildah.
pub fn build(ctx: &Path, opts: &BakeOptions, log: &mut Log) -> Result<Baked> {
    let containerfile = std::fs::read_to_string(ctx.join("Containerfile")).unwrap_or_default();
    if has_cmd("docker", &["buildx", "version"]) {
        let mut c = Command::new("docker");
        c.args(["buildx", "build", "-f"])
            .arg(ctx.join("Containerfile"))
            .args(["-t", &opts.tag]);
        if let Some(p) = &opts.platform {
            c.args(["--platform", p]);
        }
        c.arg(if opts.push { "--push" } else { "--load" });
        c.arg(ctx);
        run_logged(&mut c, log)?;
    } else if has_cmd("buildah", &["--version"]) {
        let mut c = Command::new("buildah");
        c.args(["bud", "-f"])
            .arg(ctx.join("Containerfile"))
            .args(["-t", &opts.tag]);
        if let Some(p) = &opts.platform {
            c.args(["--platform", p]);
        }
        c.arg(ctx);
        run_logged(&mut c, log)?;
        if opts.push {
            run_logged(Command::new("buildah").args(["push", &opts.tag]), log)?;
        }
    } else {
        return Err(ExecError::failed("bake needs `docker buildx` or `buildah`"));
    }
    let id = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", &opts.tag])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let mut baked = Baked {
        image: opts.tag.clone(),
        id,
        rootfs: None,
        containerfile,
    };
    if opts.target == Target::Rootfs {
        baked.rootfs = Some(rootfs(ctx, opts, log)?);
    }
    Ok(baked)
}

/// Exports the built image and packs it into an ext4 filesystem with a helper container.
fn rootfs(ctx: &Path, opts: &BakeOptions, log: &mut Log) -> Result<PathBuf> {
    let out = opts
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("rootfs.ext4"));
    let tar = ctx.join("rootfs.tar");
    let cid = Command::new("docker")
        .args(["create", &opts.tag])
        .output()
        .map_err(|e| ExecError::failed(e.to_string()))?;
    if !cid.status.success() {
        return Err(ExecError::failed(format!(
            "docker create {} failed",
            opts.tag
        )));
    }
    let cid = String::from_utf8_lossy(&cid.stdout).trim().to_string();
    let exported = run_logged(
        Command::new("docker")
            .args(["export", "-o"])
            .arg(&tar)
            .arg(&cid),
        log,
    );
    let _ = Command::new("docker").args(["rm", &cid]).output();
    exported?;
    let size_mb = std::fs::metadata(&tar)
        .map(|m| m.len() / (1024 * 1024))
        .unwrap_or(256)
        * 3
        / 2
        + 128;
    let script = format!(
        "set -e; apk add --no-cache e2fsprogs >/dev/null; mkdir /r; tar -xf /w/rootfs.tar -C /r; \
         mkfs.ext4 -q -F -d /r /w/rootfs.ext4 {size_mb}M"
    );
    run_logged(
        Command::new("docker")
            .args(["run", "--rm", "-v"])
            .arg(format!("{}:/w", ctx.display()))
            .args(["alpine:3.20", "sh", "-c", &script]),
        log,
    )?;
    std::fs::copy(ctx.join("rootfs.ext4"), &out).map_err(|e| ExecError::failed(e.to_string()))?;
    Ok(out)
}
