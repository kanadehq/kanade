<!-- kata:agents:base:begin -->
## Shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). The matching
`CLAUDE.md` and `GEMINI.md` files are thin shims that point back
here so each tool's auto-load behaviour still finds something.
**Edit AGENTS.md, not the shims.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
- **Releases are PR-driven and tagging is automatic** — in repos that
  ship a release pipeline. Bump the version in the project's own
  manifest in a `chore/release-vX.Y.Z` PR; on merge to `main` the
  language layer's `auto-tag.yml` detects the bump, pushes the
  `vX.Y.Z` tag, and that tag is what fires `release.yml`. **Do not run
  `git tag` by hand** — the bot tag will collide and the manual push
  fails. The specifics belong to the layers shipping those two
  workflows, which are not the same layer: `kata:agents:rust:*` for
  which file holds the version and for `auto-tag.yml`,
  `kata:agents:rust-{cli,lib}:*` for what `release.yml` builds and
  publishes. A repo with no `auto-tag.yml` has no release pipeline at
  all: nothing tags, and the version field in its manifest may well
  be decoration.

### Pre-merge review

Review happens **before the pull request, on the operator's machine**,
via [magi](https://github.com/yukimemi/magi). This layer no longer
ships PR-side review bots: `claude-review.yml` and `claude.yml` were
removed from it. Their scope was
human-authored PRs — their own job-level `if:` already excluded
`chore/release-*`, `kata-apply/auto`, `apm-bump/auto` and
Renovate / Dependabot — which is exactly the set magi reviews, so
keeping them meant reviewing the same diff twice, a
`CLAUDE_CODE_OAUTH_TOKEN` secret per repository, Actions minutes on
private repos, and one trap that silently cost reviews: a PR editing
either workflow was skipped by `claude-code-action`'s
workflow-validation check and merged with a green check and no
review attached.

**"Removed" is a statement about this template layer, not about
every repo's current state.** Dropping a `[[file]]` entry stops kata
from managing the rendered file — it does not delete it. A repo that
had these workflows before this change keeps `claude-review.yml` /
`claude.yml` (and the `CLAUDE_CODE_OAUTH_TOKEN` secret) under
`.github/workflows/` until someone deletes them by hand, and until
then they still fire on every human-authored PR. Check
`.github/workflows/` before treating a PR as unreviewed-except-magi:
if either file is still there, its comments are a real review, not
noise to ignore.

- **`magi review <branch>`** runs only the review + verification +
  gate half of magi's graph: nothing competes, no implementation, no
  judging, no vote. That is the mode for hand-written work.
  `magi run "<task>"` is the full competition, for work handed over
  whole. Both end at the same gate.
- What the loop actually does: each reviewer gets its **own detached
  worktree pinned at the commit under review** (no reviewer can
  perturb the tree, and the fixer never races one); `verify.e2e` runs
  in the branch's worktree and its output is fed to the fixer;
  finding ids (`R2-1-3`) are assigned by magi, not by the agent, so
  the fixer's adoption report can be matched against them; the loop
  is bounded by `review_rounds`; `verify.gate` must exit 0 before any
  merge is attempted.
- **`magi.toml` is repo-owned, not kata-managed.** Point
  `verify.gate` at the exact command CI runs, so a local pass means a
  green PR, and point `verify.e2e` at the invocation that actually
  covers the repo — feature flags included. A gate that differs from
  CI turns a clean magi run into a red PR, which is the one failure
  this arrangement cannot absorb.
- **If you did not run magi, the change was not reviewed, and nothing
  will tell you.** Do not open a PR for a hand-written change before
  `magi review` comes back clean; if you must, say so in the PR body
  and say why. What does *not* count as a substitute: a green CI run
  (it compiles and tests, it does not review), and CodeRabbit's
  silence.
- **CodeRabbit stays installed and is not part of the gate.** It does
  not auto-review repositories under 10 stars — the common case here —
  so treat it as absent unless it posts. When it does post, its
  findings are a real review: address them, reply **in the inline
  thread** with an `@coderabbitai` mention (the review-comment
  *replies* endpoint,
  `gh api repos/<owner>/<repo>/pulls/<N>/comments/<id>/replies -f body=…`),
  and reply even when declining — say why, because a silent skip
  reads as overlooked. A "review limit reached" quota notice carries
  no findings and counts as quiet; re-trigger with
  `@coderabbitai review` when the quota refills if you want a real
  pass.
- **Read the report, not the exit status.** A reviewer seat that
  times out is logged as `WARN agent timed out seat=review-2` and
  then summarised as "raised 0 finding(s)" — indistinguishable from a
  genuinely clean pass in both the summary and `magi stats`. Check
  for timeouts before believing a clean round: a round where half the
  panel never answered is not a clean round.
- **Review artifacts stay local.** magi comments on a pull request
  only when it *stops* landing one. Findings, the fixer's adoption
  report and reviewer precision live in the run directory
  (`magi show`, `magi stats`). When the PR needs a record — a
  non-obvious fix, a finding declined with an argument — paste that
  part into the PR body or a comment yourself.
- With `merge = "pr"`, magi opens the pull request and keeps going:
  watches the checks, reads the review comments (human and bot), runs
  a bounded fix round when either is unhappy, pushes, and asks before
  merging. `land_approval` is on by default and **silence is a
  hold** — nothing merges unanswered. `magi answer` (or the web UI)
  is where it asks. Out of rounds leaves the PR open with a comment
  saying what still fails; `checks: unknown` never merges.
- **Merge gate**: magi's gate green — or CI green for a change magi
  never touched — **and** every review that did post resolved (a
  leftover `claude-review.yml`, CodeRabbit, a human) **and** the
  owner's explicit approval. The irreversible step stays a human
  decision.
- **No review-monitoring poll loop for bots this layer no longer
  ships.** The old loop existed to wait on them. Where a repo still
  has `claude-review.yml` (see above) the old cadence still applies
  until it is deleted; otherwise, after opening a PR wait for CI and
  report the wait state to the owner. When magi is landing the PR
  (`land = true`), magi does the watching.
- Bot-authored PRs (Renovate / Dependabot) need no review pass at
  all: CI green + owner approval.
- **Version-bump-only PRs** — a single `chore/release-vX.Y.Z` branch
  whose entire diff is `[workspace.package].version` /
  `[package].version` plus the matching inter-crate refs and the
  lockfile — likewise. There is nothing in a version bump for a
  reviewer to find, and the release pipeline downstream of merge
  (auto-tag → `release.yml`) is time-sensitive.

### Worktree workflow

> **Before your FIRST edit to any file, run `renri add` — NEVER edit the
> main checkout.** Read-only inspection (Read / Grep / Glob) stays on the
> main checkout; the instant you intend to *change* a file, you must
> already be in a worktree. The trap that keeps catching agents: diving
> into a fix the moment the diagnosis lands and editing in place. A
> concurrent agent shares the main checkout — your in-place edits will
> clobber theirs or be clobbered, and in a jj-colocated repo a stray
> working-copy commit entangles unrelated WIP into your branch. If you
> slip and edit in the main checkout, capture the diff first (jj already
> snapshotted it into the working-copy commit, so `jj diff > patch`; for
> git, `git stash` or save a patch — if you got as far as committing on a
> branch, just push it). Then reset the main checkout to pristine main
> (`jj new main@origin`, or `git switch -`), `renri add` a worktree, and
> re-apply the captured diff there.

Use [`renri`](https://github.com/yukimemi/renri) for any
commit-bound change. From the main checkout:

```sh
renri add <branch-name> --from main@origin            # create a worktree (jj-first), off latest upstream main
renri --vcs git add <branch-name> --from origin/main  # force a git worktree, off latest upstream main
renri remove <branch-name> -y --non-interactive  # cleanup after merge (agent-safe; see note)
renri prune                        # GC stale worktrees
```

Read-only inspection can stay on the main checkout.

**Always pass `--from <upstream main>`** (`main@origin` for jj,
`origin/main` for git). Without it, `renri add` forks off the *cwd
worktree's current HEAD* — in a long-lived main checkout that often
lags upstream, so the PR later shows up CONFLICTING against a `main`
that had already moved (e.g. a refactor merged upstream before the
branch was cut), forcing a manual re-port of the whole change.
`renri add` does fetch first, but fetching only updates `main@origin`
— it never moves the checkout's HEAD, so an explicit `--from` is what
guarantees a fresh base.

**Agents / non-interactive shells:** `renri remove` prints a details
panel and waits for a confirmation prompt — without `-y` it **hangs**,
and `--non-interactive` *alone* errors asking for `-y`. Always pass
`-y`, and add `--non-interactive` so a mistyped/omitted name fails
instead of opening a fuzzy picker (the same picker-fallback applies to
`remove` / `cd` / `exec` with no name). Use `-f`/`--force` to remove a
worktree that still has uncommitted changes or conflicts. To sweep
every merged-PR worktree in one shot: `renri remove --merged -y`.

### kata-managed sections

Several files in this repo are managed by `kata apply` from the
[`yukimemi/pj-presets`](https://github.com/yukimemi/pj-presets)
templates — the bytes between `<!-- kata:*:begin -->` and
`<!-- kata:*:end -->` markers, plus the overwrite-always files
listed in `.kata/applied.toml`. **Editing those bytes locally
won't survive the next `kata apply`** — push the change to the
upstream template repo (`yukimemi/pj-base` / `yukimemi/pj-rust` /
…) instead.

The marker scopes are layered, one per applied layer:
`kata:agents:base:*` is this section, and each layer adds its own
(`kata:agents:rust:*`, `kata:agents:rust-cli:*`,
`kata:agents:pnpm:*`, `kata:agents:firebase:*`, …). Which ones apply
*here* is a grep away: `<!-- kata:` in this file.

### This project's own conventions

Everything a layer ships is generic by construction: it describes the
stack the template assumed, not what this repo grew into. **Bytes
outside every marker pair are yours and survive `kata apply`** — so
project-specific conventions belong in a section of their own, outside
the markers (conventionally at the end of the file; if a later layer
appends its block below yours, no matter — kata only ever rewrites
between its own markers). Same mechanism as the `.gitignore` /
`.gitattributes` blocks.

Write those conventions down there rather than leaving them in one
agent's head, in commit archaeology, or in a README the agent will not
read. What earns a line:

- **Any layer default that does not hold here.** A layer states its
  assumption flatly ("Hosting is the primary target", "these rules are
  a placeholder to replace"). When the project has diverged, say so and
  say why — the layer's text keeps asserting the opposite on every
  apply, and an agent that only reads the blocks will act on it.
- **Facts duplicated across files with no compiler in between** — an
  address or a path that appears in code *and* in a rules/config file
  that cannot import it, a timeout that has to stay inside another
  timeout. List every copy, so the next edit finds them all.
- **kata-shipped files this project deleted on purpose**, together with
  the `once_applied = true` line in `.kata/applied.toml` that keeps
  them deleted. Otherwise someone helpfully restores one.
- **Shapes the runtime forces but no tool checks** — an export form a
  platform requires, import specifiers that must (or must not) carry a
  file extension, a directory whose contents are reachable by URL.
- **Invariants that money or access rest on**, naming the file and line
  that actually enforces them.
- **Which language the code speaks versus what a user reads**, when the
  two differ.

A repo whose `AGENTS.md` is nothing but kata blocks is a repo where
every agent re-derives all of that from scratch — and gets the layer
defaults wrong the same way each time.
<!-- kata:agents:base:end -->
<!-- kata:agents:rust:begin -->
### Rust workflow

This repo follows the shared Rust toolchain conventions. The
language-agnostic conventions block above (`kata:agents:base:*`)
covers git workflow, PR review cycle, and worktree usage.

### Build / lint / test

```sh
cargo make check                    # editorconfig-check + fmt --check + clippy + test + lock-check (the pre-push gate)
cargo make setup                    # one-time hook install + apm install
cargo build                         # debug build
cargo build --release               # release build
cargo test                          # tests; add -- --nocapture for stdout
```

`cargo make check` is what `.github/workflows/ci.yml` runs and what
the local pre-push hook calls — anything that passes locally
should pass on CI and vice versa. Don't paper over a failing
clippy by sprinkling `#[allow(clippy::...)]`; fix the underlying
issue or push back on the lint with reasoning.

### Toolchain pin

The Rust toolchain is pinned via `rust-toolchain.toml` and the
project compiles with the `stable` channel. Don't introduce
nightly-only features without a real reason; if you do, document
the reason in the relevant module.

### Lint / format policy

`rustfmt.toml` and `clippy.toml` are kata-managed (sourced from
`yukimemi/pj-rust`). Edits to those files in this repo won't
survive the next `kata apply`; if a setting is wrong, push the
fix to `yukimemi/pj-rust` so every Rust project using these templates picks
it up.

### CI workflow

`.github/workflows/ci.yml` is also kata-managed. The source lives
in `yukimemi/pj-rust/.github/workflows/ci.yml.template` (the
`.template` suffix keeps GitHub Actions from running the source
itself in pj-rust); each Rust project receives the rendered
`ci.yml` via `kata apply`. Action versions are bumped centrally
by Renovate at `yukimemi/pj-rust` and propagate down on the next
apply, so don't bump them locally — Renovate is configured
(via the kata-distributed `renovate.json`) to ignore
`.github/workflows/ci.yml` and `.github/workflows/release.yml`
in each PJ to avoid the bump→clobber loop.

### Releasing: version bump PR + auto-tag

Releases are triggered from `main` by a Cargo.toml version
change. `.github/workflows/auto-tag.yml` is kata-managed (source:
`yukimemi/pj-rust/.github/workflows/auto-tag.yml.tera`). It
watches `main` and, whenever a commit lands that changes the
top-level `version = "..."` in `Cargo.toml`, it pushes a matching
`vX.Y.Z` tag — no manual `git tag` step is needed. The tag push
then fires `release.yml`; see `kata:agents:rust-lib:*` or
`kata:agents:rust-cli:*` for what release.yml does in each
crate shape.

Cut a release via a small PR — never `git push` the bump
straight to `main`, even though the base block lists version
bumps as an exception to "no direct push". `auto-tag.yml` only
fires on `main`-branch pushes, so the bump must land via a merge
either way; using a PR also gives CI a chance to gate the
release. Enable automerge so CI green = release start:

```sh
git switch -c chore/release-vX.Y.Z
# Edit `package.version` in Cargo.toml, then:
cargo build                     # let Cargo.lock follow
git commit -am "chore: release vX.Y.Z"
git push -u origin chore/release-vX.Y.Z
gh pr create --fill
gh pr merge --auto --squash --delete-branch
```

Once CI is green the PR auto-merges. `auto-tag.yml` then pushes
`vX.Y.Z`, which fires `release.yml`.

**In a workspace, the version is in more than one place.** A member
that is published and depended on by another member is declared
with both a `path` and a `version` — crates.io needs a
requirement it can resolve for somebody who is not building from
the checkout, so a bare `path` will not do:

```toml
my-core = { path = "crates/my-core", version = "0.4.2" }
```

That literal does not follow `[workspace.package] version`.
Nothing in Cargo makes it, and the release above will not either.

**It fails late and quietly.** `version = "0.4.2"` means `^0.4.2`,
so a stale pin keeps resolving through every *patch* release and
stops only at the first bump that crosses the minor — where
`cargo build` refuses with `candidate versions found which didn't
match`, in the middle of cutting the release. Two repos on these
templates hit exactly this, one of them three releases after its
pins were last correct, and the other had already written the
hazard down in prose and drifted anyway.

So bump the pins in the same commit, keep them in
`[workspace.dependencies]` rather than in each member, and assert
it rather than remembering it. A test is the cheapest place —
`cargo test` already runs in CI, and it needs no toolchain a Rust
workspace does not have. [pj-rust-workspace's
README](https://github.com/yukimemi/pj-rust-workspace#the-internal-version-pin-and-the-check-for-it)
carries one to copy into any member's
`tests/check_versions.rs`: `internal_pins_match_the_workspace_version`
fails when a pin and the workspace version disagree, and
`members_inherit_the_workspace_version` fails when a member writes
its own version or reaches for a sibling by path.

**Repo settings to set once:** enable
`delete_branch_on_merge=true` (Settings → General →
"Automatically delete head branches"). The `--delete-branch`
flag on `gh pr merge --auto` is effectively a no-op — gh
returns as soon as automerge is enabled, so the deletion has to
happen server-side, which requires the repo setting.

**Why `KATA_APPLY_TOKEN`:** GitHub refuses to fire downstream
workflows from tags pushed by the default `GITHUB_TOKEN`, so
`auto-tag.yml` pushes with `KATA_APPLY_TOKEN` (the same PAT
`kata-apply.yml` already uses). Each consumer repo needs a
`KATA_APPLY_TOKEN` secret set; if a version-bump merge silently
doesn't fire `release.yml`, the missing PAT is the first thing
to check.
<!-- kata:agents:rust:end -->
<!-- kata:agents:rust-cli:begin -->
### Rust CLI release flow

This is a Rust CLI crate, so the release pipeline is publish-aware.
`yukimemi/pj-rust-cli` ships a tag-driven release workflow in
`.github/workflows/release.yml` (rendered from
`release.yml.template` for the same don't-auto-execute reason
ci.yml uses).

```sh
# Bump `package.version` in Cargo.toml (run `cargo build` so
# Cargo.lock follows), then:
git commit -am "chore: bump version to X.Y.Z"
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin main vX.Y.Z
```

The workflow then:
1. Cross-compiles binaries for x86_64 Linux / Windows / macOS,
   plus aarch64 macOS (Apple Silicon) — full triples
   `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
   `x86_64-apple-darwin`, `aarch64-apple-darwin`.
2. Uploads them as a GitHub Release with auto-generated notes.
3. `cargo publish --locked` to crates.io using the
   `CARGO_REGISTRY_TOKEN` repo secret.

Set the `CARGO_REGISTRY_TOKEN` secret once per repo (`gh secret
set CARGO_REGISTRY_TOKEN`) before the first tag push. If the
crate is internal-only and shouldn't go to crates.io, either drop
the `publish` job locally (release.yml is `when = "once"` so the
edit survives subsequent applies) or set `package.publish = false`
in `Cargo.toml`.

The binary name is derived from the GitHub repo name at runtime
(`${{ github.event.repository.name }}`), so the workflow is
identical across yukimemi/* CLIs unless your `[[bin]] name` in
`Cargo.toml` deliberately differs from the repo name — in that
case override `BIN_NAME` in the workflow's `env:` block.
<!-- kata:agents:rust-cli:end -->

<!-- repo-specific guidance below this line is NOT kata-managed; edit freely -->
## Release bump checklist (repo-specific)

The generic flow above bumps `[workspace.package].version` +
`Cargo.lock` — **this repo has one more synced file**:

- `crates/kanade-client/tauri.conf.json` — its `version` field is
  rewritten from `CARGO_PKG_VERSION` by `kanade-client/build.rs`
  (#260), but **only when a build actually runs on Windows**.
  `cargo update --workspace` alone does NOT touch it, which is how
  v0.43.28 shipped with the file still at 0.43.27.

So a release PR should contain exactly three files:

```sh
# in the release worktree, after editing Cargo.toml:
cargo update --workspace        # Cargo.lock follows
cargo build -p kanade-client    # build.rs syncs tauri.conf.json (Windows host only)
git add Cargo.toml Cargo.lock crates/kanade-client/tauri.conf.json
```

The build.rs sync is `#[cfg(target_os = "windows")]`-gated — on a
macOS/Linux host `cargo build -p kanade-client` compiles the
exit-fast shim and does NOT touch the file; edit the `version`
field in `tauri.conf.json` by hand there instead.

**Every bump needs a FOURTH edit** — the internal `kanade-shared`
version pin. `Cargo.toml`'s `[workspace.dependencies]` declares
`kanade-shared = { path = ..., version = "X.Y.Z" }`, and it sits a few
lines below `[workspace.package].version` in the same file, so the
release PR stays small.

This used to say *minor / major bumps*, and that was true of what cargo
would tolerate rather than of what you want. The requirement is a caret
(`^0.45.0` = `>=0.45.0, <0.46.0`), so a patch bump stays inside the range
and a stale pin costs nothing — until the release that crosses the minor,
where `cargo update --workspace` fails with `failed to select a version
for the requirement kanade-shared = "^0.45.0"`. Nothing is red in
between, which is why this paragraph existed and why the pin was still at
0.45.0 with the workspace at 0.45.4 when someone came to check.

The sibling repo `yaiba` — same kata preset, same shape — met the other
end of that on its v0.17.0: `cargo build` refused to resolve, three
releases after its pins were last correct, in the middle of cutting a
release. Deferring the edit only moves it to the worst moment to find it.

So the pin now tracks the version on every bump, and
`crates/kanade-shared/tests/check_versions.rs` asserts it rather than
leaving it to this paragraph — the same test yaiba carries. It costs one
more line in a patch release and removes the release-day surprise. It
also refuses a member that writes its own version or reaches for a
sibling by `path = "../"`, both of which are how the pin ends up
somewhere other than the one place it belongs.

If a previous release missed the sync (file lags by one version),
the catch-up diff will appear as churn in unrelated worktrees after
any build — `jj restore` it there and fold it into the **next**
release PR instead.

## Deploying a built release to a host

The backend's SQLite shutdown budget in
`crates/kanade-backend/src/shutdown.rs` (`CLOSE_TIMEOUT`, 25 seconds)
must stay below the 30-second `WaitForStatus('Stopped', ...)` in
`scripts/deploy/backend.ps1`. `crates/kanade-backend/src/service.rs`
adds 3 seconds to that constant for the SCM StopPending wait hint. WAL retention
is configured on every writer connection in `shutdown::sqlite_options`;
it limits retained space after reuse, not active transactions or readers.

Releases (above) ship binaries to GitHub Releases + crates.io. Getting a
release onto an actual machine (e.g. a co-located host running backend +
agent + nats) is a separate, agent-driven step:

1. **Stage** the binary locally — `scripts/build-release.ps1 -Roles
   backend -Version X.Y.Z` downloads the release `.zip` (SPA embedded)
   and extracts it into `dist/backend/`. First-time / staging only.
2. **Publish + roll out** — `scripts/fleet-deploy.ps1 -Role
   backend|agent|client|cli` does the whole agent-route in one command
   (app publish → deploy-script knob injection → script/manifest publish
   → job create → `kanade exec --pcs <pc>` → verify; agent uses `agent
   publish` + `agent rollout`). `-DryRun` prints every command without
   running it. See `configs/jobs/installers/README.md` for the full
   breakdown and the manual fallback.

   `-Role cli` installs the admin CLI itself (`install-kanade-cli`) —
   target operator hosts with `-Pc`, not `-All`. It's the one component
   with no self-update path, so without this an operator host silently
   drifts behind the backend until `job validate` and `job create`
   disagree about the manifest schema.

Gotchas (each has cost a session): the exec target is `--pcs <id>` /
`--groups <g>` **not** `--target pcs=`, and pc_ids must be passed
**VERBATIM** — an agent registers its pc_id as its OS hostname
(`$env:COMPUTERNAME`) as-is, casing is **not** uniform across the fleet
(some boxes upper-, some lower-case), and NATS subjects are
case-sensitive, so target the exact registered casing (check the SPA
Inventory / `kanade ping`); do **not** case-fold it. Dev
tokens are the literal `dev`. A squashed-migration upgrade needs
`-WipeDb`; a plain upgrade does not (no
new files under `crates/kanade-backend/migrations/`).

## Licensing and the dependency audit

This project ships under MIT (`LICENSE`, `[workspace.package].license`).
That claim is a statement about ~670 Rust crates and ~130 npm packages,
not about our own source, so it is enforced rather than asserted:

- `deny.toml` — the allow list for Rust, checked by `cargo deny check
  licenses` with `all-features = true` across **every** target. The
  desktop client's MPL-2.0 edges (`cssparser` / `selectors` /
  `dtoa-short`, via Tauri/wry) are target-gated, so a host-only scan on a
  Linux runner reports clean while missing them.
- `scripts/licenses/spdx.mjs` — the npm-side allow list and a real
  recursive-descent parser for SPDX expressions (`AND` binds tighter than
  `OR`; parentheses mean what they say). `scripts/licenses/spdx.test.mjs`
  pins it with `node --test`, including regression cases for shapes no
  current dependency has — that is the point, since nothing else would
  catch a break in them. Invoke it as
  `node --test "scripts/licenses/**/*.test.mjs"`: a *directory* argument
  is resolved as a module path and dies with `MODULE_NOT_FOUND`, which
  looks exactly like a failing test.
- `scripts/licenses/npm-licenses.mjs` — applies that policy to both
  `crates/*/web` projects. It resolves the **production** closure out of
  `bun.lock` rather than listing `node_modules`, because the dev tree
  carries `lightningcss` and its 12 per-platform binaries (all MPL-2.0)
  that vite never bundles into a shipped artifact.
- `THIRD-PARTY-NOTICES.md` — generated, never hand-edited. MIT, BSD-*,
  Apache-2.0, ISC and Unicode-3.0 each require the copyright notice to
  travel with the *binary*, and the backend binary carries the SPA inside
  it via rust-embed.
- `scripts/licenses/vendored/` — licence texts for shipped npm packages
  that publish none in their own tarball (`@vscode/l10n`,
  `react-remove-scroll-bar`, `victory-vendor` today), with
  `manifest.json` recording where each came from and which part of the
  package's SPDX expression it covers. A shipped package with no notice
  and no entry here **fails** the audit: a repository link is not the
  notice those licences ask for, and treating it as one was the original
  mistake. Adding an entry is a reviewed act — fetch the text from the
  canonical source, check it names a copyright holder, record the URL.
  `covers` exists so a compound expression cannot be half-attributed:
  `victory-vendor` is `MIT AND ISC` and needs both Victory's MIT text and
  the d3 ISC text it vendors.

`cargo make licenses` runs all four; `.github/workflows/licenses.yml`
runs the same four on every PR. Regenerate the notices with `cargo make
notices` whenever a lockfile changes — the workflow fails the PR
otherwise.

Four things here are duplicated with no compiler in between:

- **The allow list exists twice** — `licenses.allow` in `deny.toml` and
  `ALLOWED` in `scripts/licenses/spdx.mjs`. One policy, two package
  managers, nothing tying them together. Change both. (`spdx.test.mjs`
  asserts the JS half carries no GPL/AGPL/LGPL identifier, which catches
  the worst way to get this wrong but not a drift between the two lists.)
- **The ship-target list exists twice** — the `matrix.include` targets in
  `.github/workflows/release.yml` and `targets` in `about.toml`. A target
  added to the release matrix but not to `about.toml` silently drops that
  platform's crates from the notices.
- **The bundle scripts copy licences in four places** —
  `deploy/linux/bundle.sh`, `bundle.ps1`, `bundle-agent.sh`,
  `bundle-agent.ps1`. The two non-agent ones additionally extract the
  Apache-2.0 texts out of the nats-server and caddy release tarballs,
  because those bundles redistribute unmodified third-party binaries.
- **The two npm project paths exist twice** — `PROJECTS` in
  `npm-licenses.mjs` and the `web-install` / `web-install-client` tasks in
  `Makefile.toml`.
- **`bun install` flags exist twice** — the `web-install*` tasks in
  `Makefile.toml` and the install steps in `licenses.yml`. Both use
  `--frozen-lockfile --ignore-scripts`; the audit job installs dependency
  content precisely in order to read it, so running that content's
  postinstall hooks would let a compromised package rewrite the licence
  files being audited.

Two traps worth knowing before you touch this:

- **`cargo install cargo-about` installs nothing.** Its binary is behind a
  non-default `cli` feature, so a plain install compiles for minutes, emits
  a *warning*, and exits 0 with no binary. Use `cargo install cargo-about
  --locked --features cli`, or let CI's `taiki-e/install-action` fetch the
  prebuilt one.
- **Upstream licence texts contain CRLF.** `pelite` and `equivalent` are two
  of them, and cargo-about reproduces their bytes exactly. With
  `* text=auto eol=lf` in `.gitattributes`, git rewrites those to LF on
  commit — so the generator's output could never equal the committed file and
  `notices --check` would fail on every fresh checkout, forever.
  `notices.mjs` normalises line endings before writing *or* comparing.
  Don't remove that, and don't "fix" it by exempting the file from
  normalisation: the point is that the bytes are identical on every platform.
- **The licence-file pattern must allow `_` and `-`, not just `.`.** The
  first version accepted `LICENSE` and `LICENSE.md` but not `LICENSE_MIT`
  or `LICENSE-MPL`, so `@tauri-apps/api` and `dompurify` were reported as
  publishing no licence while their texts sat in `node_modules`
  unreproduced. A false negative there is an attribution failure that
  looks exactly like an upstream packaging gap.
- **SPDX expressions need a parser, not a regex.** The first version of
  `evaluate()` treated any `AND` in the string as the top-level operator,
  which ignores both parentheses and SPDX precedence. It turned
  `(GPL-3.0 AND BSD-3-Clause) OR MIT` into `GPL-3.0 AND MIT` — rejecting a
  package that offers plain MIT — and `MIT OR GPL-3.0 AND ISC` into
  `MIT AND ISC`, a combination never on offer. Neither shape is in the tree
  today; both are pinned in `spdx.test.mjs`.
- **`{{!` handlebars comments end at the first `}}`.** `about.hbs`
  documents its own syntax, so it uses the `{{!-- --}}` block form; the
  short form spilled its tail into the generated notices. It also uses
  triple-stache everywhere — the double form HTML-escapes, which would
  publish an altered copy of the very licence texts we are obliged to
  reproduce verbatim.

Policy, for when the gate goes red: **MPL-2.0 is allowed, GPL / AGPL /
LGPL-only are not.** MPL-2.0 is copyleft per *file* and its section 3.3
explicitly permits distributing a Larger Work under other terms, so it
does not reach our source — but only while every MPL crate is consumed
unmodified from crates.io. A `[patch]` entry or a vendored fork of one
would oblige us to publish those modified files under the MPL-2.0.
Widening either allow list is a licensing decision, not a build fix.


## Detached launchers and local cadence

- `process::OUTPUT_DRAIN_GRACE` bounds output capture after the script host
  exits, including normal exit. Keep the async and native Windows readers
  cancellable and preserve partial output; aborting a blocking Rust task does
  not interrupt `ReadFile`. A clean launcher exit must not kill its daemon.
- Native user/session launches use `InheritedHandles` in `process_as_user.rs`.
  Keep an explicit per-launch handle list: blanket inheritance can pass another
  concurrently starting job's pipe into a long-lived child (#1452), which
  clearing the launcher's own stdout/stderr inheritance cannot fix.
- `commands::handle_command` records every successful run through
  `local_scheduler::record_job_success`. The empty schedule-id key in
  `local_completions.json` stores job-wide success across manual/local triggers;
  use the newest job-wide or schedule-specific timestamp, never move it back,
  and never clear a different execution's live claim on manual completion.
- Schedule status/coverage cadence is an observation of per-PC job starts,
  separate from historical rollout success. `OVERDUE` means no observed start
  for three intervals; offline hosts, windows and freezes can also explain it.
