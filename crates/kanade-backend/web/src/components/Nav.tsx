import { Menu, X } from 'lucide-react';
import { useState } from 'react';
import { Link, NavLink } from 'react-router-dom';

import { AuthBar } from '@/components/AuthBar';
import { cn } from '@/lib/utils';

// Sidebar is grouped into three semantic sections so operators can
// scan by intent instead of memorising 13 flat entries: Execute (fire
// something), Observe (read fleet state), Manage (CRUD metadata).
// Within each group the order is the existing one — purely a visual
// regrouping, no URL changes.
const groups: { label: string; links: { to: string; label: string }[] }[] = [
  {
    label: 'Execute',
    links: [
      { to: '/run', label: 'Run' },
      { to: '/exec', label: 'Exec' },
    ],
  },
  {
    label: 'Observe',
    links: [
      { to: '/agents', label: 'Agents' },
      { to: '/inventory', label: 'Inventory' },
      { to: '/inventory/search', label: 'Search' },
      { to: '/activity', label: 'Activity' },
      { to: '/audit', label: 'Audit' },
      { to: '/logs', label: 'Logs' },
    ],
  },
  {
    label: 'Manage',
    links: [
      { to: '/jobs', label: 'Jobs' },
      { to: '/schedules', label: 'Schedules' },
      { to: '/rollout', label: 'Rollout' },
      { to: '/config', label: 'Config' },
      { to: '/jetstream', label: 'JetStream' },
    ],
  },
];

export function Nav() {
  const [open, setOpen] = useState(false);

  return (
    <header className="border-b border-border bg-card/60 backdrop-blur-sm sticky top-0 z-10">
      <div className="max-w-screen-2xl mx-auto px-4 py-3 flex items-center gap-6">
        <Link to="/" className="flex items-center gap-2 group">
          {/* Baton-only crop of the canonical mark (assets/icon.svg).
              Dark variant swaps via <picture> + prefers-color-scheme.
              The 奏 kanji lives in the title text below, not the icon. */}
          <picture>
            <source media="(prefers-color-scheme: dark)" srcSet="/icon-dark.svg" />
            <img
              src="/icon.svg"
              alt="kanade baton"
              className="h-7 w-auto transition-transform group-hover:rotate-3"
            />
          </picture>
          <h1 className="text-xl font-extrabold bg-gradient-to-br from-violet via-amber to-teal bg-clip-text text-transparent">
            奏 kanade
          </h1>
        </Link>

        {/* Desktop nav — visible from md upward; wraps when crowded so
            it can keep up with new entries without truncation. Each
            group renders as an inline cluster: small uppercase label
            then its links, separated by a wider gap between clusters
            so the grouping reads even when the row wraps. */}
        <nav className="hidden md:flex gap-5 flex-wrap items-center">
          {groups.map((g) => (
            <div key={g.label} className="flex items-center gap-1">
              <span className="text-muted text-[10px] font-semibold uppercase tracking-wider mr-1">
                {g.label}
              </span>
              {g.links.map((l) => (
                <NavLink
                  key={l.to}
                  to={l.to}
                  className={({ isActive }) =>
                    cn(
                      'px-3 py-1.5 rounded-md text-sm font-medium transition-colors',
                      isActive ? 'bg-muted/10 text-fg' : 'text-muted hover:text-fg',
                    )
                  }
                >
                  {l.label}
                </NavLink>
              ))}
            </div>
          ))}
        </nav>

        <div className="ml-auto flex items-center gap-2">
          <AuthBar />
          {/* Hamburger — mobile only. Toggles the dropdown drawer
              rendered below this row. */}
          <button
            type="button"
            aria-label={open ? 'Close menu' : 'Open menu'}
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
            className="md:hidden inline-flex items-center justify-center h-9 w-9 rounded-md text-muted hover:text-fg hover:bg-muted/10"
          >
            {open ? <X className="size-5" /> : <Menu className="size-5" />}
          </button>
        </div>
      </div>

      {/* Mobile drawer — full-width dropdown directly under the header
          row. Tapping a link auto-closes via onClick (NavLink fires
          before the route change).

          max-h + overflow-y-auto guards against the link list overflow­
          ing the viewport on short screens (the header is sticky, so
          without the cap the bottom of the list ends up unreachable).
          The cap is "screen height minus a header's worth" — picked
          large rather than tight so tall phones use it fully. */}
      {open && (
        <nav className="md:hidden border-t border-border bg-card/95 backdrop-blur-sm max-h-[calc(100vh-4rem)] overflow-y-auto">
          <div className="max-w-screen-2xl mx-auto px-4 py-2 flex flex-col">
            {groups.map((g) => (
              <div key={g.label} className="flex flex-col">
                <span className="px-3 pt-3 pb-1 text-muted text-[10px] font-semibold uppercase tracking-wider">
                  {g.label}
                </span>
                {g.links.map((l) => (
                  <NavLink
                    key={l.to}
                    to={l.to}
                    onClick={() => setOpen(false)}
                    // Asymmetric padding indents the links under the
                    // uppercase group header so the hierarchy reads
                    // even before the operator hovers anything.
                    className={({ isActive }) =>
                      cn(
                        'pl-6 pr-3 py-2 rounded-md text-sm font-medium transition-colors',
                        isActive ? 'bg-muted/10 text-fg' : 'text-muted hover:text-fg',
                      )
                    }
                  >
                    {l.label}
                  </NavLink>
                ))}
              </div>
            ))}
          </div>
        </nav>
      )}
    </header>
  );
}
