# ghrs

Converge GitHub repository settings to a declaration file.

ghrs reads a `.ghrs.toml` that declares the desired settings of one repository, compares it with the current state on GitHub, and applies the difference.
It covers repository settings, GitHub Actions permissions, rulesets, collaborators, and teams.

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
`import` reads no declaration file, so it skips `[meta]`, and it does not write `[meta]` either.

## Commands

| Command | Description |
| --- | --- |
| `ghrs plan` | Show the difference between the declaration file and the current state |
| `ghrs apply` | Apply the difference to GitHub |
| `ghrs import` | Export the current settings in `.ghrs.toml` format |
| `ghrs drift` | Report only whether there is any difference |
| `ghrs schema` | Write the JSON Schema: key meanings, types, and allowed values |
| `ghrs explain [KEY]` | Show what keys mean, and their names in the GitHub API |
| `ghrs completion <SHELL>` | Write a shell completion script to standard output |

Common options for `plan` and `apply`:

| Option | Description |
| --- | --- |
| `--repo OWNER/NAME` | Target repository |
| `--config PATH` | Declaration file (default: `.ghrs.toml`) |
| `--only RESOURCE,...` | Limit to `repository`, `actions`, `ruleset`, `collaborator`, `team` |
| `--prune` | Delete rulesets, collaborators, and teams that are not declared |
| `--strict` | Fail when a setting ghrs knows is neither written nor listed in `meta.unmanaged` |
| `--verbose` | Also show unchanged fields |
| `--json` | Output JSON |

`apply` additionally takes `--yes` to skip the confirmation prompt, and `--no-fail-fast` to continue with independent steps after an error.
When a change weakens protection, `apply` asks you to type the repository name instead of `yes`.
For an existing ruleset, that covers deleting it, disabling it, changing its target, narrowing its conditions, removing a rule, widening bypass actors, and relaxing parameters such as lowering the required approvals or removing a required status check.
Without a terminal, `apply` refuses to proceed unless `--yes` is given.

`drift --exit-code` exits with status 2 when there is a difference, which is useful for scheduled checks in CI.
`drift` also takes `--strict`.
Like `plan` without `--prune`, `drift` does not count undeclared rulesets, collaborators, and teams as a difference.

## JSON Schema

