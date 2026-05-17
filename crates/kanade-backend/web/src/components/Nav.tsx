import { Link, NavLink } from 'react-router-dom';

import { AuthBar } from '@/components/AuthBar';
import { cn } from '@/lib/utils';

const links = [
  { to: '/agents', label: 'Agents' },
  { to: '/inventory', label: 'Inventory' },
  { to: '/run', label: 'Run' },
  { to: '/results', label: 'Results' },
  { to: '/audit', label: 'Audit' },
  { to: '/logs', label: 'Logs' },
  { to: '/schedules', label: 'Schedules' },
  { to: '/exec', label: 'Exec' },
  { to: '/rollout', label: 'Rollout' },
  { to: '/config', label: 'Config' },
  { to: '/jetstream', label: 'JetStream' },
];

export function Nav() {
  return (
    <header className="border-b border-border bg-card/60 backdrop-blur-sm sticky top-0 z-10">
      <div className="max-w-7xl mx-auto px-6 py-3 flex items-center gap-6">
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
        <nav className="flex gap-1">
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
        <div className="ml-auto">
          <AuthBar />
        </div>
      </div>
    </header>
  );
}
