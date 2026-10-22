# ghrs

Converge GitHub repository settings to a declaration file.

ghrs reads a `.ghrs.toml` that declares the desired settings of one repository, compares it with the current state on GitHub, and applies the difference.
It covers repository settings, rulesets, collaborators, and teams.

## Installation

Download a binary for your platform from the [releases](https://github.com/takumin/ghrs/releases) page.
Linux binaries are published for `x86_64`, `i686`, `aarch64`, and `armv7`, each in a glibc and a musl variant.

```sh
curl -fsSLO https://github.com/takumin/ghrs/releases/latest/download/ghrs-x86_64-unknown-linux-musl
install -m 0755 ghrs-x86_64-unknown-linux-musl ~/.local/bin/ghrs
```

Every release asset comes with a `SHA256SUMS` entry, a build provenance attestation, and a cosign bundle (`*.sigstore.json`).

```sh
gh attestation verify ghrs-x86_64-unknown-linux-musl --repo takumin/ghrs

cosign verify-blob ghrs-x86_64-unknown-linux-musl \
  --bundle ghrs-x86_64-unknown-linux-musl.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/takumin/ghrs/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

To build from source, use Rust 1.99 or later.

```sh
cargo install --locked --git https://github.com/takumin/ghrs
```

## Quick start

```sh
# Export the current settings as a starting point
ghrs import --out .ghrs.toml

# Edit .ghrs.toml, then review the difference
ghrs plan

# Apply it
ghrs apply
```

The target repository is taken from `--repo OWNER/NAME`, then from `[meta]` in the declaration file, then from the git remote of the current directory.

## Commands

| Command | Description |
| --- | --- |
| `ghrs plan` | Show the difference between the declaration file and the current state |
| `ghrs apply` | Apply the difference to GitHub |
| `ghrs import` | Export the current settings in `.ghrs.toml` format |
| `ghrs drift` | Report only whether there is any difference |
| `ghrs completion <SHELL>` | Write a shell completion script to standard output |

Common options for `plan` and `apply`:

| Option | Description |
| --- | --- |
| `--repo OWNER/NAME` | Target repository |
| `--config PATH` | Declaration file (default: `.ghrs.toml`) |
| `--only RESOURCE,...` | Limit to `repository`, `ruleset`, `collaborator`, `team` |
| `--prune` | Delete rulesets, collaborators, and teams that are not declared |
| `--verbose` | Also show unchanged fields |
| `--json` | Output JSON |

`apply` additionally takes `--yes` to skip the confirmation prompt, and `--no-fail-fast` to continue with independent steps after an error.
When a change weakens protection, `apply` asks you to type the repository name instead of `yes`.
Without a terminal, `apply` refuses to proceed unless `--yes` is given.

`drift --exit-code` exits with status 2 when there is a difference, which is useful for scheduled checks in CI.

## Declaration file

```toml
# Optional. Pins the target repository instead of detecting it from git remote.
[meta]
owner = "takumin"
repo = "ghrs"

# Only the fields written here are compared. Omitted fields are left untouched.
[repository]
description = "Converge GitHub repository settings to a declaration file"
visibility = "public"
has_wiki = false
allow_merge_commit = false
allow_squash_merge = true
allow_rebase_merge = false
delete_branch_on_merge = true
default_branch = "main"
topics = ["github", "cli"]

[[ruleset]]
name = "main"
target = "branch"        # "branch" or "tag"
enforcement = "active"   # "active" or "disabled"

[ruleset.conditions]
include = ["~DEFAULT_BRANCH"]
exclude = []

[[ruleset.bypass_actors]]
type = "integration"     # integration, organization_admin, repository_role, team, deploy_key
name = "renovate"
mode = "pull_request"    # always (default), pull_request, exempt

[[ruleset.bypass_actors]]
type = "repository_role"
name = "admin"

# Rules not listed here are disabled.
[ruleset.rules]
deletion = true
non_fast_forward = true

[ruleset.rules.pull_request]
required_approving_review_count = 1

[ruleset.rules.required_status_checks]
strict = true
checks = [{ context = "test" }, { context = "build", integration = "github-actions" }]

[[collaborator]]
login = "octocat"
permission = "push"      # pull, triage, push, maintain, admin

[[team]]
slug = "maintainers"
permission = "maintain"
```

Actors are written by name (App slug, team slug, user login, role name), and ghrs resolves them to IDs.
When running as a GitHub App, `integration = "self"` refers to that App.

Rule parameters not written in a rule's table keep their current values.
Unknown keys are rejected, so a misspelled key fails instead of being silently ignored.
The declaration is validated before any API call, and settings that GitHub would reject with an unexplained 422 (for example, organization-only rules in a personal repository) are reported up front.

Rulesets inherited from the organization are not managed.

## Authentication

Reading bypass actors and changing settings both require admin permission on the repository.
Credentials are resolved in the following order.

1. `--app-id` with `--private-key`, or `--token`
2. `GHRS_APP_ID` with `GHRS_PRIVATE_KEY` (PEM or base64 PEM) or `GHRS_PRIVATE_KEY_PATH`
3. `~/.config/ghrs/config.toml`
4. `GITHUB_TOKEN`
5. `gh auth token`

`~/.config/ghrs/config.toml` holds either an App or a token, not both.

```toml
[app]
id = 123456
private_key_path = "~/.config/ghrs/app.pem"
```

```toml
token = "op://Private/GitHub/token"
```

A GitHub App needs the Repository → Administration: Read and write permission and must be installed on the target repository.
Private keys and tokens also accept a 1Password secret reference (`op://vault/item/field`), which is read with `op read`.

## Development

The toolchain is managed with [aqua](https://aquaproj.github.io/) and [Task](https://taskfile.dev/).

```sh
aqua install
task          # format, lint, audit, test, doc, build
task test
```

## License

[Apache-2.0](LICENSE)
