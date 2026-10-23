//! Specifying the target repository, and detecting it from the git remote.

use anyhow::{Context, Result, bail};
use std::fmt;
use std::process::Command;
use std::str::FromStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl FromStr for RepoRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once('/') {
            Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
                Ok(Self {
                    owner: owner.to_string(),
                    name: name.to_string(),
                })
            }
            _ => Err(format!("not in owner/name form: {s}")),
        }
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

pub fn resolve(repo: Option<RepoRef>) -> Result<RepoRef> {
    if let Some(repo) = repo {
        return Ok(repo);
    }

    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .context("--repo is not given, and the git command could not be run")?;
    if !output.status.success() {
        bail!("--repo is not given, and git remote origin could not be read");
    }

    let url =
        String::from_utf8(output.stdout).context("output of git remote get-url is not UTF-8")?;
    let url = url.trim();
    parse_remote(url).with_context(|| format!("origin is not a GitHub repository: {url}"))
}

/// Extracts owner/name from a GitHub remote URL.
///
/// Accepted forms: scp-like `[user@]github.com:o/r`, and
/// `<scheme>://[userinfo@]github.com[:port]/o/r` for `https`, `http`, `ssh` and `git`. The host
/// is matched case-insensitively, and `ssh.github.com` (SSH over port 443) is accepted as well.
/// In all of them, the trailing `.git` and `/` are optional.
fn parse_remote(url: &str) -> Option<RepoRef> {
    let (host, path) = match ["https://", "http://", "ssh://", "git://"]
        .iter()
        .find_map(|scheme| url.strip_prefix(scheme))
    {
        Some(rest) => {
            let (authority, path) = rest.split_once('/')?;
            // User info may itself contain `@` (e.g. in a password), so take the last one
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            let host = host.split_once(':').map_or(host, |(host, _port)| host);
            (host, path)
        }
        None if !url.contains("://") => {
            let (authority, path) = url.split_once(':')?;
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            (host, path)
        }
        None => return None,
    };
    if !["github.com", "ssh.github.com"]
        .iter()
        .any(|h| host.eq_ignore_ascii_case(h))
    {
        return None;
    }

    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    path.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, name: &str) -> Option<RepoRef> {
        Some(RepoRef {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    #[test]
    fn parses_github_remotes() {
        for url in [
            "https://github.com/takumin/ghrs",
            "https://github.com/takumin/ghrs.git",
            "https://github.com/takumin/ghrs/",
            "https://token@github.com/takumin/ghrs.git",
            "git@github.com:takumin/ghrs.git",
            "ssh://git@github.com/takumin/ghrs.git",
            "ssh://git@github.com:22/takumin/ghrs.git",
            "ssh://git@ssh.github.com:443/takumin/ghrs.git",
            "org-123@github.com:takumin/ghrs.git",
            "github.com:takumin/ghrs",
            "https://GitHub.com/takumin/ghrs",
            "https://user:p@ss@github.com/takumin/ghrs",
        ] {
            assert_eq!(parse_remote(url), repo("takumin", "ghrs"), "{url}");
        }
    }

    #[test]
    fn rejects_non_github_remotes() {
        for url in [
            "https://gitlab.com/takumin/ghrs.git",
            "git@gitlab.com:takumin/ghrs.git",
            "https://github.com/takumin",
            "https://github.com/takumin/ghrs/tree/main",
            "/path/to/local/repo",
            "https://github.com.evil.example/takumin/ghrs",
            "file://github.com/takumin/ghrs",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
    }

    #[test]
    fn rejects_malformed_repo_arg() {
        for arg in ["ghrs", "/ghrs", "takumin/", "a/b/c"] {
            assert!(arg.parse::<RepoRef>().is_err(), "{arg}");
        }
    }
}
