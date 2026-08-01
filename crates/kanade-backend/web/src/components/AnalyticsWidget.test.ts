import { describe, expect, it } from 'bun:test';

import { widgetSpanClass } from './AnalyticsWidget';

const WIDE = 'lg:col-span-2';
const NARROW = '';

describe('widgetSpanClass', () => {
  describe('with no width hint (every manifest written before #1257)', () => {
    it('keeps the wide renders wide', () => {
      for (const render of ['bar', 'timeline', 'op_timeline', 'table']) {
        expect(widgetSpanClass(render, undefined)).toBe(WIDE);
      }
    });

    it('keeps the compact renders in one column', () => {
      for (const render of ['gauge', 'stat', 'pie']) {
        expect(widgetSpanClass(render, undefined)).toBe(NARROW);
      }
    });
  });

  describe('with an explicit hint', () => {
    // The whole point of #1257: two pinned bars that answer the same
    // question should be able to share a row.
    it('narrows a bar that asks to be half', () => {
      expect(widgetSpanClass('bar', 'half')).toBe(NARROW);
    });

    // The hint has to work in both directions, or it is just a
    // "no-really-make-this-small" flag rather than a width control.
    it('widens a gauge that asks to be full', () => {
      expect(widgetSpanClass('gauge', 'full')).toBe(WIDE);
    });

    it('narrows every wide render on request', () => {
      for (const render of ['bar', 'timeline', 'op_timeline', 'table']) {
        expect(widgetSpanClass(render, 'half')).toBe(NARROW);
      }
    });
  });

  describe('forward compatibility (#492)', () => {
    // An older SPA meeting a manifest that uses a width variant added
    // later must fall back to the default layout, NOT drop or mis-size
    // the widget: a presentation hint is never worth losing content over.
    it('treats the unknown catch-all as unset', () => {
      expect(widgetSpanClass('bar', 'unknown')).toBe(WIDE);
      expect(widgetSpanClass('gauge', 'unknown')).toBe(NARROW);
    });

    // Same reasoning one level down: a render this build doesn't know
    // gets the compact default rather than an exception.
    it('gives an unknown render the narrow default', () => {
      expect(widgetSpanClass('sankey', undefined)).toBe(NARROW);
    });

    it('still honours an explicit hint on an unknown render', () => {
      expect(widgetSpanClass('sankey', 'full')).toBe(WIDE);
    });
  });
});
