//! uv-managed Python environment provisioning.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A uv-created virtualenv that hosts the Wisp kernel worker.
pub struct PythonEnv {
    pub venv: PathBuf,
}

impl PythonEnv {
    /// The app-managed environment location without creating or modifying it.
    pub fn managed(app_data: &Path) -> Self {
        Self {
            venv: app_data.join("python").join(".venv"),
        }
    }

    /// Locate `uv` on PATH (or via `UV_PATH` env).
    pub fn find_uv() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("UV_PATH") {
            return Some(PathBuf::from(p));
        }
        which::which("uv").ok()
    }

    /// Locate `node` on PATH.
    pub fn find_node() -> Option<PathBuf> {
        which::which("node").ok()
    }

    /// Locate `npm` on PATH.
    pub fn find_npm() -> Option<PathBuf> {
        which::which("npm").ok()
    }

    /// Locate `sci` (scimaster-cli) on PATH.
    pub fn find_sci() -> Option<PathBuf> {
        which::which("sci").ok()
    }

    /// Locate `pixi` on PATH (or via `PIXI_PATH` env).
    pub fn find_pixi() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("PIXI_PATH") {
            return Some(PathBuf::from(p));
        }
        which::which("pixi").ok()
    }

    /// Python interpreter inside the venv (`Scripts\python.exe` on Windows).
    pub fn python(&self) -> PathBuf {
        if cfg!(target_os = "windows") {
            self.venv.join("Scripts").join("python.exe")
        } else {
            self.venv.join("bin").join("python")
        }
    }

    /// Ensure a venv exists under `app_data/python/.venv`, create with `uv venv`,
    /// and install MCP/kernel deps from the bundled requirements file when needed.
    ///
    /// Blocks on a wheel download that can run for minutes on a slow link — only
    /// call it from the background bootstrap, never from a request path. Use
    /// [`Self::ensure_venv`] there.
    pub fn ensure(app_data: &Path) -> Result<Self> {
        let env = Self::ensure_venv(app_data)?;
        let uv = Self::find_uv()
            .ok_or_else(|| anyhow!("uv not found on PATH; install uv or set UV_PATH"))?;
        Self::install_deps(&uv, &env.python(), &env.venv)?;
        Ok(env)
    }

    /// Create the venv only, skipping the dependency install.
    ///
    /// ponytail: request paths (tool wiring, MCP bridge) need the interpreter
    /// path, not the wheels. `uv venv` is local and fast; the deps land later
    /// via the startup bootstrap's `ensure`. Anything that truly needs a
    /// third-party package fails fast on import instead of stalling the turn.
    pub fn ensure_venv(app_data: &Path) -> Result<Self> {
        let env = Self::managed(app_data);
        if env.python().exists() {
            return Ok(env);
        }
        let uv = Self::find_uv()
            .ok_or_else(|| anyhow!("uv not found on PATH; install uv or set UV_PATH"))?;
        std::fs::create_dir_all(env.venv.parent().unwrap_or(Path::new(".")))?;
        let mut cmd = Command::new(&uv);
        cmd.arg("venv").arg(&env.venv);
        wisp_tools::process::hide_console(&mut cmd);
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "uv venv failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(env)
    }

    fn install_deps(uv: &Path, python: &Path, venv: &Path) -> Result<()> {
        let Some(req) = wisp_paths::mcp_requirements_path() else {
            return Ok(());
        };
        let marker = venv.join(".wisp_deps_ok");
        if marker.is_file() {
            return Ok(());
        }
        let mut cmd = Command::new(uv);
        cmd.args(["pip", "install", "-r"])
            .arg(&req)
            .arg("--python")
            .arg(python);
        wisp_tools::process::hide_console(&mut cmd);
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "uv pip install failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        std::fs::write(&marker, b"ok")?;
        Ok(())
    }
}

/// Locate `Rscript`: PATH first, then well-known install locations, so an R
/// installed outside PATH (e.g. `D:\R-4.5.2` on Windows or a conda base env)
/// is still found (issue #651). Context-specific interpreter paths are
/// resolved by the host from persisted execution-context configuration.
pub fn find_rscript() -> Option<PathBuf> {
    if let Ok(path) = which::which("Rscript") {
        return Some(path);
    }
    rscript_common_install_candidates()
        .into_iter()
        .find(|path| path.is_file())
}

