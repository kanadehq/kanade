#!/usr/bin/env node
// notices.mjs — regenerates THIRD-PARTY-NOTICES.md from the two lockfiles.
//
//   node scripts/licenses/notices.mjs            # write THIRD-PARTY-NOTICES.md
//   node scripts/licenses/notices.mjs --check    # fail if the committed file is stale
//
// `cargo make notices` and `cargo make notices-check` wrap both.
//
// Why the file is committed rather than generated at release time: the notices
// have to ship with the binaries, and `release.yml` builds on four runners
// that would each have to install cargo-about and reproduce an identical file.
// Committing it makes the content reviewable in the PR that changes a
// dependency — which is where a licence change should actually be noticed —
// and `--check` in CI is what stops it from drifting.
//
// Prerequisites (both are checked with a real error message, not a stack
// trace, because the failure mode is somebody running this for the first time):
//   cargo install cargo-about --locked --features cli
//       ^ the `cli` feature is NOT default; without it `cargo install
//         cargo-about` installs no binary at all and only warns.
//   bun install --frozen-lockfile   in each crates/*/web

import { execFileSync } from 'node:child_process'
import { readFileSync, writeFileSync, existsSync } from 'node:fs'
import { join, dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..')
const OUTPUT = join(REPO_ROOT, 'THIRD-PARTY-NOTICES.md')

// `what` is the remedy, and it is appended to BOTH failure paths on purpose.
// A missing cargo subcommand does not surface as ENOENT — `cargo` itself
// exists, so `cargo about` when cargo-about is not installed exits 101 with
// "no such command", and reporting only the status code would hide the one
// instruction that fixes it.
/**
 * Run a command and return its stdout, turning a failure into an error that
 * names the remedy rather than just a status code.
 *
 * @param {string} cmd
 * @param {string[]} args
 * @param {string} what how to fix it if the command is missing
 * @returns {string} stdout
 */
function run(cmd, args, what) {
  try {
    return execFileSync(cmd, args, { cwd: REPO_ROOT, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024, stdio: ['ignore', 'pipe', 'inherit'] })
  } catch (err) {
    if (err.code === 'ENOENT') throw new Error(`${cmd} is not installed — ${what}`)
    throw new Error(`\`${cmd} ${args.join(' ')}\` failed with status ${err.status} — if the command itself is missing: ${what}`)
  }
}

// The workspace's own crates are covered by the root LICENSE, not by a
// *third-party* notice. cargo-about has no switch for this (its
// `private.ignore` keys off `publish = false`, and every member here does
// publish to crates.io), so filter them out afterwards — reading the member
// list from cargo rather than hardcoding it, so adding a sixth crate cannot
// silently start listing us as our own third party.
/**
 * Names of this workspace's own crates, read from cargo rather than hardcoded
 * so a new member cannot silently start appearing as our own third party.
 *
 * @returns {Set<string>}
 */
function workspaceMembers() {
  const meta = JSON.parse(run('cargo', ['metadata', '--no-deps', '--format-version', '1'], 'install Rust'))
  return new Set(meta.packages.map((p) => p.name))
}

/**
 * The Rust half of the notices: cargo-about's output with our own crates
 * filtered out, prefixed by a summary table counted from what survives.
 *
 * @param {Set<string>} members workspace crate names to omit
 * @returns {string} markdown
 */
function rustSection(members) {
  const raw = run('cargo', ['about', 'generate', 'about.hbs'], 'run: cargo install cargo-about --locked --features cli')

  // Everything before the first licence heading is template preamble, not
  // content. Dropping it keeps a stray byte from a malformed handlebars
  // comment (the short `{{!` form ends at the first closing braces it sees)
  // out of the notices instead of silently into them.
  const firstHeading = raw.indexOf('#### ')
  if (firstHeading === -1) throw new Error('cargo about produced no licence sections — check about.hbs and about.toml')
  const lines = raw.slice(firstHeading).split('\n')
  const kept = []
  let dropped = 0
  for (const line of lines) {
    // Bullets emitted by about.hbs look like: `- **crate-name** 1.2.3 — url`
    const match = /^- \*\*([^*]+)\*\* /.exec(line)
    if (match && members.has(match[1])) { dropped++; continue }
    kept.push(line)
  }
  if (dropped === 0) {
    // Not fatal, but it means the filter silently stopped matching — most
    // likely because about.hbs changed shape. Say so loudly rather than
    // shipping a notices file that quietly lists our own crates.
    console.warn('notices: warning: no workspace-member bullets were filtered out — check that about.hbs still emits `- **name** version` lines')
  }

  // Dropping bullets can leave a licence section with a heading, no crates
  // and an orphaned licence text. That only happens if a licence is used
  // *exclusively* by our own crates, which is exactly the case that should
  // not appear in a third-party file at all.
  const body = stripEmptySections(kept.join('\n'))
  return `${summaryTable(body)}\n\n### Notices\n\n${body.trim()}`
}

// Count crates per licence heading from the FILTERED body, so the table can
// never claim a different number from the list under it. (cargo-about's own
// `overview` is computed before the workspace-member filter runs.)
/**
 * Count crates per licence from the already-filtered body, so the table can
 * never disagree with the list beneath it.
 *
 * @param {string} body markdown produced by about.hbs, post-filter
 * @returns {string} a markdown table
 */
function summaryTable(body) {
  const rows = []
  let current = null
  for (const line of body.split('\n')) {
    const heading = /^#### (.+)$/.exec(line)
    if (heading) {
      // Several sections share one id: `MIT` appears once per distinct
      // copyright notice. Fold them together for the table.
      const id = /\(`([^`]+)`\)\s*$/.exec(heading[1])?.[1] ?? heading[1]
      const name = heading[1].replace(/\s*\(`[^`]+`\)\s*$/, '')
      current = rows.find((r) => r.id === id)
      if (!current) { current = { id, name, count: 0 }; rows.push(current) }
      continue
    }
    if (current && /^- \*\*/.test(line)) current.count++
  }
  rows.sort((a, b) => b.count - a.count || a.id.localeCompare(b.id))
  const total = rows.reduce((n, r) => n + r.count, 0)
  return [
    '### Summary',
    '',
    '| Licence | Crates |',
    '| --- | ---: |',
    ...rows.map((r) => `| ${r.name} (\`${r.id}\`) | ${r.count} |`),
    `| **Total** | **${total}** |`,
  ].join('\n')
}

