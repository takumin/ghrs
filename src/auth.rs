//! Authentication. Builds the API client from a GitHub App installation token or a
//! PAT.
//!
//! Wherever a private key or a token is accepted, a 1Password secret reference
//! (`op://vault/item/field`) can be given instead; it is read with the `op` CLI.

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::AuthArgs;
use crate::github::GitHub;
use crate::repo::RepoRef;

pub enum Credential {
    App { id: u64, key: Vec<u8> },
    Token(String),
}

impl Credential {
    pub fn is_app(&self) -> bool {
        matches!(self, Credential::App { .. })
    }
}

/// The App itself when running as an App. Used to resolve `integration = "self"`.
pub struct SelfApp {
    pub id: u64,
    /// The App's installation on the target repository. `pull_request` actors refer to Apps by
    /// installation ID rather than App ID
    pub installation_id: u64,
}

/// `$XDG_CONFIG_HOME/ghrs/config.toml` (`~/.config/ghrs/config.toml` by default)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserConfig {
    app: Option<AppConfig>,
    token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppConfig {
    id: u64,
    private_key_path: PathBuf,
}

/// Explicit flags take precedence over environment variables and the config file.
pub fn resolve(args: &AuthArgs) -> Result<Credential> {
    if let (Some(id), Some(path)) = (args.app_id, &args.private_key) {
        return Ok(Credential::App {
            id,
            key: read_key(path)?,
        });
    }
    if let Some(token) = &args.token {
        // An empty value usually comes from an unset variable (--token "$TOKEN"). Falling through
        // would quietly use another credential
        if token.trim().is_empty() {
            bail!("--token is empty");
        }
        return Ok(Credential::Token(read_token(token)?));
    }

    if let Some(id) = nonempty(std::env::var("GHRS_APP_ID").ok()) {
        let id = id
            .parse()
            .with_context(|| format!("GHRS_APP_ID \"{id}\" is not a number"))?;
        let key = match (
            nonempty(std::env::var("GHRS_PRIVATE_KEY").ok()),
            nonempty(std::env::var("GHRS_PRIVATE_KEY_PATH").ok()),
        ) {
            (Some(key), _) => {
                decode_key(&read_secret(&key)?).context("cannot read GHRS_PRIVATE_KEY")?
            }
            (None, Some(path)) => read_key(&expand_home(Path::new(&path)))?,
            (None, None) => bail!(
                "GHRS_APP_ID is set but no private key is given; \
                set GHRS_PRIVATE_KEY or GHRS_PRIVATE_KEY_PATH"
            ),
        };
        return Ok(Credential::App { id, key });
    }
    // Falling through would use another credential, which fails much later for lack of permission
    for name in ["GHRS_PRIVATE_KEY", "GHRS_PRIVATE_KEY_PATH"] {
        if nonempty(std::env::var(name).ok()).is_some() {
            bail!("{name} is set but GHRS_APP_ID is not; set GHRS_APP_ID as well");
        }
    }

    if let Some(path) = user_config_path()
        && path.exists()
    {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let config: UserConfig =
            toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))?;
        match (config.app, nonempty(config.token)) {
            (Some(_), Some(_)) => bail!(
                "{} sets both [app] and token; keep only one",
                path.display()
            ),
            (Some(app), None) => {
                return Ok(Credential::App {
                    id: app.id,
                    key: read_key(&expand_home(&app.private_key_path))?,
                });
            }
            (None, Some(token)) => return Ok(Credential::Token(read_token(&token)?)),
            (None, None) => {}
        }
    }

    if let Some(token) = nonempty(std::env::var("GITHUB_TOKEN").ok()) {
        return Ok(Credential::Token(read_token(&token)?));
    }
    gh_token().map(Credential::Token)
}

