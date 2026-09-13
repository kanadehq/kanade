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
});
