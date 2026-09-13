#!/usr/bin/env node
// npm-licenses.mjs — the npm half of the licence audit.
//
// Why this exists at all: the SPA in `crates/kanade-backend/web` is embedded
// into the backend binary with rust-embed, and the Tauri client bundles
// `crates/kanade-client/web`. Both ship inside artifacts this project
// distributes under MIT, so their dependencies need exactly the treatment
// `cargo-deny` + `cargo-about` give the Rust side. No npm equivalent reads a
// bun.lock, so this script is it.
//
//   node scripts/licenses/npm-licenses.mjs --check
//       Exit non-zero if any SHIPPED package carries a licence outside the
//       allow list in spdx.mjs. This is the CI gate.
//
//   node scripts/licenses/npm-licenses.mjs --notices
//       Emit the markdown notice section on stdout, for
//       `scripts/licenses/notices.mjs` to splice into THIRD-PARTY-NOTICES.md.
//
// Both modes require `bun install` to have run in each web project: licence
// *text* only exists on disk, never in the lockfile.
//
// The dev/prod split is the whole point of walking the lockfile rather than
// listing `node_modules`. `tailwindcss` pulls `lightningcss` plus its 12
// per-platform binary packages, all MPL-2.0, and `vite`/`typescript`/
// `playwright` bring more — none of which are ever bundled by vite into the
// artifact we distribute. Reporting them as shipped third-party code would
// claim we distribute code we do not; omitting a genuinely shipped MPL
// package would be the far worse error. So: resolve the production closure
// from the lockfile, and treat anything unresolvable as fatal instead of
// silently dropping it.

