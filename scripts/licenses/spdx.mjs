// spdx.mjs — the licence policy, and a real parser for SPDX expressions.
//
// Split out of npm-licenses.mjs so the parsing logic is importable and
// therefore testable (`node --test scripts/licenses/`). It gates a
// compliance check; it should not be the one part of this that nothing
// exercises.
//
// The previous implementation was regex-based: it tested for `AND` anywhere
// in the string and treated that as the top-level operator. That ignores
// both parentheses and SPDX's precedence rule, and it failed in *both*
// directions:
//
//   (GPL-3.0 AND BSD-3-Clause) OR MIT  ->  "GPL-3.0 AND MIT"
//       The top-level operator is OR and MIT alone is available, but it
//       fabricated a conjunction carrying GPL-3.0 and rejected the package.
//   MIT OR GPL-3.0 AND ISC            ->  "MIT AND ISC"
//       Invented a combination that was never on offer.
//
// Neither shape exists in today's tree, which is exactly why it needed
// fixing rather than documenting: a gate that is wrong only for inputs
// nobody has yet is a gate that fails the first time it matters.

// The allow list. Mirrors `licenses.allow` in deny.toml — one policy, two
// package managers, nothing mechanically tying them together. See the
// licensing section of AGENTS.md.
export const ALLOWED = new Set([
  'MIT', 'MIT-0', 'Apache-2.0', 'Apache-2.0 WITH LLVM-exception',
  'BSD-1-Clause', 'BSD-2-Clause', 'BSD-3-Clause', 'ISC', 'Zlib', '0BSD',
  'BSL-1.0', 'CC0-1.0', 'Unlicense', 'Unicode-3.0', 'CDLA-Permissive-2.0',
  'MPL-2.0',
])

// For an `A OR B` choice, which branch this project takes. MIT leads for the
// same reason as in about.toml: it is what this project itself ships under
// and it carries no NOTICE-file obligation. Anything not listed ranks last
// but is still selectable if it is the only allowed branch.
export const PREFERENCE = [
  'MIT', 'MIT-0', 'ISC', '0BSD', 'BSD-2-Clause', 'BSD-3-Clause', 'Zlib',
  'Apache-2.0', 'MPL-2.0',
]

// --- Parsing ---------------------------------------------------------------
// Grammar (SPDX 2.3, simplified to what package metadata actually uses):
//
//   expression := or-expr
//   or-expr    := and-expr (OR and-expr)*
//   and-expr   := primary (AND primary)*
//   primary    := '(' expression ')' | id ('WITH' id)?
//
// AND binds tighter than OR, so `A OR B AND C` is `A OR (B AND C)`.

function tokenize(source) {
  const tokens = []
  let i = 0
  while (i < source.length) {
    if (/\s/.test(source[i])) { i++; continue }
    if (source[i] === '(' || source[i] === ')') { tokens.push({ type: source[i] }); i++; continue }
    let j = i
    while (j < source.length && !/[\s()]/.test(source[j])) j++
    const word = source.slice(i, j)
    i = j
    const upper = word.toUpperCase()
    if (upper === 'AND' || upper === 'OR' || upper === 'WITH') tokens.push({ type: upper })
    else tokens.push({ type: 'ID', value: word })
  }
  return tokens
}

export function parse(expression) {
  const state = { tokens: tokenize(expression), pos: 0 }
  const ast = parseOr(state, expression)
  if (state.pos !== state.tokens.length) {
    throw new Error(`unparseable SPDX expression "${expression}": unexpected trailing input`)
  }
  return ast
}

const peek = (s) => s.tokens[s.pos]
const next = (s) => s.tokens[s.pos++]

function parseOr(state, source) {
  const children = [parseAnd(state, source)]
  while (peek(state)?.type === 'OR') { next(state); children.push(parseAnd(state, source)) }
  return children.length === 1 ? children[0] : { type: 'or', children }
}

