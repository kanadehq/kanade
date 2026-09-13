/**
 * Minimal RFC 4180 CSV serialisation for client-side "export what's on
 * screen" buttons (no server endpoint involved — the caller already has
 * the rows). Kept intentionally small rather than pulling in a dependency:
 * quoting a value that contains a comma, quote, or line break is the only
 * rule Excel actually needs.
 */

/**
 * Characters that make a spreadsheet application read a cell as a formula
 * (or, for `\t`/`\r`, feed it to a DDE handler) instead of literal text —
 * the classic CSV/formula-injection vector. Guarded here, not at the call
 * sites: `toCsv`'s rows come from data an agent or operator supplied
 * (hostnames, agent_meta values), any of which could contain a value like
 * `=cmd|'/c calc'!A1` that a spreadsheet would otherwise execute on open.
 */
const FORMULA_TRIGGERS = ['=', '+', '-', '@', '\t', '\r'];

/** Prefix a value with an apostrophe if it would otherwise be read as a
 *  formula — the standard mitigation, since it forces text interpretation
 *  without changing what the cell displays. */
function guardFormula(value: string): string {
  return FORMULA_TRIGGERS.some((p) => value.startsWith(p)) ? `'${value}` : value;
}

/** Quote one field when it contains a comma, quote, or line break, doubling
 *  any embedded quotes. Plain values pass through unquoted. */
function csvField(value: string): string {
  const guarded = guardFormula(value);
  return /[",\r\n]/.test(guarded) ? `"${guarded.replace(/"/g, '""')}"` : guarded;
}

/**
 * Serialise rows of plain strings into CSV text.
 *
 * CRLF line endings (the ending Excel writes itself) and a leading UTF-8
 * BOM — without it Excel guesses the system codepage for a `.csv` file and
 * mangles anything non-ASCII (Japanese labels, host/user names).
 */
export function toCsv(rows: string[][]): string {
  return '﻿' + rows.map((row) => row.map(csvField).join(',')).join('\r\n');
}

/**
 * Trigger a browser download of `rows` as a CSV file named `filename`.
 * Mirrors the Collect page's blob-download pattern (object URL, revoked on
 * a delay — revoking immediately after `click()` aborts the download in
 * some browsers).
 */
export function downloadCsv(filename: string, rows: string[][]): void {
  const blob = new Blob([toCsv(rows)], { type: 'text/csv;charset=utf-8;' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
