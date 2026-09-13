/**
 * Minimal RFC 4180 CSV serialisation for client-side "export what's on
 * screen" buttons (no server endpoint involved — the caller already has
 * the rows). Kept intentionally small rather than pulling in a dependency:
 * quoting a value that contains a comma, quote, or line break is the only
 * rule Excel actually needs.
 */

/** Quote one field when it contains a comma, quote, or line break, doubling
 *  any embedded quotes. Plain values pass through unquoted. */
function csvField(value: string): string {
  return /[",\r\n]/.test(value) ? `"${value.replace(/"/g, '""')}"` : value;
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