function parseAnd(state, source) {
  const children = [parsePrimary(state, source)]
  while (peek(state)?.type === 'AND') { next(state); children.push(parsePrimary(state, source)) }
  return children.length === 1 ? children[0] : { type: 'and', children }
}

function parsePrimary(state, source) {
  const token = next(state)
  if (!token) throw new Error(`unparseable SPDX expression "${source}": unexpected end of input`)
  if (token.type === '(') {
    const inner = parseOr(state, source)
    const close = next(state)
    if (!close || close.type !== ')') {
      throw new Error(`unparseable SPDX expression "${source}": unbalanced parentheses`)
    }
    return inner
  }
  if (token.type !== 'ID') {
    throw new Error(`unparseable SPDX expression "${source}": unexpected "${token.type}"`)
  }
  let id = token.value
  // `Apache-2.0 WITH LLVM-exception` is one licence identifier, not two.
  if (peek(state)?.type === 'WITH') {
    next(state)
    const exception = next(state)
    if (!exception || exception.type !== 'ID') {
      throw new Error(`unparseable SPDX expression "${source}": WITH is missing its exception`)
    }
    id = `${id} WITH ${exception.value}`
  }
  return { type: 'license', id }
}

// --- Evaluation ------------------------------------------------------------

const rank = (id, preference) => {
  const i = preference.indexOf(id)
  return i === -1 ? preference.length : i
}

function evalNode(node, allowed, preference) {
  if (node.type === 'license') {
    return allowed.has(node.id) ? { id: node.id, rank: rank(node.id, preference) } : null
  }
  if (node.type === 'and') {
    // Every operand applies simultaneously, so all of them must be allowed.
    const parts = node.children.map((c) => evalNode(c, allowed, preference))
    if (parts.some((p) => p === null)) return null
    // Deduplicate: `ISC AND (Apache-2.0 OR ISC)` (aws-lc-rs, really) resolves
    // to ISC on both sides, and the set of licences that apply is {ISC}, not
    // "ISC AND ISC". Order is preserved so the result stays readable.
    const ids = [...new Set(parts.flatMap((p) => p.id.split(' AND ')))]
    return { id: ids.join(' AND '), rank: Math.max(...parts.map((p) => p.rank)) }
  }
  // A choice: keep the allowed branches and take the most preferred.
  const options = node.children.map((c) => evalNode(c, allowed, preference)).filter(Boolean)
  if (options.length === 0) return null
  options.sort((a, b) => a.rank - b.rank)
  return options[0]
}

// Source text for one node, used to report what the package actually offered.
export function render(node) {
  if (node.type === 'license') return node.id
  if (node.type === 'and') return node.children.map(renderNested).join(' AND ')
  return node.children.map(renderNested).join(' OR ')
}

const renderNested = (node) => (node.type === 'license' ? node.id : `(${render(node)})`)

// Leaf identifiers that are not on the allow list — for error messages only.
export function unlistedLeaves(node, allowed = ALLOWED, seen = []) {
  if (node.type === 'license') {
    if (!allowed.has(node.id) && !seen.includes(node.id)) seen.push(node.id)
    return seen
  }
  for (const child of node.children) unlistedLeaves(child, allowed, seen)
  return seen
}

/**
 * Reduce an SPDX expression to the single licence this project relies on.
 *
 * Returns `{ id, alternatives }` where `alternatives` lists the top-level
 * choices when the expression was a choice (so the notices can say "declared
 * X, relied on the Y branch"), or `null` when no combination is satisfiable
 * from the allow list. Throws on a malformed expression rather than guessing.
 */
export function evaluate(expression, allowed = ALLOWED, preference = PREFERENCE) {
  const ast = parse(expression)
  const result = evalNode(ast, allowed, preference)
  if (!result) return null
  return {
    id: result.id,
    // renderNested, not render: a compound branch is parenthesised so the
    // notices can say `declared "(GPL-3.0-only AND BSD-3-Clause) OR MIT"`
    // without the reader having to guess where the branches divide.
    alternatives: ast.type === 'or' ? ast.children.map(renderNested) : null,
  }
}

export { parse as parseExpression }