/// Candidate `Rscript` paths in well-known install locations, most preferred
/// first. Kept separate from `find_rscript` so the ordering stays testable
/// without touching the host filesystem layout.
fn rscript_common_install_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "windows")]
    {
        // C:\Program Files\R\R-<version>\bin\Rscript.exe — newest version first.
        let program_files = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        let mut names: Vec<String> = std::fs::read_dir(program_files.join("R"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().is_dir())
                    .filter_map(|entry| Some(entry.file_name().into_string().ok()?))
                    .collect()
            })
            .unwrap_or_default();
        sort_r_install_dirs_newest_first(&mut names);
        candidates.extend(names.into_iter().map(|name| {
            program_files
                .join("R")
                .join(name)
                .join("bin")
                .join("Rscript.exe")
        }));
    }
    #[cfg(not(target_os = "windows"))]
    {
        for path in [
            "/usr/local/bin/Rscript",
            "/opt/homebrew/bin/Rscript",
            "/usr/bin/Rscript",
        ] {
            candidates.push(PathBuf::from(path));
        }
        if let Some(home) = std::env::var_os("HOME") {
            for dir in ["miniconda3", "anaconda3", "miniforge3", "mambaforge"] {
                candidates.push(Path::new(&home).join(dir).join("bin").join("Rscript"));
            }
        }
        candidates.push(PathBuf::from("/opt/conda/bin/Rscript"));
    }
    candidates
}

/// Order `R-x.y.z` install directory names newest-first. Pure string parsing
/// so it can be unit-tested on any host.
#[cfg(any(target_os = "windows", test))]
fn sort_r_install_dirs_newest_first(names: &mut [String]) {
    names.sort_by_key(|name| std::cmp::Reverse(r_install_version_key(name)));
}

#[cfg(any(target_os = "windows", test))]
fn r_install_version_key(name: &str) -> (u64, u64, u64) {
    let version = name.strip_prefix("R-").unwrap_or(name);
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Path to the kernel worker bundled with the app (`python/kernel_worker.py`).
pub fn bundled_worker_path() -> Option<PathBuf> {
    wisp_paths::kernel_worker_path()
}

/// Path to the R worker bundled with the app (`r/kernel_worker.R`).
pub fn bundled_r_worker_path() -> Option<PathBuf> {
    wisp_paths::r_kernel_worker_path()
}

/// Path to the mock MCP server bundled with the app.
pub fn bundled_mock_mcp_path() -> Option<PathBuf> {
    wisp_paths::python_dir()
        .map(|d| d.join("mock_mcp_server.py"))
        .filter(|p| p.is_file())
}

/// Resolve a script path, remapping known names to bundled resources when missing.
pub fn resolve_bundled_script(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_file() {
        return p;
    }
    match p.file_name().and_then(|n| n.to_str()) {
        Some("kernel_worker.py") => bundled_worker_path().unwrap_or(p),
        Some("kernel_worker.R") => bundled_r_worker_path().unwrap_or(p),
        Some("mock_mcp_server.py") => bundled_mock_mcp_path().unwrap_or(p),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ensure_venv` must never shell out to `uv pip install` — it runs on the
    /// chat send path (#477). A bogus `uv` path proves no install was attempted.
    #[test]
    fn install_deps_short_circuits_on_marker() {
        let venv = std::env::temp_dir().join(format!("wisp-env-{}", std::process::id()));
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join(".wisp_deps_ok"), b"ok").unwrap();
        let nope = Path::new("/nonexistent/uv");
        assert!(PythonEnv::install_deps(nope, Path::new("/nonexistent/python"), &venv).is_ok());
        std::fs::remove_file(venv.join(".wisp_deps_ok")).unwrap();
        assert!(PythonEnv::install_deps(nope, Path::new("/nonexistent/python"), &venv).is_err());
        let _ = std::fs::remove_dir_all(&venv);
    }

    #[test]
    fn r_install_dirs_sort_newest_version_first() {
        let mut names = vec![
            "R-4.3.2".to_string(),
            "R-4.10.0".to_string(),
            "R-3.6.3".to_string(),
            "R-4.5.2".to_string(),
            "unrelated".to_string(),
        ];
        sort_r_install_dirs_newest_first(&mut names);
        assert_eq!(
            names,
            ["R-4.10.0", "R-4.5.2", "R-4.3.2", "R-3.6.3", "unrelated"]
        );
    }

    #[test]
    fn rscript_candidates_are_absolute_and_prefer_standard_locations() {
        let candidates = rscript_common_install_candidates();
        assert!(!candidates.is_empty());
        assert!(candidates.iter().all(|path| path.is_absolute()));
        #[cfg(not(target_os = "windows"))]
        assert_eq!(candidates[0], PathBuf::from("/usr/local/bin/Rscript"));
    }
}
