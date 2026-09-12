// Unit tests for the CycloneDX shaping helpers.
//
// Run with `node --test "scripts/licenses/**/*.test.mjs"` (or
// `cargo make licenses-test`).
//
// These exist because the first version of this code shipped two defects that
// only manual inspection of the output caught: an absolute build path leaking
// into a published asset, and a purl shape that no spec-conforming parser
// would read the way it was meant. Both are pinned below. A lockfile bump
// should not require someone to remember to look again.

import test from 'node:test'
import assert from 'node:assert/strict'
import { npmPurl, cycloneDxHash, npmComponent, normalizeBuildPaths } from './cyclonedx.mjs'

// --- purl identifiers -------------------------------------------------------

test('an unscoped package is a plain name segment', () => {
  assert.equal(npmPurl('dompurify', '3.4.14'), 'pkg:npm/dompurify@3.4.14')
})

test('regression: a scope is a namespace segment, joined by a LITERAL slash', () => {
  // The previous version produced `pkg:npm/@babel%2Fruntime@7.29.7`, encoding
  // away the separator that a parser splits on.
  assert.equal(npmPurl('@babel/runtime', '7.29.7'), 'pkg:npm/%40babel/runtime@7.29.7')
  assert.equal(npmPurl('@radix-ui/react-dialog', '1.1.23'), 'pkg:npm/%40radix-ui/react-dialog@1.1.23')
})

test('regression: a scoped purl parses back to namespace + name', () => {
  // The property that actually matters: an advisory matcher keys on
  // namespace+name, so this is what decides whether a component is found.
  const parse = (purl) => {
    const body = purl.slice('pkg:npm/'.length)
    const path = body.slice(0, body.lastIndexOf('@'))
    const segments = path.split('/')
    return segments.length > 1
      ? { namespace: decodeURIComponent(segments[0]), name: segments.slice(1).join('/') }
      : { namespace: null, name: decodeURIComponent(segments[0]) }
  }
  assert.deepEqual(parse(npmPurl('@babel/runtime', '7.29.7')), { namespace: '@babel', name: 'runtime' })
  assert.deepEqual(parse(npmPurl('dompurify', '3.4.14')), { namespace: null, name: 'dompurify' })
})

test('a bare @-prefixed name with no slash is not treated as a namespace', () => {
  assert.equal(npmPurl('@weird', '1.0.0'), 'pkg:npm/%40weird@1.0.0')
})

// --- integrity hashes -------------------------------------------------------

test('sha512 integrity decodes to hex of the right length', () => {
  // dompurify@3.4.14's real integrity string from bun.lock.
  const hash = cycloneDxHash('sha512-dVoHc/OMY6Bm5Hf3Dk1sQmnNiNU0ZBIJS4Vl9G7GOwRNJUcUMZBIUoCXpJDNpCTUM6BZqnMOJ6c1QIIEsI8n5A==')
  assert.equal(hash.alg, 'SHA-512')
  assert.equal(hash.content.length, 128) // 64 bytes as hex
  assert.match(hash.content, /^[0-9a-f]+$/)
})

test('a known base64 decodes to the exact expected hex', () => {
  // Buffer.from('deadbeef', 'hex').toString('base64') === '3q2+7w=='
  assert.deepEqual(cycloneDxHash('sha256-3q2+7w=='), { alg: 'SHA-256', content: 'deadbeef' })
})

test('each supported algorithm maps to its CycloneDX name', () => {
  assert.equal(cycloneDxHash('sha1-3q2+7w==').alg, 'SHA-1')
  assert.equal(cycloneDxHash('sha384-3q2+7w==').alg, 'SHA-384')
})

test('an absent or unrecognised integrity yields no hash rather than a wrong one', () => {
  for (const bad of [null, undefined, '', 'md5-3q2+7w==', 'sha512', 'not-an-integrity']) {
    assert.equal(cycloneDxHash(bad), null, `expected null for ${JSON.stringify(bad)}`)
  }
})