/// Builds a client that can operate on the target repository. For an App, looks up the
/// installation and issues an installation token.
pub async fn connect(
    credential: Credential,
    target: &RepoRef,
) -> Result<(GitHub, Option<SelfApp>)> {
    let (id, key) = match credential {
        Credential::Token(token) => return Ok((GitHub::new(token_client(&token)?), None)),
        Credential::App { id, key } => (id, key),
    };

    // The re-export from octocrab, so the key type always matches the one `app` takes
    let key = octocrab::EncodingKey::from_rsa_pem(&key)
        .context("private key is not a valid RSA PEM")?;
    let mut app = GitHub::new(octocrab::Octocrab::builder().app(id.into(), key).build()?);

    let me = app.get("/app").await.with_context(|| {
        format!(
            "cannot authenticate as App ID {id}; check that the App ID and the private key \
            belong together"
        )
    })?;
    let slug = me["slug"].as_str().unwrap_or_default().to_string();

    let Some(installation) = app
        .get_optional(&format!("/repos/{target}/installation"))
        .await?
    else {
        bail!(
            "App \"{slug}\" is not installed on {target}\n  \
            -> install it from https://github.com/apps/{slug}/installations/new \
            and include {target} in its repository access"
        );
    };
    let Some(installation_id) = installation["id"].as_u64() else {
        bail!("response of GET /repos/{target}/installation has no id");
    };

    // Issue a token scoped to the target repository only. Permissions are not narrowed, so the
    // permissions granted to the installation appear as-is in the response's permissions. Missing
    // ones are checked there.
    let body = json!({ "repositories": [target.name] });
    let issued = app
        .send(
            crate::github::Method::Post,
            &format!("/app/installations/{installation_id}/access_tokens"),
            Some(&body),
        )
        .await?;
    check_permissions(&slug, &issued["permissions"])?;
    let Some(token) = issued["token"].as_str() else {
        bail!("installation access token response has no token");
    };

    let mut gh = GitHub::new(token_client(token)?);
    gh.calls = app.calls;
    Ok((
        gh,
        Some(SelfApp {
            id,
            installation_id,
        }),
    ))
}

/// Reading and writing rulesets and repository settings requires Administration: write.
/// bypass_actors is only returned with admin permission, so the same applies to plan / drift
/// (observed).
fn check_permissions(slug: &str, permissions: &Json) -> Result<()> {
    if permissions["administration"] != "write" {
        bail!(
            "App \"{slug}\" lacks the Repository -> Administration: Read and write permission\n  \
            -> add the permission in the App settings, then approve it on the installation"
        );
    }
    Ok(())
}

fn token_client(token: &str) -> Result<octocrab::Octocrab> {
    Ok(octocrab::Octocrab::builder()
        .personal_token(token.to_string())
        .build()?)
}