`ghrs schema` writes the JSON Schema of the declaration file, which editors and coding agents can use to find the allowed keys and values.
It is also committed as [`schema/ghrs.schema.json`](schema/ghrs.schema.json), which `import` references for its own version in the form [taplo](https://taplo.tamasfe.dev/) reads:

```toml
#:schema https://raw.githubusercontent.com/takumin/ghrs/v<VERSION>/schema/ghrs.schema.json
# Take key meanings from `ghrs explain`, not from key names or the GitHub API.
```

`--schema PATH` changes the reference, and `--no-schema` drops these lines.

```sh
ghrs schema --out ghrs.schema.json
ghrs import --schema ./ghrs.schema.json --out .ghrs.toml
```

`ghrs explain` reads the same schema without a JSON parser: it lists every key, explains one key, or explains each key of a file.

```sh
ghrs explain
ghrs explain ruleset.main.rules.pull_request
ghrs explain --config .ghrs.toml
```

Rulesets follow GitHub's REST API, and the schema ties them to [its description](https://github.com/github/rest-api-description) at the commit ghrs was checked against.
`x-github-source` names the schema there that a type or rule follows, `x-github-api-name` gives the API's name for a key ghrs names differently or writes by name, and `x-github-observed` marks a key the API returns but does not document.
Validation errors under `rules` carry the same reference.

## Declaration file

```toml
# Optional. Pins the target repository instead of detecting it from git remote.
[meta]
owner = "takumin"
repo = "ghrs"
# Settings left unchanged on purpose, so plan does not report them as unwritten.
unmanaged = ["repository.homepage"]

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

# GitHub Actions permissions. Only the fields written here are compared, too.
[actions]
allowed_actions = "selected"          # "all", "local_only", or "selected"
sha_pinning_required = true           # actions must be pinned to a full commit SHA
default_workflow_permissions = "read" # GITHUB_TOKEN permissions: "read" or "write"
can_approve_pull_request_reviews = false
# fork PRs needing approval to run workflows (public repositories only):
# "all_external_contributors", "first_time_contributors", or "first_time_contributors_new_to_github"
fork_pr_contributor_approval = "all_external_contributors"

[actions.selected_actions]            # requires allowed_actions = "selected"
github_owned_allowed = true
verified_allowed = false
patterns_allowed = ["aquaproj/*", "reviewdog/*"]

[actions.oidc_subject_claim]          # the template of the OIDC token's sub claim
use_default = false
use_immutable_subject = true
include_claim_keys = ["repo", "context"] # requires use_default = false

# Each ruleset is a table keyed by its name.
[ruleset.main]
target = "branch"        # "branch" or "tag"
enforcement = "active"   # "active" or "disabled"
bypass_actors = [
  # type: integration, organization_admin, repository_role, team, deploy_key
  # mode: always (default), pull_request, exempt
  { type = "integration", name = "renovate", mode = "pull_request" },
  { type = "repository_role", name = "admin" },
]

[ruleset.main.conditions]
include = ["~DEFAULT_BRANCH"]
exclude = []

# Rules not listed here keep their current state; `false` disables a rule.
[ruleset.main.rules]
restrict_deletions = true   # only bypass actors can delete the branch
block_force_pushes = true

[ruleset.main.rules.pull_request]
required_approving_review_count = 1

[ruleset.main.rules.required_status_checks]
require_up_to_date = true   # the branch must be up to date before merging
checks = [{ context = "test" }, { context = "build", integration = "github-actions" }]

[collaborator.octocat]   # keyed by login
permission = "push"      # pull, triage, push, maintain, admin

[team.maintainers]       # keyed by slug
permission = "maintain"
```

Settings ghrs knows but the file leaves unwritten keep their current values: fields of `[repository]` and `[actions]`, rules a ruleset can have (enabled or disabled), and parameters of an enabled rule, including every parameter of a rule written as `true`.
`plan`, `apply`, and `drift` list them in a warning (`undeclared` with `--json`), naming a table as a whole when none of its keys is written.
List the ones to leave unchanged on purpose in `meta.unmanaged`, as key paths; a table covers every key in it.
An entry that is not a key path, or covers no unwritten setting, is an error.
`--strict` turns each remaining unwritten setting into an error, which catches a setting a newer ghrs adds before it goes unmanaged without notice.
`import` writes every setting GitHub returns, and writes each rule the ruleset can have but does not enable as `false`.

Actors are written by name (App slug, team slug, user login, role name), and ghrs resolves them to IDs.
When running as a GitHub App, `integration = "self"` refers to that App.

Rules and parameters take GitHub's REST API names, except where those read ambiguously.
`creation`, `update`, `deletion`, and `non_fast_forward` are written `restrict_creations`, `restrict_updates`, `restrict_deletions`, and `block_force_pushes` as in the web UI, and `strict_required_status_checks_policy` is written `require_up_to_date`.
The JSON Schema gives the API's name in `x-github-api-name`.

Rule parameters not written in a rule's table keep their current values.
Rules, parameters, and bypass actor types that ghrs does not know yet (for example, ones GitHub added recently) cannot be declared; `import` leaves them out with a warning, and `plan` keeps them at their current values instead of removing them.
The exception is a parameter inside an element of an array (for example, an entry of `required_reviewers`): the element is sent as a whole, so `plan` shows its removal.

Unknown keys are rejected, including rule names and rule parameters, so a misspelled key fails instead of being silently ignored.
The error names the key's path and suggests the closest allowed key.
The declaration is validated before any API call, and settings that GitHub would reject with an unexplained 422 (for example, organization-only rules in a personal repository) are reported up front.
A new rule missing a required parameter (for example, `merge_queue = true`) is reported the same way once the current rulesets are read, before any change is made.
With `--json`, these errors are written to standard output as structured diagnostics with a code, a path, and a suggested fix.

Rulesets inherited from the organization are not managed.

`[actions]` reads and writes the settings on the repository's Actions settings page.
Like `[repository]`, it compares only the fields written, and `plan` makes no API calls for it when it is empty.
It also covers the fork pull request approval policy, which private repositories do not have, and the OIDC `sub` claim template.
Changes that widen what workflows can do, such as moving `allowed_actions` from `selected` to `all`, granting `write` to `GITHUB_TOKEN`, adding to `patterns_allowed`, letting more fork pull requests run without approval, or turning off `use_immutable_subject`, count as weakening protection.
When the owning organization's policy fixes a setting, GitHub rejects the change with a 409.

ghrs assumes the GitHub Free plan.
It rejects what needs a paid plan: rulesets on a private repository, `enforcement = "evaluate"`, `target = "push"`, and `visibility = "internal"`.

## Authentication

Reading bypass actors and changing settings both require admin permission on the repository.
Credentials are resolved in the following order.

1. `--app-id` with `--private-key`, or `--token`
2. `GHRS_APP_ID` with `GHRS_PRIVATE_KEY` (PEM or base64 PEM) or `GHRS_PRIVATE_KEY_PATH`
3. `$XDG_CONFIG_HOME/ghrs/config.toml` (`~/.config/ghrs/config.toml` when `XDG_CONFIG_HOME` is not set)
4. `GITHUB_TOKEN`
5. `gh auth token`

The config file holds either an App or a token, not both.

```toml
[app]
id = 123456
private_key_path = "~/.config/ghrs/app.pem"
```

```toml
token = "op://Private/GitHub/token"
```

A GitHub App needs the Repository → Administration: Read and write permission and must be installed on the target repository.
Resolving team names also needs Organization → Members: Read (for a token, membership in the Organization).
Private keys and tokens also accept a 1Password secret reference (`op://vault/item/field`), which is read with `op read`.

## Development

The toolchain is managed with [aqua](https://aquaproj.github.io/) and [Task](https://taskfile.dev/).

```sh
aqua install
task          # format, schema, lint, dependency policy check, test, doc, build
task audit    # the policy check and the RustSec advisory scan
task test
task build:debug    # build dist/ghrs-debug-<target> and its sha256sum
task build:release  # the same with the release profile (BUILD_TARGET picks the target)
task schema   # regenerate schema/ghrs.schema.json (CI does this on pull requests)
```

## License

[Apache-2.0](LICENSE)