/**
 * Drop licence sections left with a heading and no crates — which only happens
 * when a licence was used exclusively by our own workspace members.
 *
 * @param {string} markdown
 * @returns {string}
 */
function stripEmptySections(markdown) {
  const sections = markdown.split(/\n(?=#### )/)
  return sections
    .filter((section, i) => i === 0 || /^- \*\*/m.test(section))
    .join('\n')
}

/**
 * The npm half of the notices, delegated to npm-licenses.mjs so the two modes
 * of that script cannot drift apart.
 *
 * @returns {string} markdown
 */
function npmSection() {
  return run('node', [join('scripts', 'licenses', 'npm-licenses.mjs'), '--notices'], 'install Node.js')
}

/**
 * Assemble the complete THIRD-PARTY-NOTICES.md, preamble included.
 *
 * @returns {string} the full file contents, before line-ending normalisation.
 */
function build() {
  const members = workspaceMembers()
  const rust = rustSection(members)
  const npm = npmSection()

  return `<!-- GENERATED FILE — DO NOT EDIT BY HAND.
     Regenerate with \`cargo make notices\`; CI enforces freshness via
     \`cargo make notices-check\` in .github/workflows/licenses.yml.
     What may enter this file at all is decided by deny.toml (Rust) and the
     ALLOWED list in scripts/licenses/spdx.mjs (npm). -->

# Third-party notices

kanade itself is distributed under the MIT Licence — see [LICENSE](./LICENSE).

The binaries this project ships (\`kanade\`, \`kanade-agent\`, \`kanade-backend\`,
\`kanade-client\`) statically link Rust crates, and the backend binary also
embeds the compiled SPA via \`rust-embed\`. Those dependencies keep their own
licences, and the notices below travel with the binaries to satisfy them.

Two things worth stating plainly, because they are the questions this file
exists to answer:

- **No dependency is under the GPL, the AGPL, or an LGPL-only licence.**
  Nothing here obliges this project to relicense its own source.
- **Some dependencies are under the MPL-2.0**, which is copyleft *per file*:
  \`option-ext\` (via \`dirs\`, in all four binaries) and
  \`cssparser\` / \`selectors\` / \`dtoa-short\` (via Tauri/wry, in the desktop
  client). MPL-2.0 section 3.3 explicitly permits distributing a Larger Work
  under other terms, so the MIT licence above stands. Every one of them is
  consumed unmodified from crates.io; modifying one would require publishing
  the modified *files* under the MPL-2.0.
  On the npm side \`dompurify\` is dual \`MPL-2.0 OR Apache-2.0\`, and this
  project relies on the Apache-2.0 branch.

Where a package publishes no licence file of its own, that is stated inline
rather than papered over.

## Rust crates

${rust.trim()}

## npm packages

${npm.trim()}
`
}

const check = process.argv.includes('--check')
try {
  // Normalise line endings before anything compares or writes this file.
  //
  // Several upstream licence texts are stored with CRLF (pelite and
  // equivalent among them) and cargo-about reproduces them byte for byte.
  // `.gitattributes` sets `* text=auto eol=lf`, so git rewrites those to LF
  // on commit — meaning the committed file could never equal what the
  // generator produces, and `--check` would fail on every fresh checkout
  // forever. Normalising here makes the generator's output and the committed
  // bytes the same thing on every platform, which is the only way this gate
  // can be believed.
  //
  // Line endings are the only thing touched; no licence text is otherwise
  // altered.
  const content = build().replace(/\r\n/g, '\n').replace(/\r/g, '\n')
  if (check) {
    if (!existsSync(OUTPUT)) {
      console.error(`notices: ${OUTPUT} does not exist — run \`cargo make notices\` and commit the result`)
      process.exit(1)
    }
    if (readFileSync(OUTPUT, 'utf8') !== content) {
      console.error(`notices: THIRD-PARTY-NOTICES.md is out of date with the lockfiles — run \`cargo make notices\` and commit the result`)
      process.exit(1)
    }
    console.log('notices: THIRD-PARTY-NOTICES.md is up to date.')
  } else {
    writeFileSync(OUTPUT, content)
    console.log(`notices: wrote ${OUTPUT}`)
  }
} catch (err) {
  console.error(`notices: ${err.message}`)
  process.exit(1)
}
