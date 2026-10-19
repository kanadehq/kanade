# Pending upstream signing hooks

The generated release workflow alone does not preserve signing integration.
These patches add the optional consumer-owned `release-post-build.sh` hooks
before packaging and before upload in both upstream release templates. The
verification hook receives no signing secrets. Consumers without the script
remain unaffected; consumers with it enforce their own signing policy.

Both files target `.github/workflows/release.yml.template` (not `.tera`).
They were prepared against the following upstream main file blobs:

| Repository | Source blob SHA | Patch |
| --- | --- | --- |
| yukimemi/pj-rust-workspace | 71f0890424dbb9e20db8e949e0d85552f91a8e2d | pj-rust-workspace.patch |
| yukimemi/pj-rust-cli | 967c96a41b8687f50fa99e390cfcfa3a1b39fbe1 | pj-rust-cli.patch |

Status: prepared only, not applied upstream. No upstream PR or commit is
claimed. Both patches were checked with `git apply --check` against the
upstream blobs listed above (still the current main blobs when checked), so they
apply cleanly. Opening the upstream PRs is a separate step in those
repositories.

Apply each patch in the corresponding upstream repository with `git apply
--check` first, review it under that repository's conventions, and open a PR.
Do not merge without owner approval. After the workspace template change lands,
run `kata apply` in the consumer and commit the resulting source revision and
workflow updates. Verify the generated workflow retains both hook invocations
in sign → package → verify → upload order. Keep release readiness blocked
until that integration and the production certificate match are confirmed.

Note: the templates keep the hook optional for consumers without a signing
pin, but add a fail-closed guard step: when `deploy/macos/signing-cert.sha1`
exists and `scripts/ci/release-post-build.sh` does not, the release fails
instead of publishing linker ad-hoc signatures. The generated workflow in this
repository is exactly what the patched templates render, and
`check_release_signing.rs` asserts the guard, so a `kata apply` that drops the
integration fails CI.
