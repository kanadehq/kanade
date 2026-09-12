// cyclonedx.mjs — the pure shaping helpers for CycloneDX output.
//
// Split out of npm-licenses.mjs and sbom.mjs so they can be unit-tested
// (`node --test scripts/licenses/`). Both of those files run work on import,
// which is what made the identifier handling here unreachable from a test —
// the same reason spdx.mjs exists.
//
// Everything here is a pure function of its arguments. Nothing reads the
// filesystem or the environment.

/**
 * Build a package URL for an npm package.
 *
 * The scope of a scoped package is a purl **namespace**, which is its own
 * path segment joined to the name by a LITERAL, unencoded `/`:
 *
 *     @babel/runtime  ->  pkg:npm/%40babel/runtime@7.29.7
 *                                 ^^^^^^^ ^^^^^^^
 *                                 namespace   name
 *
 * Percent-encoding the separator instead — `pkg:npm/@babel%2Fruntime@7.29.7`
 * — is not a formatting nit. A spec-conforming parser splits on the literal
 * `/`, so the encoded form reads back as `namespace=null,
 * name="@babel/runtime"`. Advisory matchers key on namespace+name, so every
 * scoped component (54 of the 129 shipped here) would fail to match the feed
 * it is supposed to be queried against — defeating the only reason to emit a
 * purl at all.
 *
 * @param {string} name npm package name, scoped or not
 * @param {string} version
 * @returns {string} canonical purl
 */
export function npmPurl(name, version) {
  if (name.startsWith('@')) {
    const slash = name.indexOf('/')
    if (slash !== -1) {
      const namespace = encodeURIComponent(name.slice(0, slash))
      const bare = encodeURIComponent(name.slice(slash + 1))
      return `pkg:npm/${namespace}/${bare}@${version}`
    }
  }
  return `pkg:npm/${encodeURIComponent(name)}@${version}`
}

/**
 * Convert npm's `sha512-<base64>` integrity string to a CycloneDX hash entry.
 *
 * @param {string|null|undefined} integrity
 * @returns {{alg: string, content: string}|null} null for an absent or
 *   unrecognised algorithm — a missing hash is better than a wrong one.
 */
export function cycloneDxHash(integrity) {
  if (!integrity) return null
  const match = /^(sha512|sha384|sha256|sha1)-(.+)$/.exec(integrity)
  if (!match) return null
  const alg = { sha512: 'SHA-512', sha384: 'SHA-384', sha256: 'SHA-256', sha1: 'SHA-1' }[match[1]]
  try {
    return { alg, content: Buffer.from(match[2], 'base64').toString('hex') }
  } catch {
    return null
  }
}

/**
 * One CycloneDX `component` for a shipped npm package.
 *
 * @param {{name: string, version: string, choice: {id: string}|null,
 *          integrity: string|null, vendored: object|null}} pkg
 * @returns {object} CycloneDX component
 */
export function npmComponent(pkg) {
  const purl = npmPurl(pkg.name, pkg.version)
  const component = {
    type: 'library',
    'bom-ref': purl,
    name: pkg.name,
    version: pkg.version,
    purl,
    scope: 'required',
  }
  if (pkg.choice) {
    // The branch this project relies on, not the raw declaration: for
    // `MPL-2.0 OR Apache-2.0` a consumer needs to know which one we took.
    // `expression` rather than `license.id` because an AND of two ids is not
    // expressible as a single id.
    component.licenses = [{ expression: pkg.choice.id }]
  }
  const hash = cycloneDxHash(pkg.integrity)
  if (hash) component.hashes = [hash]
  if (pkg.vendored) {
    component.properties = [{ name: 'kanade:notice-source', value: 'vendored' }]
  }
  return component
}

/**
 * Replace the absolute build path cargo-cyclonedx bakes into workspace-local
 * refs with a repo-relative one.
 *
 * It emits `path+file:///home/runner/work/kanade/kanade/crates/...` for
 * members built from a path, and the same string appears again in every
 * `dependencies[].ref` / `dependsOn` entry pointing at one. Published as-is
 * that leaks the build machine's layout into a release asset and makes two
 * builds of the same tag differ for no reason.
 *
 * Rewritten as a whole-document substitution rather than field by field,
 * precisely because the ref is a cross-reference: changing the definition and
 * missing a `dependsOn` would leave a dependency graph pointing at nodes that
 * no longer exist.
 *
 * @param {object} bom parsed CycloneDX document
 * @param {string} repoRoot absolute path to the repository root
 * @returns {object} a document with `<repoRoot>` paths made relative
 */
export function normalizeBuildPaths(bom, repoRoot) {
  let text = JSON.stringify(bom)
  if (!text.includes(repoRoot)) return bom
  const fileUrl = `file://${repoRoot}`
  // `path+file:///abs/crates/x` -> `path+file:crates/x`
  text = text.split(`path+${fileUrl}/`).join('path+file:')
  // `?download_url=file://.` is already relative but meaningless in a
  // published artifact; drop the qualifier rather than shipping a URL that
  // resolves to wherever the consumer happens to stand.
  text = text.split('?download_url=file://.').join('')
  // Anything else still carrying the absolute root becomes repo-relative too.
  text = text.split(`${fileUrl}/`).join('file:')
  return JSON.parse(text)
}
