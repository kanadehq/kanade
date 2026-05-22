import { Menu, X } from 'lucide-react';
import { useState } from 'react';
import { Link, NavLink } from 'react-router-dom';

import { AuthBar } from '@/components/AuthBar';
import { cn } from '@/lib/utils';

const links = [
  { to: '/agents', label: 'Agents' },
  { to: '/inventory', label: 'Inventory' },
  { to: '/software', label: 'Software' },
  { to: '/run', label: 'Run' },
  { to: '/activity', label: 'Activity' },
  { to: '/audit', label: 'Audit' },
  { to: '/logs', label: 'Logs' },
  { to: '/jobs', label: 'Jobs' },
  { to: '/schedules', label: 'Schedules' },
  { to: '/exec', label: 'Exec' },
  { to: '/rollout', label: 'Rollout' },
  { to: '/config', label: 'Config' },
  { to: '/jetstream', label: 'JetStream' },
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
            it can keep up with new entries without truncation. */}
        <nav className="hidden md:flex gap-1 flex-wrap">
          {links.map((l) => (
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
          before the route change). */}
      {open && (
        <nav className="md:hidden border-t border-border bg-card/95 backdrop-blur-sm">
          <div className="max-w-screen-2xl mx-auto px-4 py-2 flex flex-col">
            {links.map((l) => (
              <NavLink
                key={l.to}
                to={l.to}
                onClick={() => setOpen(false)}
                className={({ isActive }) =>
                  cn(
                    'px-3 py-2 rounded-md text-sm font-medium transition-colors',
                    isActive ? 'bg-muted/10 text-fg' : 'text-muted hover:text-fg',
                  )
                }
              >
                {l.label}
              </NavLink>
            ))}
          </div>
        </nav>
      )}
    </header>
  );
}
