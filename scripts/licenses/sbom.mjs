#!/usr/bin/env node
// sbom.mjs — one CycloneDX document per shipped binary.
//
//   node scripts/licenses/sbom.mjs [--out DIR]
//
// `cargo make sbom` wraps it. Default output: target/sbom/<binary>.cdx.json
//
// Why per binary rather than one for the workspace: the four binaries do not
// contain the same code. `kanade-agent` has no SPA in it; `kanade-backend`
// embeds one via rust-embed; `kanade-client` bundles a different one through
// Tauri. A single workspace BOM would claim every component is in every
// artifact, which is the kind of "technically a superset" answer that makes an
// SBOM useless for the question people actually ask it — *is this binary
// affected?*
//
// Why not committed, unlike THIRD-PARTY-NOTICES.md: an SBOM describes a
// release artifact, not a commit. It is generated at tag time and uploaded as
// a release asset (.github/workflows/release-extras.yml). CI still runs this
// script on every PR so a break is caught by the PR that causes it rather than
// at the next release.
//
// Prerequisites:
//   cargo install cargo-cyclonedx --locked
//   bun install --frozen-lockfile   in each crates/*/web

import { execFileSync } from 'node:child_process'
import { readFileSync, writeFileSync, mkdirSync, existsSync, rmSync } from 'node:fs'
import { join, dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..')

// Which web project ends up inside which binary. This is the same mapping
// `PROJECTS` in npm-licenses.mjs encodes, seen from the other side — keep the
// two in step (see the licensing section of AGENTS.md).
const EMBEDS = {
  'kanade-backend': 'crates/kanade-backend/web',
  'kanade-client': 'crates/kanade-client/web',
}

function run(cmd, args, what) {
  try {
    return execFileSync(cmd, args, { cwd: REPO_ROOT, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024, stdio: ['ignore', 'pipe', 'inherit'] })
  } catch (err) {
    if (err.code === 'ENOENT') throw new Error(`${cmd} is not installed — ${what}`)
    throw new Error(`\`${cmd} ${args.join(' ')}\` failed with status ${err.status} — if the command itself is missing: ${what}`)
  }
}

/**
 * Every workspace member, with the bin names it builds (empty for a lib).
 *
 * Read from cargo rather than hardcoded, for the same reason notices.mjs reads
 * the member list: a fifth binary must not be able to appear without an SBOM.
 * Lib-only members are returned too, because cargo-cyclonedx writes a file
 * beside their manifests that has to be cleaned up.
 *
 * @returns {Array<{package: string, manifestDir: string, bins: string[]}>}
 */
function workspaceMembers() {
  const meta = JSON.parse(run('cargo', ['metadata', '--no-deps', '--format-version', '1'], 'install Rust'))
  return meta.packages.map((p) => ({
    package: p.name,
    manifestDir: dirname(p.manifest_path),
    bins: p.targets.filter((t) => t.kind.includes('bin')).map((t) => t.name),
  }))
}

/**
 * Run cargo-cyclonedx over the workspace and read back one BOM per member.
 *
 * `--all-features` for the same reason deny.toml sets it: a default-features
 * scan describes a narrower tree than the release build links.
 *
 * @returns {Map<string, object>} package name -> parsed CycloneDX document
 */
function rustBoms(members) {
  run('cargo', ['cyclonedx', '--all-features', '--format', 'json', '--spec-version', '1.5'],
    'run: cargo install cargo-cyclonedx --locked')

  // cargo-cyclonedx writes `<name>.cdx.json` next to EVERY member manifest,
  // including lib-only ones we ship no BOM for. Clean up all of them, in a
  // finally so a failure partway through does not leave the source tree
  // littered with untracked files that look committable.
  const boms = new Map()
  try {
    for (const member of members) {
      const path = join(member.manifestDir, `${member.package}.cdx.json`)
      if (member.bins.length === 0) continue
      if (!existsSync(path)) {
        throw new Error(`cargo-cyclonedx produced no BOM for ${member.package} at ${path} — its output layout may have changed between versions`)
      }
      boms.set(member.package, normalizeBuildPaths(JSON.parse(readFileSync(path, 'utf8'))))
    }
  } finally {
    for (const member of members) {
      rmSync(join(member.manifestDir, `${member.package}.cdx.json`), { force: true })
    }
  }
  return boms
}

/**
 * Replace the absolute build path cargo-cyclonedx bakes into workspace-local
 * refs with a repo-relative one.
 *
 * It emits `path+file:///home/runner/work/kanade/kanade/crates/...` for
 * members built from a path, and the same string appears again in every
 * `dependencies[].ref` / `dependsOn` entry that points at one. Published as-is
 * that leaks the build machine's layout into a release asset and makes two
 * builds of the same tag produce different documents for no reason.
 *
 * Rewritten as a whole-document string substitution rather than field by
 * field, precisely because the ref is a cross-reference: changing the
 * definition and missing a `dependsOn` would produce a BOM whose dependency
 * graph points at nodes that no longer exist.
 *
 * @param {object} bom
 * @returns {object} the same document with `<repo root>` paths made relative
 */
function normalizeBuildPaths(bom) {
  const fileUrl = `file://${REPO_ROOT}`
  let text = JSON.stringify(bom)
  if (!text.includes(REPO_ROOT)) return bom
  // `path+file:///abs/crates/x` -> `path+file:crates/x`
  text = text.split(`path+${fileUrl}/`).join('path+file:')
  // `?download_url=file://.` is already relative but meaningless for a
  // published artifact; drop the qualifier rather than shipping a URL that
  // resolves to wherever the consumer happens to stand.
  text = text.split('?download_url=file://.').join('')
  // Anything else still carrying the absolute root (an externalReference,
  // say) becomes repo-relative too.
  text = text.split(`${fileUrl}/`).join('file:')
  return JSON.parse(text)
}

/** npm components per web project, from the resolver that owns "what ships". */
function npmComponents() {
  return JSON.parse(run('node', [join('scripts', 'licenses', 'npm-licenses.mjs'), '--sbom'], 'install Node.js'))
}

/**
 * Fold a web project's components into a binary's BOM.
 *
 * Existing `bom-ref`s are left alone: npm refs are PURLs in the npm namespace
 * and cargo's are in the cargo namespace, so they cannot collide.
 */
function merge(bom, npm, binaryName) {
  bom.components = [...(bom.components ?? []), ...npm.components]

  if (npm.unresolved.length) {
    // An SBOM that quietly omits a platform-gated component is wrong in the
    // direction that matters, so say so in the document itself rather than
    // only in a log line the consumer never sees.
    bom.metadata ??= {}
    bom.metadata.properties = [
      ...(bom.metadata.properties ?? []),
      ...npm.unresolved.map((u) => ({
        name: 'kanade:unresolved-component',
        value: `${u.name}@${u.version} (platform-restricted, not installed on the machine that generated this SBOM)`,
      })),
    ]
  }
  return bom
}

const outFlag = process.argv.indexOf('--out')
const outDir = outFlag === -1 ? join(REPO_ROOT, 'target', 'sbom') : resolve(process.argv[outFlag + 1])

try {
  const allMembers = workspaceMembers()
  const boms = rustBoms(allMembers)
  const members = allMembers.filter((m) => m.bins.length > 0)
  const npm = npmComponents()

  // Every mapping must correspond to a real binary. A rename that silently
  // stopped folding the SPA into the backend's BOM would produce a document
  // that looks complete and is not.
  for (const pkg of Object.keys(EMBEDS)) {
    if (!members.some((m) => m.package === pkg)) {
      throw new Error(`EMBEDS names "${pkg}", which is not a binary-producing workspace member — update the mapping in scripts/licenses/sbom.mjs`)
    }
  }

  mkdirSync(outDir, { recursive: true })
  const written = []
  for (const member of members) {
    const bom = boms.get(member.package)
    const webDir = EMBEDS[member.package]
    if (webDir) {
      if (!npm[webDir]) throw new Error(`no npm components for "${webDir}" — PROJECTS in npm-licenses.mjs and EMBEDS in sbom.mjs disagree`)
      merge(bom, npm[webDir], member.package)
    }
    const path = join(outDir, `${member.package}.cdx.json`)
    writeFileSync(path, `${JSON.stringify(bom, null, 2)}\n`)
    written.push({ path, package: member.package, components: bom.components?.length ?? 0, web: webDir ?? null })
  }

  for (const w of written) {
    console.log(`sbom: ${w.package} — ${w.components} components${w.web ? ` (incl. ${w.web})` : ''} → ${w.path}`)
  }
} catch (err) {
  console.error(`sbom: ${err.message}`)
  process.exit(1)
}
