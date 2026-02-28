use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Classifies user input as either a local path or a git URL
#[derive(Debug)]
pub enum InputKind {
    LocalPath(String),
    GitUrl { url: String, name: String },
}

/// Classify an input string as a local path or git URL
pub fn classify_input(input: &str) -> InputKind {
    if is_git_url(input) {
        let url = normalize_url(input);
        let name = extract_repo_name(&url);
        InputKind::GitUrl { url, name }
    } else {
        InputKind::LocalPath(input.to_string())
    }
}

/// Check if a string looks like a git URL
fn is_git_url(s: &str) -> bool {
    // Explicit schemes
    if s.starts_with("https://") || s.starts_with("http://") || s.starts_with("git://") {
        return true;
    }
    // SSH style: git@github.com:user/repo
    if s.starts_with("git@") && s.contains(':') {
        return true;
    }
    // Bare domain shortcuts: github.com/user/repo, gitlab.com/user/repo
    let known_hosts = ["github.com/", "gitlab.com/", "bitbucket.org/", "codeberg.org/"];
    for host in &known_hosts {
        if s.starts_with(host) {
            return true;
        }
    }
    false
}

/// Normalize a URL to a full https:// form
fn normalize_url(s: &str) -> String {
    // Already has a scheme
    if s.starts_with("https://") || s.starts_with("http://") || s.starts_with("git://") {
        return s.to_string();
    }
    // SSH: git@github.com:user/repo → https://github.com/user/repo
    if s.starts_with("git@") {
        let rest = &s[4..]; // skip "git@"
        if let Some((host, path)) = rest.split_once(':') {
            return format!("https://{host}/{path}");
        }
    }
    // Bare domain: github.com/user/repo → https://github.com/user/repo
    format!("https://{s}")
}

/// Extract repository name from a URL (last path segment, lowercase, strip .git)
fn extract_repo_name(url: &str) -> String {
    let path = url
        .trim_end_matches('/')
        .rsplit_once('/')
        .map(|(_, name)| name)
        .unwrap_or("project");

    path.trim_end_matches(".git").to_lowercase()
}

/// Clone a git repo (shallow) to the target directory.
/// If the target already has a `.git/` directory, does a pull instead.
pub fn clone(url: &str, target: &Path) -> Result<()> {
    if target.join(".git").is_dir() {
        // Already cloned — update
        tracing::info!(url, path = %target.display(), "pulling existing clone");
        let status = Command::new("git")
            .args(["pull", "--ff-only"])
            .current_dir(target)
            .status()
            .context("running git pull")?;

        if !status.success() {
            anyhow::bail!("git pull failed (exit {})", status.code().unwrap_or(-1));
        }
    } else {
        // Fresh shallow clone
        tracing::info!(url, path = %target.display(), "cloning repository");
        std::fs::create_dir_all(target)
            .with_context(|| format!("creating clone target: {}", target.display()))?;

        let status = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                url,
                &target.to_string_lossy(),
            ])
            .status()
            .context("running git clone")?;

        if !status.success() {
            anyhow::bail!("git clone failed (exit {})", status.code().unwrap_or(-1));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_git_url_https() {
        assert!(is_git_url("https://github.com/user/repo"));
        assert!(is_git_url("https://gitlab.com/user/repo.git"));
    }

    #[test]
    fn test_is_git_url_ssh() {
        assert!(is_git_url("git@github.com:user/repo.git"));
    }

    #[test]
    fn test_is_git_url_bare() {
        assert!(is_git_url("github.com/user/repo"));
        assert!(is_git_url("gitlab.com/user/repo"));
    }

    #[test]
    fn test_is_not_git_url() {
        assert!(!is_git_url("/home/user/project"));
        assert!(!is_git_url("./local-project"));
        assert!(!is_git_url("C:\\Users\\kord\\project"));
        assert!(!is_git_url("my-project"));
    }

    #[test]
    fn test_normalize_url_already_https() {
        assert_eq!(
            normalize_url("https://github.com/user/repo"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn test_normalize_url_ssh() {
        assert_eq!(
            normalize_url("git@github.com:user/repo.git"),
            "https://github.com/user/repo.git"
        );
    }

    #[test]
    fn test_normalize_url_bare() {
        assert_eq!(
            normalize_url("github.com/user/repo"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn test_extract_repo_name() {
        assert_eq!(
            extract_repo_name("https://github.com/ItzCrazyKns/Perplexica"),
            "perplexica"
        );
        assert_eq!(
            extract_repo_name("https://github.com/user/My-Repo.git"),
            "my-repo"
        );
        assert_eq!(
            extract_repo_name("https://github.com/user/REPO/"),
            "repo"
        );
    }

    #[test]
    fn test_classify_input_local() {
        match classify_input("/home/user/project") {
            InputKind::LocalPath(p) => assert_eq!(p, "/home/user/project"),
            _ => panic!("expected LocalPath"),
        }
    }

    #[test]
    fn test_classify_input_git() {
        match classify_input("https://github.com/ItzCrazyKns/Perplexica") {
            InputKind::GitUrl { url, name } => {
                assert_eq!(url, "https://github.com/ItzCrazyKns/Perplexica");
                assert_eq!(name, "perplexica");
            }
            _ => panic!("expected GitUrl"),
        }
    }

    #[test]
    fn test_classify_input_bare_github() {
        match classify_input("github.com/user/cool-app") {
            InputKind::GitUrl { url, name } => {
                assert_eq!(url, "https://github.com/user/cool-app");
                assert_eq!(name, "cool-app");
            }
            _ => panic!("expected GitUrl"),
        }
    }
}
