/**
 * Minimal RFC 4180 CSV serialisation + parsing for client-side "export
 * what's on screen" / "import a CSV back in" flows (no server endpoint
 * involved — the caller already has the rows, or hands the parsed rows
 * straight to one). Kept intentionally small rather than pulling in a
 * dependency: quoting/unquoting a value that contains a comma, quote, or
 * line break is the only rule Excel actually needs.
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
 * Parse RFC 4180 CSV text into rows of raw string fields — the counterpart
 * to `toCsv`, for client-side "import a CSV I exported (or edited in Excel)"
 * flows. A hand-rolled state machine rather than a `.split(',')` /
 * `.split('\n')` pair because both of those break on exactly the cells
 * `toCsv` knows how to quote: a value containing a comma, a quoted value
 * spanning multiple physical lines, or a doubled `""` escaping a literal
 * quote. Handles `\r\n`, bare `\n`, and a leading UTF-8 BOM (which Excel
 * writes and `toCsv` also emits).
 */
export function parseCsv(text: string): string[][] {
  const src = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
  const rows: string[][] = [];
  let row: string[] = [];
  let field = '';
  let inQuotes = false;
  let i = 0;
  const n = src.length;
  const endField = () => {
    row.push(field);
    field = '';
  };
  const endRow = () => {
    endField();
    rows.push(row);
    row = [];
  };
  while (i < n) {
    const c = src[i];
    if (inQuotes) {
      if (c === '"') {
        if (src[i + 1] === '"') {
          field += '"';
          i += 2;
        } else {
          inQuotes = false;
          i++;
        }
      } else {
        field += c;
        i++;
      }
      continue;
    }
    if (c === '"') {
      inQuotes = true;
      i++;
    } else if (c === ',') {
      endField();
      i++;
    } else if (c === '\r') {
      endRow();
      i += src[i + 1] === '\n' ? 2 : 1;
    } else if (c === '\n') {
      endRow();
      i++;
    } else {
      field += c;
      i++;
    }
  }
  // A trailing newline leaves nothing pending; anything else (including a
  // file with no trailing newline at all) is one more row to flush.
  if (field !== '' || row.length > 0) endRow();
  // Drop wholly-blank rows (a trailing blank line, or one in the middle) —
  // a row with a real pc_id always has at least one non-empty field.
  return rows.filter((r) => r.some((v) => v !== ''));
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
