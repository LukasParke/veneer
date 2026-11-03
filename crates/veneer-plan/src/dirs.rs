//! Where veneer keeps state, data, and caches (design spec §5.6).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// Journals, locks, logs, generated configs.
    pub state: PathBuf,
    /// Installed artifacts (mise installs, image tarballs, bin).
    pub data: PathBuf,
    /// Re-downloadable content (downloads, git mirrors, remote specs).
    pub cache: PathBuf,
}

/// True when running as uid 0.
pub fn is_root() -> bool {
    geteuid() == 0
}

#[cfg(unix)]
fn geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() }
}

#[cfg(not(unix))]
fn geteuid() -> u32 {
    1
}

impl Dirs {
    /// `VENEER_STATE_DIR` > `/var/lib/veneer` (root on Linux) > `$XDG_STATE_HOME/veneer` > `~/.local/state/veneer`.
    pub fn from_env() -> Self {
        if let Some(dir) = std::env::var_os("VENEER_STATE_DIR") {
            return Self::under(PathBuf::from(dir));
        }
        if cfg!(target_os = "linux") && is_root() {
            return Self::under(PathBuf::from("/var/lib/veneer"));
        }
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        Self::under(base.join("veneer"))
    }

    pub fn under(root: PathBuf) -> Self {
        Self {
            data: root.join("data"),
            cache: root.join("cache"),
            state: root,
        }
    }

    pub fn workspace_state(&self, id: &str) -> PathBuf {
        self.state.join("workspaces").join(id)
    }

    pub fn default_workspace_root(&self, id: &str) -> PathBuf {
        self.state.join("roots").join(id)
    }

    pub fn mise_bin(&self) -> PathBuf {
        self.data.join("bin").join("mise")
    }
}
