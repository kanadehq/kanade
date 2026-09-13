import { describe, expect, test } from 'bun:test';

import { toCsv } from './csv';

describe('toCsv', () => {
  test('leads with a UTF-8 BOM so Excel doesn\'t mojibake non-ASCII text', () => {
    expect(toCsv([['a']])).toStartWith('﻿');
  });

  test('joins fields with commas and rows with CRLF', () => {
    expect(toCsv([['a', 'b'], ['c', 'd']])).toBe('﻿a,b\r\nc,d');
  });

  test('quotes a field containing a comma', () => {
    expect(toCsv([['a,b', 'c']])).toBe('﻿"a,b",c');
  });

  test('quotes a field containing a double quote, doubling it', () => {
    expect(toCsv([['say "hi"']])).toBe('﻿"say ""hi"""');
  });

  test('quotes a field containing a line break', () => {
    expect(toCsv([['line1\nline2']])).toBe('﻿"line1\nline2"');
  });

  test('leaves a plain field unquoted', () => {
    expect(toCsv([['plain-value']])).toBe('﻿plain-value');
  });

  test('guards a field that would open as a formula, one apostrophe per trigger', () => {
    // =, +, -, @ all launch a formula in Excel/Sheets. A hostname or
    // agent_meta value under attacker control could contain any of these.
    for (const payload of ['=cmd|\'/c calc\'!A1', '+1+1', '-1+1', '@evil', '\tevil']) {
      expect(toCsv([[payload]])).toBe(`﻿'${payload}`);
    }
  });

  test('guards a field starting with \\r, quoted because \\r also needs quoting', () => {
    // \r can feed a DDE handler, same as the triggers above — but it is
    // ALSO one of the three characters that force quoting on its own, so
    // the guarded field is both apostrophe-prefixed and quoted.
    expect(toCsv([['\revil']])).toBe('﻿"\'\revil"');
  });

  test('does not guard a value that merely contains a trigger character mid-string', () => {
    expect(toCsv([['total=5']])).toBe('﻿total=5');
  });

  test('quotes a guarded field that also needs quoting', () => {
    expect(toCsv([['=a,b']])).toBe('﻿"\'=a,b"');
  });
});
