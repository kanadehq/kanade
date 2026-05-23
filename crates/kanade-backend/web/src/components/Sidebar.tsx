import { Menu, X } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, NavLink, useLocation } from 'react-router-dom';

import { AuthBar } from '@/components/AuthBar';
import { cn } from '@/lib/utils';

// Sidebar groups: same three semantic clusters introduced in #52,
// but rendered vertically so the at-a-glance scan target is one
// short column instead of a row of 13+ entries crammed across the
// header. Order within each group is the existing one. Each group
// carries an accent class so the section label borrows one of the
// kanade brand colours (violet → amber → teal, same gradient as the
// logo) and reads visually distinct from the link rows below it.
// Labels are translation keys (resolved at render time); the
// structural metadata (path, accent) stays in code.
const groups: {
  labelKey: string;
  accent: string;
  links: { to: string; labelKey: string }[];
}[] = [
  {
    labelKey: 'nav.groups.execute',
    accent: 'text-violet-light',
    links: [
      { to: '/run', labelKey: 'nav.run' },
      { to: '/exec', labelKey: 'nav.exec' },
    ],
  },
  {
    labelKey: 'nav.groups.observe',
    accent: 'text-amber-light',
    links: [
      { to: '/agents', labelKey: 'nav.agents' },
      { to: '/inventory', labelKey: 'nav.inventory' },
      { to: '/inventory/search', labelKey: 'nav.search' },
      { to: '/activity', labelKey: 'nav.activity' },
      { to: '/audit', labelKey: 'nav.audit' },
      { to: '/logs', labelKey: 'nav.logs' },
    ],
  },
  {
    labelKey: 'nav.groups.manage',
    accent: 'text-teal-light',
    links: [
      { to: '/jobs', labelKey: 'nav.jobs' },
      { to: '/schedules', labelKey: 'nav.schedules' },
      { to: '/rollout', labelKey: 'nav.rollout' },
      { to: '/config', labelKey: 'nav.config' },
      { to: '/jetstream', labelKey: 'nav.jetstream' },
      { to: '/settings', labelKey: 'nav.settings' },
    ],
  },
];

function SidebarBody({ onNavigate }: { onNavigate?: () => void }) {
  const { t } = useTranslation('common');
  return (
    <>
      <Link
        to="/"
        onClick={onNavigate}
        className="flex items-center gap-2 px-4 py-4 group"
      >
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

      <nav className="flex-1 overflow-y-auto px-2 pb-3">
        {groups.map((g, idx) => (
          <div
            key={g.labelKey}
            // Top border + padding between groups (except for the
            // first one) separates the three sections visually so
            // the operator's eye can chunk them at a glance instead
            // of reading 13 rows linearly.
            className={cn(
              'flex flex-col',
              idx === 0 ? 'mt-1' : 'mt-3 pt-3 border-t border-border/60',
            )}
          >
            {/* h3 (not div) so screen readers can navigate the
                sidebar by section heading instead of having to crawl
                every link. The visual chrome is unchanged. */}
            <h3
              className={cn(
                'px-3 pb-1 text-[10px] font-semibold uppercase tracking-wider',
                g.accent,
              )}
            >
              {t(g.labelKey)}
            </h3>
            {g.links.map((l) => (
              <NavLink
                key={l.to}
                to={l.to}
                onClick={onNavigate}
                // pl-5 indents the link a notch to the right of the
                // header so the parent–child relationship reads
                // without anyone having to think about it.
                className={({ isActive }) =>
                  cn(
                    'pl-5 pr-3 py-1.5 rounded-md text-sm font-medium transition-colors',
                    isActive
                      ? 'bg-muted/15 text-fg'
                      : 'text-muted hover:text-fg hover:bg-muted/10',
                  )
                }
              >
                {t(l.labelKey)}
              </NavLink>
            ))}
          </div>
        ))}
      </nav>

      <div className="px-3 py-3 border-t border-border">
        <AuthBar />
      </div>
    </>
  );
}