import { readFileSync, readdirSync, existsSync, statSync } from 'node:fs'
import { join, dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

// The allow list and the SPDX expression evaluator live in their own module so
// they can be unit-tested (`node --test scripts/licenses/`). See spdx.mjs for
// why the expression handling is a real parser rather than a regex.
import { evaluate, parse, unlistedLeaves } from './spdx.mjs'
// CycloneDX shaping lives in its own module so the identifier handling is
// reachable from a unit test — see cyclonedx.test.mjs.
import { npmComponent } from './cyclonedx.mjs'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..')

// The two npm projects whose output is distributed. Keep in step with the
// `web-install` / `web-build` tasks in Makefile.toml.
const PROJECTS = [
  { label: 'kanade-backend SPA (embedded in the backend binary via rust-embed)', dir: 'crates/kanade-backend/web' },
  { label: 'kanade-client web assets (bundled into the Tauri desktop client)', dir: 'crates/kanade-client/web' },
]

// ---------------------------------------------------------------------------
// bun.lock is JSONC: trailing commas, and (in principle) comments. Strip them
// with a scanner that respects string literals rather than a bare regex — a
// regex that rewrites `,\s*}` anywhere would happily corrupt a dependency
// range or a base64 integrity hash that contained the same bytes.
// ---------------------------------------------------------------------------
/**
 * Parse bun.lock, which is JSONC: trailing commas and (in principle) comments.
 *
 * @param {string} text raw file contents
 * @param {string} path only used to make the error message locatable
 * @returns {object}
 * @throws {Error} if the result is still not valid JSON after stripping.
 */
function parseJsonc(text, path) {
  let out = ''
  let i = 0
  while (i < text.length) {
    const c = text[i]
    if (c === '"') {
      // Copy the string literal verbatim, honouring backslash escapes.
      let j = i + 1
      while (j < text.length) {
        if (text[j] === '\\') { j += 2; continue }
        if (text[j] === '"') break
        j++
      }
      out += text.slice(i, j + 1)
      i = j + 1
      continue
    }
    if (c === '/' && text[i + 1] === '/') {
      while (i < text.length && text[i] !== '\n') i++
      continue
    }
    if (c === '/' && text[i + 1] === '*') {
      i += 2
      while (i < text.length && !(text[i] === '*' && text[i + 1] === '/')) i++
      i += 2
      continue
    }
    if (c === ',') {
      // Look ahead past whitespace: a comma before `}` or `]` is trailing.
      let j = i + 1
      while (j < text.length && /\s/.test(text[j])) j++
      if (text[j] === '}' || text[j] === ']') { i++; continue }
    }
    out += c
    i++
  }
  try {
    return JSON.parse(out)
  } catch (err) {
    throw new Error(`${path}: not parseable as JSONC after comma/comment stripping: ${err.message}`)
  }
}

// ---------------------------------------------------------------------------
// Production closure.
//
// bun.lock keys mirror the installed tree: a hoisted package is `"name"`, a
// nested one is `"parent/name"` (and `node_modules/parent/node_modules/name`
// on disk). Resolving a dependency therefore walks up from the deepest
// candidate to the root, exactly as node's own resolution does.
// ---------------------------------------------------------------------------
/**
 * Resolve a dependency name to its bun.lock key, as seen from `fromKey`.
 *
 * Walks up the nesting path the way node's own resolution does, so a package
 * with its own pinned copy of a dependency finds that copy first.
 *
 * @returns {string|null} the lockfile key, or null if nothing resolves.
 */
function resolveKey(packages, fromKey, depName) {
  const segments = fromKey === '' ? [] : fromKey.split('/')
  for (let depth = segments.length; depth >= 0; depth--) {
    const candidate = [...segments.slice(0, depth), depName].join('/')
    if (candidate in packages) return candidate
  }
  return null
}

/**
 * The set of lockfile keys reachable from the workspace's *runtime*
 * dependencies — i.e. what can end up in a distributed artifact.
 *
 * @returns {Set<string>} lockfile keys.
 * @throws {Error} on an unresolvable hard dependency, which means the lockfile
 *   is stale. Continuing would under-report what we ship, so it stops instead.
 */
function productionClosure(lock, path) {
  const packages = lock.packages ?? {}
  const workspaces = lock.workspaces ?? {}

  // Roots: `dependencies` of every workspace, deliberately NOT
  // `devDependencies`. `optionalDependencies` are included — an optional dep
  // that does get installed ships like any other.
  const queue = []
  const seen = new Set()
  for (const [wsName, ws] of Object.entries(workspaces)) {
    for (const depName of Object.keys({ ...ws.dependencies, ...ws.optionalDependencies })) {
      const key = resolveKey(packages, wsName === '' ? '' : wsName, depName)
      if (!key) {
        throw new Error(`${path}: root dependency "${depName}" has no entry in the lockfile — run \`bun install\` and commit the updated bun.lock`)
      }
      queue.push(key)
    }
  }

  while (queue.length) {
    const key = queue.pop()
    if (seen.has(key)) continue
    seen.add(key)

    const entry = packages[key]
    if (!entry) continue
    const meta = entry[2] ?? {}

    // `optionalPeers` names peers the package works without. Those are NOT
    // shipped edges: `typescript` is an optional peer of i18next and
    // react-i18next purely so the package can offer types, and it lands in
    // node_modules here only because it is a devDependency. Traversing it
    // would put the whole TypeScript compiler in the notices as shipped
    // code. A non-optional peer (react, react-dom) really is bundled, so
    // those stay.
    const optionalPeers = new Set(meta.optionalPeers ?? [])
    const peers = Object.fromEntries(
      Object.entries(meta.peerDependencies ?? {}).filter(([name]) => !optionalPeers.has(name)),
    )
    const deps = {
      ...meta.dependencies,
      ...meta.optionalDependencies,
      ...peers,
    }

    for (const depName of Object.keys(deps)) {
      const resolved = resolveKey(packages, key, depName)
      if (resolved) { queue.push(resolved); continue }
      // An unresolvable *optional* peer is normal (react-is for recharts,
      // say). An unresolvable hard dependency means the lockfile is stale,
      // and silently continuing would under-report what we ship.
      const isOptional = optionalPeers.has(depName) || depName in (meta.optionalDependencies ?? {})
      const isPeer = depName in (meta.peerDependencies ?? {})
      if (!isOptional && !isPeer) {
        throw new Error(`${path}: "${key}" depends on "${depName}", which has no entry in the lockfile — run \`bun install\` and commit the updated bun.lock`)
      }
    }
  }
  return seen
}

// ---------------------------------------------------------------------------
// On-disk metadata.
// ---------------------------------------------------------------------------
// `LICENSE`, `LICENSE.md`, and also `LICENSE_MIT` / `LICENSE-MPL`. The
// separator class is not cosmetic: the first version of this pattern allowed
// only a dot, so `@tauri-apps/api` (LICENSE_MIT + LICENSE_APACHE-2.0) and
// `dompurify` (LICENSE-MPL) were reported as publishing no licence at all
// while their texts sat on disk unreproduced. A false negative here is an
// attribution failure, not a cosmetic one.
const LICENSE_FILE = /^(LICEN[CS]E|COPYING|NOTICE)([._-].*)?$/i

// Licence texts vendored for packages that publish none. See the _comment in
// the manifest: an entry here is a reviewed decision, and its absence is what
// makes a missing notice fail the audit instead of warning.
const VENDORED_DIR = join(REPO_ROOT, 'scripts', 'licenses', 'vendored')
const VENDORED = JSON.parse(readFileSync(join(VENDORED_DIR, 'manifest.json'), 'utf8')).packages

/**
 * On-disk location of a lockfile key: `a/b` lives at
 * `node_modules/a/node_modules/b`.
 *
 * @returns {string} absolute path (the scope of a scoped name is not a nesting
 *   level, and is rejoined accordingly).
 */
function packageDir(projectDir, key) {
  // "a/b" -> node_modules/a/node_modules/b; "@scope/x" is a single package
  // name, not a nesting separator, so rebuild the segments scope-aware.
  const segments = []
  for (const part of key.split('/')) {
    if (segments.length && segments[segments.length - 1].startsWith('@') && !segments[segments.length - 1].includes('/')) {
      segments[segments.length - 1] += '/' + part
    } else {
      segments.push(part)
    }
  }
  return join(projectDir, 'node_modules', segments.join('/node_modules/'))
}

/**
 * Extract an SPDX expression from a package.json, covering the modern
 * `license` string, the object form, and the deprecated `licenses` array.
 *
 * @returns {string|null} null when the package declares no licence at all.
 */
function normalizeLicense(pkgJson) {
  if (typeof pkgJson.license === 'string') return pkgJson.license.trim()
  if (pkgJson.license && typeof pkgJson.license === 'object' && pkgJson.license.type) return String(pkgJson.license.type).trim()
  // Deprecated pre-npm-v5 form, still present in a few long-lived packages.
  if (Array.isArray(pkgJson.licenses)) {
    const types = pkgJson.licenses.map((l) => l.type).filter(Boolean)
    if (types.length) return types.length === 1 ? types[0] : `(${types.join(' OR ')})`
  }
  return null
}

/**
 * Read both web projects: resolve what ships, then load each package's licence
 * expression and notice text from disk (or from the vendored overrides).
 *
 * @returns {Array<{label: string, dir: string, packages: object[], unreadable: object[]}>}
 * @throws {Error} if a lockfile or a node_modules tree is missing.
 */
function collect() {
  const results = []
  for (const project of PROJECTS) {
    const projectDir = join(REPO_ROOT, project.dir)
    const lockPath = join(projectDir, 'bun.lock')
    if (!existsSync(lockPath)) throw new Error(`missing ${lockPath}`)
    const lock = parseJsonc(readFileSync(lockPath, 'utf8'), lockPath)
    const shipped = productionClosure(lock, lockPath)

    if (!existsSync(join(projectDir, 'node_modules'))) {
      throw new Error(`${project.dir}: node_modules is missing — run \`bun install --frozen-lockfile\` there first (licence text is only on disk, not in the lockfile)`)
    }

    const packages = []
    const unreadable = []
    for (const key of [...shipped].sort()) {
      const entry = lock.packages[key]
      const [nameAtVersion] = entry
      const at = nameAtVersion.lastIndexOf('@')
      const name = nameAtVersion.slice(0, at)
      const version = nameAtVersion.slice(at + 1)

      const dir = packageDir(projectDir, key)
      if (!existsSync(dir)) {
        // A package with `os`/`cpu` constraints is simply not installed on
        // this machine — bun filters it out. That is not a stale lockfile, so
        // failing here would turn a routine dependency bump into a CI failure
        // whose message points at the wrong problem. But it is not nothing
        // either: such a package DOES ship on its own platform, and its
        // licence cannot be read from here. Surface it by name instead of
        // dropping it, and let a human decide.
        const constraints = entry[2] ?? {}
        if (constraints.os || constraints.cpu) {
          unreadable.push({ name, version, constraints: { os: constraints.os, cpu: constraints.cpu } })
          continue
        }
        throw new Error(`${project.dir}: "${key}" is in the production closure but not installed at ${dir} — run \`bun install --frozen-lockfile\``)
      }
      const pkgJson = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'))
      const expression = normalizeLicense(pkgJson)

      let texts = []
      for (const file of readdirSync(dir)) {
        if (!LICENSE_FILE.test(file)) continue
        const full = join(dir, file)
        if (!statSync(full).isFile()) continue
        texts.push({ file, text: readFileSync(full, 'utf8').trim() })
      }
      texts.sort((a, b) => a.file.localeCompare(b.file))

      // Nothing in the tarball — fall back to a reviewed vendored text.
      let vendored = null
      if (texts.length === 0 && VENDORED[name]) {
        vendored = VENDORED[name]
        texts = vendored.files.map((f) => ({
          file: f.path,
          vendoredFrom: f.source,
          covers: f.covers,
          note: f.note,
          text: readFileSync(join(VENDORED_DIR, f.path), 'utf8').trim(),
        }))
      }

      // evaluate() throws on a malformed expression rather than guessing at
      // one. Capture it per package so one bad `license` field names itself
      // instead of aborting the whole run with a stack trace.
      let choice = null
      let malformed = null
      if (expression) {
        try {
          choice = evaluate(expression)
        } catch (err) {
          malformed = err.message
        }
      }

      // bun.lock's 4th element is the npm integrity string
      // (`sha512-<base64>`). Carried through for the SBOM, where a hash
      // is what lets a consumer tell whether the component they have is
      // the component this build resolved.
      const integrity = typeof entry[3] === 'string' ? entry[3] : null

      packages.push({
        name, version, expression, malformed, vendored, integrity,
        choice,
        repository: typeof pkgJson.repository === 'string' ? pkgJson.repository : pkgJson.repository?.url ?? null,
        texts,
      })
    }
    results.push({ ...project, packages, unreadable })
  }
  return results
}

// ---------------------------------------------------------------------------
/**
 * The CI gate. Prints warnings, then exits non-zero if any shipped package
 * carries a licence outside the allow list or ships without a reproducible
 * notice.
 *
 * @param {ReturnType<typeof collect>} projects
 */
function check(projects) {
  // Two severities, because they are two different problems.
  //
  // `problems` are licence *terms* this project cannot ship under — the thing
  // the gate exists to catch, and always fixable here (drop the dependency,
  // or make a deliberate decision to widen the allow list).
  //
  // `warnings` are packages that declare a licence in package.json but ship
  // no licence text in the published tarball. That is a defect in the
  // upstream package, not in this repo, and nothing in this tree can fix it —
  // failing CI on it would mean a red build that the only available "fix" is
  // to delete a working dependency. The notices file records these
  // explicitly (declared licence + upstream repository) rather than pretending
  // the text was reproduced.
  const problems = []
  const warnings = []
  for (const project of projects) {
    for (const pkg of project.packages) {
      const where = `${project.dir}: ${pkg.name}@${pkg.version}`
      if (!pkg.expression) {
        problems.push(`${where} has no \`license\` field — unlicensed code is under exclusive copyright by default and cannot ship inside an MIT artifact`)
        continue
      }
      if (pkg.malformed) {
        problems.push(`${where} declares "${pkg.expression}", which is not a valid SPDX expression (${pkg.malformed}) — it cannot be checked, so it cannot be shipped`)
        continue
      }
      if (!pkg.choice) {
        // evaluate() already proved no combination is satisfiable. Name the
        // identifiers that are missing from the allow list, so the message
        // says what to decide about rather than just "not allowed".
        const unlisted = unlistedLeaves(parse(pkg.expression))
        problems.push(`${where} is "${pkg.expression}" — no combination of that expression is on the allow list (not listed: ${unlisted.join(', ')})`)
        continue
      }
      if (pkg.texts.length === 0) {
        // Hard failure, not a warning. MIT, ISC and every other licence on
        // the allow list require the copyright notice to travel with the
        // distributed binary, and a repository URL is not that notice. The
        // escape hatch is scripts/licenses/vendored/ — a reviewed, committed
        // text — which is deliberately a decision someone has to make rather
        // than a line of output they can scroll past.
        problems.push(`${where} declares ${pkg.choice.id} but publishes no licence text, and has no entry in scripts/licenses/vendored/manifest.json — ${pkg.choice.id} requires its notice to ship with the binary. Fetch the text from the package's canonical source, add it under scripts/licenses/vendored/, and record the source URL in the manifest.`)
        continue
      }
      if (pkg.vendored) {
        // A compound expression must not be half-attributed: `MIT AND ISC`
        // needs both notices, and vendoring only the MIT half would look
        // complete while leaving the ISC half unattributed.
        const required = pkg.choice.id.split(' AND ')
        const covered = new Set(pkg.vendored.files.flatMap((f) => f.covers ?? []))
        const uncovered = required.filter((id) => !covered.has(id))
        if (uncovered.length) {
          problems.push(`${where} resolves to ${pkg.choice.id}, but its vendored entry only covers ${[...covered].join(', ') || 'nothing'} — no reproduced notice for ${uncovered.join(', ')}`)
        }
      }
    }
  }

  for (const project of projects) {
    for (const u of project.unreadable) {
      const where = [u.constraints.os && `os=${[].concat(u.constraints.os).join('|')}`, u.constraints.cpu && `cpu=${[].concat(u.constraints.cpu).join('|')}`].filter(Boolean).join(' ')
      warnings.push(`${project.dir}: ${u.name}@${u.version} is platform-restricted (${where}) and is not installed here, so its licence could not be read or reproduced — re-run this check on a matching platform, or confirm it is not bundled into a shipped artifact`)
    }
  }

  for (const w of warnings) console.warn(`  warning: ${w}`)

  if (problems.length) {
    console.error('\nnpm licence check FAILED:\n')
    for (const p of problems) console.error(`  - ${p}`)
    console.error(`
Adding a licence to the allow list is a licensing decision, not a build fix.
The list lives in scripts/licenses/spdx.mjs (ALLOWED) and must stay in step
with \`licenses.allow\` in deny.toml.`)
    process.exit(1)
  }

  const total = projects.reduce((n, p) => n + p.packages.length, 0)
  const vendoredCount = projects.reduce((n, p) => n + p.packages.filter((pkg) => pkg.vendored).length, 0)
  console.log(`npm licence check OK — ${total} shipped package(s) across ${projects.length} project(s), all within the allow list, all with a reproduced notice${vendoredCount ? ` (${vendoredCount} vendored from upstream)` : ''}.`)
}

/**
 * Write the npm half of THIRD-PARTY-NOTICES.md to stdout, grouped by resolved
 * licence, with each package's notice text reproduced verbatim.
 *
 * @param {ReturnType<typeof collect>} projects
 */
function notices(projects) {
  // Group by resolved licence so identical text is reproduced once, matching
  // how cargo-about lays out the Rust half.
  const out = []
  for (const project of projects) {
    out.push(`### ${project.label}`, '')
    out.push(`Source: \`${project.dir}\` — ${project.packages.length} package(s) in the production dependency closure of \`bun.lock\`. Build-only tooling (vite, TypeScript, Tailwind/lightningcss, Playwright) is excluded: it is never bundled into a distributed artifact.`, '')
    for (const u of project.unreadable) {
      out.push(`> \`${u.name}@${u.version}\` is platform-restricted and was not installed on the machine that generated this file, so its licence text is not reproduced here.`, '')
    }

    const byLicense = new Map()
    for (const pkg of project.packages) {
      const id = pkg.choice?.id ?? 'UNKNOWN'
      if (!byLicense.has(id)) byLicense.set(id, [])
      byLicense.get(id).push(pkg)
    }

    for (const id of [...byLicense.keys()].sort()) {
      const pkgs = byLicense.get(id)
      out.push(`#### ${id}`, '')
      for (const pkg of pkgs) {
        const chosen = pkg.choice?.alternatives
          ? ` (declared \`${pkg.expression}\`; this project relies on the ${id} branch)`
          : ''
        out.push(`- **${pkg.name}** ${pkg.version}${chosen}${pkg.repository ? ` — ${pkg.repository}` : ''}`)
      }
      out.push('')
      // One representative text per (licence, package) pair: licences such as
      // MIT and BSD carry a per-copyright-holder notice, so they cannot be
      // collapsed to a single shared body the way an Apache-2.0 or MPL-2.0
      // text could.
      for (const pkg of pkgs) {
        if (pkg.vendored) {
          // Say where a vendored text came from. A notice whose provenance is
          // unstated is only marginally better than no notice.
          out.push(`> \`${pkg.name}@${pkg.version}\` publishes no licence text in its npm tarball. The notice below is reproduced from the package's canonical source, recorded in \`scripts/licenses/vendored/manifest.json\`.`, '')
        }
        for (const t of pkg.texts) {
          const provenance = t.vendoredFrom ? ` (vendored from ${t.vendoredFrom})` : ''
          out.push(`<details><summary><code>${pkg.name}@${pkg.version}</code> — ${t.file}${provenance}</summary>`, '', '````text', t.text, '````', '', '</details>', '')
        }
      }
    }
  }
  process.stdout.write(out.join('\n'))
}

/**
 * Emit the shipped npm packages as CycloneDX components on stdout, keyed by
 * project directory so the caller can fold each set into the right binary's
 * BOM.
 *
 * Deliberately *not* a standalone tool run: `@cyclonedx/cyclonedx-npm` shells
 * out to `npm ls` and wants an npm lockfile, which this repo does not have.
 * More importantly, a second tool would compute its own notion of "what
 * ships" — and two answers to that question is exactly the failure mode this
 * whole area exists to prevent. Reusing `collect()` means the SBOM and
 * THIRD-PARTY-NOTICES.md cannot disagree, because they are one computation.
 *
 * @param {ReturnType<typeof collect>} projects
 */
function sbom(projects) {
  const out = {}
  for (const project of projects) {
    out[project.dir] = {
      label: project.label,
      components: project.packages.map((pkg) => npmComponent(pkg)),
      // Named, not dropped: a platform-gated package ships on its own
      // platform, and an SBOM that silently omits it is wrong in the
      // direction that matters.
      unresolved: project.unreadable.map((u) => ({ name: u.name, version: u.version, constraints: u.constraints })),
    }
  }
  process.stdout.write(JSON.stringify(out, null, 2))
}

const mode = process.argv[2]
if (!['--check', '--notices', '--sbom'].includes(mode)) {
  console.error('usage: node scripts/licenses/npm-licenses.mjs (--check | --notices | --sbom)')
  process.exit(2)
}
try {
  const projects = collect()
  if (mode === '--check') check(projects)
  else if (mode === '--notices') notices(projects)
  else sbom(projects)
} catch (err) {
  console.error(`npm-licenses: ${err.message}`)
  process.exit(1)
}