// --- component shape --------------------------------------------------------

test('a component carries the resolved licence branch, not the declaration', () => {
  // dompurify declares `MPL-2.0 OR Apache-2.0`; a consumer needs to know which
  // branch this project relies on.
  const c = npmComponent({ name: 'dompurify', version: '3.4.14', choice: { id: 'Apache-2.0' }, integrity: null, vendored: null })
  assert.deepEqual(c.licenses, [{ expression: 'Apache-2.0' }])
  assert.equal(c.purl, 'pkg:npm/dompurify@3.4.14')
  assert.equal(c['bom-ref'], c.purl)
  assert.equal(c.scope, 'required')
  assert.equal(c.hashes, undefined)
})

test('a vendored notice is recorded as a property', () => {
  const c = npmComponent({ name: 'victory-vendor', version: '37.3.6', choice: { id: 'MIT AND ISC' }, integrity: null, vendored: { files: [] } })
  assert.deepEqual(c.properties, [{ name: 'kanade:notice-source', value: 'vendored' }])
})

test('a package with no resolvable licence omits the field rather than guessing', () => {
  const c = npmComponent({ name: 'x', version: '1.0.0', choice: null, integrity: null, vendored: null })
  assert.equal(c.licenses, undefined)
})

// --- build-path normalisation ----------------------------------------------

test('regression: the absolute build path is removed from refs AND cross-references', () => {
  // The defect this pins: rewriting only the definition would leave
  // `dependsOn` pointing at a node that no longer exists.
  const root = '/home/runner/work/kanade/kanade'
  const bom = {
    metadata: { component: { 'bom-ref': `path+file://${root}/crates/kanade-backend#0.45.17`, purl: 'pkg:cargo/kanade-backend@0.45.17?download_url=file://.' } },
    components: [{ 'bom-ref': 'registry+https://github.com/rust-lang/crates.io-index#serde@1.0.229' }],
    dependencies: [
      { ref: `path+file://${root}/crates/kanade-backend#0.45.17`, dependsOn: ['registry+https://github.com/rust-lang/crates.io-index#serde@1.0.229'] },
      { ref: `path+file://${root}/crates/kanade-shared#0.45.17`, dependsOn: [`path+file://${root}/crates/kanade-backend#0.45.17`] },
    ],
  }
  const out = normalizeBuildPaths(bom, root)
  const text = JSON.stringify(out)

  assert.ok(!text.includes(root), 'no absolute build path may survive')
  assert.equal(out.metadata.component['bom-ref'], 'path+file:crates/kanade-backend#0.45.17')
  assert.equal(out.metadata.component.purl, 'pkg:cargo/kanade-backend@0.45.17')
  // The definition and every reference to it must still agree.
  assert.equal(out.dependencies[0].ref, out.metadata.component['bom-ref'])
  assert.equal(out.dependencies[1].dependsOn[0], out.metadata.component['bom-ref'])
})

test('no reference dangles after normalisation', () => {
  const root = '/build/here'
  const bom = {
    metadata: { component: { 'bom-ref': `path+file://${root}/a#1.0.0` } },
    components: [{ 'bom-ref': `path+file://${root}/b#1.0.0` }],
    dependencies: [{ ref: `path+file://${root}/a#1.0.0`, dependsOn: [`path+file://${root}/b#1.0.0`] }],
  }
  const out = normalizeBuildPaths(bom, root)
  const defined = new Set([out.metadata.component['bom-ref'], ...out.components.map((c) => c['bom-ref'])])
  const referenced = out.dependencies.flatMap((d) => [d.ref, ...(d.dependsOn ?? [])])
  assert.deepEqual(referenced.filter((r) => !defined.has(r)), [])
})

test('a document with no absolute path is returned untouched', () => {
  const bom = { components: [{ 'bom-ref': 'pkg:npm/dompurify@3.4.14' }] }
  assert.equal(normalizeBuildPaths(bom, '/nowhere'), bom)
})