// Single shared definition of the sidebar's visual width keeps the
// fixed-position aside and the mobile drawer in sync. Bumping this
// value is one place — but Tailwind's JIT requires literal class
// names, so the matching `md:pl-56` on the layout wrapper has to be
// updated by hand. See ProtectedLayout.tsx; the two must stay in
// lockstep.
const SIDEBAR_WIDTH_CLASS = 'w-56';

export function Sidebar() {
  const [open, setOpen] = useState(false);
  const location = useLocation();
  const { t } = useTranslation('common');

  // Close the mobile drawer when the route changes — clicking a
  // link inside the drawer fires the NavLink before this effect, so
  // we react to location changes rather than relying on each
  // NavLink's onClick. Without this an inbound /search?q= query
  // change wouldn't dismiss the drawer either.
  useEffect(() => {
    setOpen(false);
  }, [location.pathname]);

  // Block body scroll when the mobile drawer is open so the
  // underlying page doesn't scroll behind the overlay.
  useEffect(() => {
    if (open) {
      const prev = document.body.style.overflow;
      document.body.style.overflow = 'hidden';
      return () => {
        document.body.style.overflow = prev;
      };
    }
  }, [open]);

  return (
    <>
      {/* Mobile top bar — slim row above the page that exposes the
          hamburger. Auth lives in the sidebar body so it isn't
          duplicated here. */}
      <header className="md:hidden sticky top-0 z-20 bg-card/80 backdrop-blur-sm border-b border-border">
        <div className="flex items-center gap-3 px-4 py-3">
          <button
            type="button"
            aria-label={t('menu.open')}
            aria-expanded={open}
            onClick={() => setOpen(true)}
            className="inline-flex items-center justify-center h-9 w-9 rounded-md text-muted hover:text-fg hover:bg-muted/10"
          >
            <Menu className="size-5" />
          </button>
          <Link to="/" className="flex items-center gap-2">
            <picture>
              <source media="(prefers-color-scheme: dark)" srcSet="/icon-dark.svg" />
              <img src="/icon.svg" alt="kanade baton" className="h-6 w-auto" />
            </picture>
            <h1 className="text-lg font-extrabold bg-gradient-to-br from-violet via-amber to-teal bg-clip-text text-transparent">
              奏 kanade
            </h1>
          </Link>
        </div>
      </header>

      {/* Desktop sidebar — fixed to the left edge, full height. The
          page content gets a matching left margin (set on the layout
          wrapper) so it doesn't render under the aside. */}
      <aside
        className={cn(
          'hidden md:flex md:flex-col md:fixed md:inset-y-0 md:left-0 md:z-10 border-r border-border bg-card/60 backdrop-blur-sm',
          SIDEBAR_WIDTH_CLASS,
        )}
      >
        <SidebarBody />
      </aside>

      {/* Mobile drawer — full-height slide-in from the left when the
          hamburger is tapped. Backdrop closes the drawer; route
          changes close it too (see effect above). */}
      {open && (
        <div className="md:hidden fixed inset-0 z-30 flex">
          <div
            className="absolute inset-0 bg-bg/70 backdrop-blur-sm"
            onClick={() => setOpen(false)}
            aria-hidden
          />
          <aside
            className={cn(
              'relative flex flex-col h-full bg-card border-r border-border',
              SIDEBAR_WIDTH_CLASS,
            )}
          >
            <button
              type="button"
              aria-label={t('menu.close')}
              onClick={() => setOpen(false)}
              className="absolute top-3 right-3 inline-flex items-center justify-center h-8 w-8 rounded-md text-muted hover:text-fg hover:bg-muted/10"
            >
              <X className="size-4" />
            </button>
            <SidebarBody onNavigate={() => setOpen(false)} />
          </aside>
        </div>
      )}
    </>
  );
}
