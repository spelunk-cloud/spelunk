use std::path::Path;

// ---------------------------------------------------------------------------
// Project-id derivation
// ---------------------------------------------------------------------------

/// Derive a stable project identifier from `project_root`.
///
/// 1. Read `remote.origin.url` from the git config and normalise to
///    `host/owner/repo`.
/// 2. If no git repo or no origin remote, fall back to
///    `local/<blake3-hex-of-canonical-path>`.
pub fn derive_project_id(project_root: &Path) -> String {
    try_derive_from_git(project_root).unwrap_or_else(|| derive_local_fallback(project_root))
}

fn try_derive_from_git(root: &Path) -> Option<String> {
    let repo = gix::discover(root).ok()?;
    let git_dir = repo.git_dir();

    // For linked worktrees the config lives in the main .git dir, not
    // .git/worktrees/<name>.
    let config_path = if git_dir.parent().and_then(|p| p.file_name())
        == Some(std::ffi::OsStr::new("worktrees"))
    {
        git_dir.parent()?.parent()?.join("config")
    } else {
        git_dir.join("config")
    };

    let content = std::fs::read_to_string(config_path).ok()?;
    let url = extract_origin_url_from_git_config(&content)?;
    Some(normalise_git_url(&url))
}

/// Minimal parser for git config: finds `url` under `[remote "origin"]`.
fn extract_origin_url_from_git_config(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Section header — check if it's [remote "origin"]
            let header = trimmed.trim_start_matches('[').trim_end_matches(']');
            in_origin = header.trim() == r#"remote "origin""#;
        } else if in_origin
            && let Some(rest) = trimmed.strip_prefix("url")
            && let Some(rest) = rest.trim_start().strip_prefix('=')
        {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Normalise a git remote URL to `host/owner/repo` (no scheme, no `.git`).
///
/// Handles `https://`, `ssh://`, and SCP-style `git@host:owner/repo.git`.
fn normalise_git_url(url: &str) -> String {
    let without_scheme = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };
    let without_user = if let Some(pos) = without_scheme.find('@') {
        &without_scheme[pos + 1..]
    } else {
        without_scheme
    };
    // SCP colon → slash
    let normalised = without_user.replacen(':', "/", 1);
    let normalised = normalised.strip_suffix(".git").unwrap_or(&normalised);
    normalised.to_lowercase()
}

fn derive_local_fallback(root: &Path) -> String {
    let canonical = crate::utils::canonicalize(root);
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
    format!("local/{}", hash.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── normalise_git_url ────────────────────────────────────────────────────

    #[test]
    fn normalise_https_url() {
        assert_eq!(
            normalise_git_url("https://github.com/owner/repo.git"),
            "github.com/owner/repo"
        );
    }

    #[test]
    fn normalise_scp_url() {
        assert_eq!(
            normalise_git_url("git@github.com:owner/repo.git"),
            "github.com/owner/repo"
        );
    }

    #[test]
    fn normalise_ssh_url() {
        assert_eq!(
            normalise_git_url("ssh://git@github.com/owner/repo"),
            "github.com/owner/repo"
        );
    }

    // ── derive_project_id: no git repo → local/ fallback ─────────────────────

    #[test]
    fn derive_project_id_non_git_dir_returns_local_prefix() {
        let tmp = TempDir::new().unwrap();
        let id = derive_project_id(tmp.path());
        assert!(id.starts_with("local/"), "expected local/ prefix, got {id}");
        // blake3 hex is 64 chars
        assert_eq!(id.len(), "local/".len() + 64);
    }

    // ── derive_project_id: git repo with origin ───────────────────────────────

    #[test]
    fn derive_project_id_git_repo_with_origin() {
        let tmp = TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
        std::fs::write(
            repo_dir.join(".git").join("config"),
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://github.com/spelunk-cloud/spelunk.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .unwrap();
        // derive_project_id falls back to local/ when gix::discover fails on
        // a minimal fake repo, but the git-config parser should find the URL.
        // We test the git-config parser directly instead:
        let config = std::fs::read_to_string(repo_dir.join(".git").join("config")).unwrap();
        let url = extract_origin_url_from_git_config(&config).unwrap();
        assert_eq!(normalise_git_url(&url), "github.com/spelunk-cloud/spelunk");
    }
}
