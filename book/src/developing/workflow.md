# Developer Workflow and Contribution

This document outlines the standard workflows, lint/test requirements, and VCS branching guidelines for contributors working on the `kanade` codebase.

---

## 1. Quality Gates (Pre-Push & CI)

The local test suite must be green before you push or submit PRs. This matches the automated checks running on GitHub Actions.

```bash
# Run formatting check, clippy checks, target tests, and cargo lock checks
cargo make check
```

- **FMT & Clippy**: We maintain a strict zero-warning policy. Do not sprinkle `#[allow(clippy::...)]` unless there is a strong architectural justification.
- **TDD (Test-Driven Development)**: Follow Kent Beck's TDD methodology. Write failing tests first to define the *what*, then implement the code to satisfy them.

---

## 2. Worktree Management with `renri`

For isolated feature development, we use [`renri`](https://github.com/yukimemi/renri) to manage lightweight repository worktrees. This prevents staging pollution, keeps your main checkout clean, and allows you to switch tasks instantly.

### Why `renri`?
In co-located Git and Jujutsu (jj) environments, managing worktrees manually can get complex. `renri` simplifies this by automatically wrapping VCS-specific worktree creation (favoring `jj` when configured) and cleanups.

### Common Commands

```bash
# Create an isolated worktree (uses Jujutsu by default if present)
renri add feat/your-awesome-feature

# Force a Git-native worktree (bypassing jj)
renri --vcs git add feat/your-awesome-feature

# Clean up and delete a worktree after merging
renri remove feat/your-awesome-feature

# Garbage-collect and prune stale or broken worktrees
renri prune
```

*Note: Worktree creation automatically invokes the cargo-make `on-add` hook to fetch remote refs and bootstrap APM configurations immediately.*

---

## 3. Co-located Jujutsu (jj) & Git Workflow

Our development environment is configured with co-located Git and Jujutsu. We prefer `jj` for local version control due to its safe, conflict-free commit model.

### Guidelines
- **No Direct Push to `main`**: All changes must land via a Pull Request.
- **Branch/Bookmark Naming**:
  - `feat/...` for new features.
  - `fix/...` for bugs.
  - `chore/...` for infrastructure, dependency bumps, or releases.
- **Commit Messages**: Write commit messages, PR titles, and bodies in **English**.
- **Version Bumps**: Release version bumps are managed exclusively via PRs on `main` and automated tagging pipelines. Never run `git tag` manually.

---

## 4. Documentation Policy

Documentation must stay in lock-step with code changes. 
Whenever you add or modify features:
- Update docstrings and comments explaining the *why* (avoid comments restating *how*).
- Update the relevant book pages (written in English under `book/src/`).
- Synchronize localization catalogs by running the translation template generator.
