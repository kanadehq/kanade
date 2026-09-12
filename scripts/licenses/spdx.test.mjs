// Unit tests for the SPDX expression evaluator.
//
// Run with `node --test scripts/licenses/` (or `cargo make licenses-test`).
// No test framework: node's built-in runner keeps this dependency-free, which
// matters for a script that has to work in CI before `bun install` has run.
//
// The cases below are not decoration. Every "regression" test here is a shape
// the previous regex-based implementation got WRONG, and none of them exist in
// today's dependency tree — which is precisely why they need pinning. A gate
// that is only correct for the inputs it has already seen provides no
// assurance about the input that eventually breaks it.

import test from 'node:test'
import assert from 'node:assert/strict'
import { evaluate, parse, render, unlistedLeaves, ALLOWED } from './spdx.mjs'

// A small fixed policy, so these tests describe the PARSER and not whatever
// the real allow list happens to contain today.
const allowed = new Set(['MIT', 'ISC', 'Apache-2.0', 'BSD-3-Clause', 'MPL-2.0', 'Apache-2.0 WITH LLVM-exception'])
const preference = ['MIT', 'ISC', 'BSD-3-Clause', 'Apache-2.0', 'MPL-2.0']
const evalWith = (expr) => evaluate(expr, allowed, preference)

test('a bare identifier resolves to itself', () => {
  assert.deepEqual(evalWith('MIT'), { id: 'MIT', alternatives: null })
})

test('a disallowed identifier is unsatisfiable', () => {
  assert.equal(evalWith('GPL-3.0-only'), null)
})

test('AND requires every operand, and reports them all', () => {
  assert.deepEqual(evalWith('MIT AND ISC'), { id: 'MIT AND ISC', alternatives: null })
})

test('AND fails if any single operand is disallowed', () => {
  assert.equal(evalWith('MIT AND GPL-3.0-only'), null)
})

test('OR picks the most preferred allowed branch and records the choice', () => {
  assert.deepEqual(evalWith('Apache-2.0 OR MIT'), {
    id: 'MIT',
    alternatives: ['Apache-2.0', 'MIT'],
  })
})

test('OR falls back to an allowed branch that is not in the preference list', () => {
  const narrow = new Set(['CC0-1.0'])
  assert.deepEqual(evaluate('GPL-3.0-only OR CC0-1.0', narrow, preference), {
    id: 'CC0-1.0',
    alternatives: ['GPL-3.0-only', 'CC0-1.0'],
  })
})

test('OR is unsatisfiable only when every branch is disallowed', () => {
  assert.equal(evalWith('GPL-3.0-only OR AGPL-3.0-only'), null)
})

test('outer parentheses do not change the meaning', () => {
  assert.deepEqual(evalWith('(Apache-2.0 OR MIT)'), {
    id: 'MIT',
    alternatives: ['Apache-2.0', 'MIT'],
  })
})

test('WITH binds its exception into one identifier', () => {
  // Would be three separate tokens under a naive split, and
  // `Apache-2.0 WITH LLVM-exception` would never match the allow list.
  assert.deepEqual(evalWith('Apache-2.0 WITH LLVM-exception'), {
    id: 'Apache-2.0 WITH LLVM-exception',
    alternatives: null,
  })
})

test('an AND of identical resolutions collapses (aws-lc-rs shape)', () => {
  // Real expression: `ISC AND (Apache-2.0 OR ISC)`. The set of licences that
  // applies is {ISC}, so "ISC AND ISC" would be noise.
  assert.deepEqual(evalWith('ISC AND (Apache-2.0 OR ISC)'), { id: 'ISC', alternatives: null })
})

// --- Regression tests: shapes the previous regex implementation got wrong ---

test('regression: a parenthesised AND inside a top-level OR does not leak', () => {
  // Previously produced "GPL-3.0-only AND MIT" and rejected the package,
  // even though the OR offers plain MIT. False rejection.
  assert.deepEqual(evalWith('(GPL-3.0-only AND BSD-3-Clause) OR MIT'), {
    id: 'MIT',
    alternatives: ['(GPL-3.0-only AND BSD-3-Clause)', 'MIT'],
  })
})

test('regression: AND binds tighter than OR', () => {
  // `MIT OR GPL-3.0-only AND ISC` is `MIT OR (GPL-3.0-only AND ISC)`.
  // Previously produced "MIT AND ISC" — a combination never on offer.
  assert.deepEqual(evalWith('MIT OR GPL-3.0-only AND ISC'), {
    id: 'MIT',
    alternatives: ['MIT', '(GPL-3.0-only AND ISC)'],
  })
})

test('regression: a three-way AND with a parenthesised middle clause', () => {
  assert.deepEqual(evalWith('ISC AND (BSD-3-Clause OR GPL-3.0-only) AND MIT'), {
    id: 'ISC AND BSD-3-Clause AND MIT',
    alternatives: null,
  })
})

test('regression: an AND whose only satisfiable branch is behind an OR', () => {
  // If the parser flattened parentheses it would see GPL-3.0-only and reject.
  assert.deepEqual(evalWith('MIT AND (GPL-3.0-only OR ISC)'), {
    id: 'MIT AND ISC',
    alternatives: null,
  })
})

test('regression: nested OR inside OR still yields a single branch', () => {
  assert.deepEqual(evalWith('GPL-3.0-only OR (AGPL-3.0-only OR Apache-2.0)'), {
    id: 'Apache-2.0',
    alternatives: ['GPL-3.0-only', '(AGPL-3.0-only OR Apache-2.0)'],
  })
})

// --- Malformed input must fail loudly, never resolve to something -----------

test('malformed expressions throw rather than guessing', () => {
  for (const bad of ['MIT AND', 'AND MIT', '(MIT OR ISC', 'MIT OR ISC)', '', 'MIT WITH']) {
    assert.throws(() => evalWith(bad), /unparseable SPDX expression/, `expected "${bad}" to throw`)
  }
})

// --- Helpers used by the error messages ------------------------------------

test('unlistedLeaves names every identifier missing from the allow list', () => {
  assert.deepEqual(unlistedLeaves(parse('GPL-3.0-only AND (MIT OR AGPL-3.0-only)'), allowed), [
    'GPL-3.0-only',
    'AGPL-3.0-only',
  ])
})

test('render round-trips an expression back to readable source', () => {
  assert.equal(render(parse('MIT OR (ISC AND Apache-2.0)')), 'MIT OR (ISC AND Apache-2.0)')
})

// --- A guard on the real policy, not just the test fixture ------------------

test('the real allow list contains no copyleft stronger than MPL-2.0', () => {
  for (const id of ALLOWED) {
    assert.ok(!/^(A?GPL|LGPL)/i.test(id), `${id} must not be on the allow list`)
  }
})
