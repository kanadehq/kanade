// Playwright Component Testing mount host.
//
// Both imports are side effects the mounted components rely on:
//   - `./index.css` pulls in Tailwind (via the app's `src/index.css`) and
//     pins the `@source` scan to the component tree. Hit-testing is
//     meaningless without the real stylesheet: the whole point is that
//     `pointer-events-none` (on the gridlines and crosshair) actually
//     applies, so those non-tooltip overlays stay transparent to
//     `elementFromPoint`. Without it every class is inert and the test would
//     assert against a layout the browser never ships.
//   - `i18n` initialises react-i18next so `useTranslation` resolves real copy
//     instead of raw keys. Not load-bearing for hit-testing (a key is still a
//     non-empty `title`), but it keeps the mounted tree identical to prod and
//     silences the "no i18n instance" warning.
import './index.css';
import '../src/i18n';