/// Reads the PEM (or base64 PEM) in a file or behind a 1Password secret reference.
fn read_key(path: &Path) -> Result<Vec<u8>> {
    if let Some(reference) = path.to_str().filter(|p| is_secret_ref(p)) {
        return decode_key(&read_secret(reference)?)
            .with_context(|| format!("cannot read private key {reference}"));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read private key {}", path.display()))?;
    decode_key(&text).with_context(|| format!("cannot read private key {}", path.display()))
}

fn read_token(value: &str) -> Result<String> {
    let token = read_secret(value)?.trim().to_string();
    if token.is_empty() {
        bail!("token {value} is empty");
    }
    Ok(token)
}

fn is_secret_ref(value: &str) -> bool {
    value.starts_with("op://")
}

/// Resolves a 1Password secret reference with `op read`; any other value is returned as-is.
fn read_secret(value: &str) -> Result<String> {
    if !is_secret_ref(value) {
        return Ok(value.to_string());
    }
    let output = Command::new("op")
        .args(["read", "--no-newline", value])
        .output()
        .with_context(|| format!("cannot run the op command to read {value}"))?;
    if !output.status.success() {
        bail!(
            "op read {value} failed: {}\n  \
            -> check the reference and that op is signed in (op signin)",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).with_context(|| format!("value of {value} is not UTF-8"))
}

/// Accepts either raw PEM or base64-encoded PEM. The latter is for CI secrets that cannot hold
/// values containing newlines.
///
/// PEM may come after other text, such as the `Bag Attributes` openssl writes when exporting a
/// key, and may be on one line with its newlines written as `\n`, as JSON and env files hold it.
fn decode_key(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    if let Some(start) = value.find("-----BEGIN") {
        let pem = &value[start..];
        if !pem.contains('\n') && pem.contains("\\n") {
            return Ok(pem.replace("\\n", "\n").into_bytes());
        }
        return Ok(pem.as_bytes().to_vec());
    }
    let compact: String = value.split_whitespace().collect();
    base64::engine::general_purpose::STANDARD
        .decode(compact)
        .context("value is neither PEM nor base64")
}

fn user_config_path() -> Option<PathBuf> {
    let base = nonempty(std::env::var("XDG_CONFIG_HOME").ok())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
    Some(base.join("ghrs").join("config.toml"))
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => Path::new(&home).join(rest),
        _ => path.to_path_buf(),
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Credentials from the `gh` CLI. Cannot issue installation tokens, so this is for local checks.
fn gh_token() -> Result<String> {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("no credentials found, and the gh command could not be run")?;
    if !output.status.success() {
        bail!(
            "no credentials found; set up a GitHub App (--app-id / GHRS_APP_ID) or \
            GITHUB_TOKEN"
        );
    }
    let token = String::from_utf8(output.stdout)
        .context("output of gh auth token is not UTF-8")?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("gh auth token returned an empty token");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_pem_and_base64_keys() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\n";
        assert_eq!(decode_key(pem).unwrap(), pem.trim().as_bytes());

        let encoded = base64::engine::general_purpose::STANDARD.encode(pem);
        // Also tolerate line wrapping that tends to creep in when pasting into a secret
        let wrapped = format!("{}\n{}", &encoded[..20], &encoded[20..]);
        assert_eq!(decode_key(&wrapped).unwrap(), pem.as_bytes());

        assert!(decode_key("not a key!").is_err());

        let escaped = pem.trim().replace('\n', "\\n");
        assert_eq!(decode_key(&escaped).unwrap(), pem.trim().as_bytes());
        let exported = format!("Bag Attributes\n    localKeyID: 01 02\n{pem}");
        assert_eq!(decode_key(&exported).unwrap(), pem.trim().as_bytes());
    }

    #[test]
    fn rejects_empty_token_flag() {
        let args = AuthArgs {
            app_id: None,
            private_key: None,
            token: Some(" ".to_string()),
        };
        let Err(err) = resolve(&args) else {
            panic!("accepted an empty --token");
        };
        assert!(err.to_string().contains("--token"), "{err}");
    }

    #[test]
    fn passes_through_non_reference_secrets() {
        assert!(is_secret_ref("op://vault/item/field"));
        assert!(!is_secret_ref("ghp_xxx"));
        assert!(!is_secret_ref("/path/to/op://x"));
        assert_eq!(read_secret("ghp_xxx").unwrap(), "ghp_xxx");
        assert_eq!(read_token(" ghp_xxx\n").unwrap(), "ghp_xxx");
    }

    #[test]
    fn requires_administration_write() {
        assert!(check_permissions("x", &json!({ "administration": "write" })).is_ok());
        assert!(check_permissions("x", &json!({ "administration": "read" })).is_err());
        assert!(check_permissions("x", &json!({ "metadata": "read" })).is_err());
    }

    #[test]
    fn expands_home_prefix() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        assert_eq!(
            expand_home(Path::new("~/k.pem")),
            Path::new(&home).join("k.pem")
        );
        assert_eq!(expand_home(Path::new("/k.pem")), Path::new("/k.pem"));
    }
}
