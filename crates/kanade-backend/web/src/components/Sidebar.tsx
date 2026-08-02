import { useQuery } from '@tanstack/react-query';
import { Menu, X } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, NavLink, useLocation } from 'react-router-dom';

import { AuthBar } from '@/components/AuthBar';
import { apiFetch } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import type { Feature } from '@/lib/features';
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
// `feature` (when present) is the #1008 page-visibility key: the link is
// hidden unless the caller's allow-list permits it (`canSee`). Links without
// a `feature` are baseline/commons — shown to every UNRESTRICTED account, but
// hidden from restricted ones (the backend 403s them on the commons APIs, so
// the link would dead-end). The keys match `lib/features`.
// Exported so ProtectedLayout can derive a restricted account's landing page
// (first link, in this order, whose feature passes `canSee`).
export const groups: {
  labelKey: string;
  accent: string;
  links: { to: string; labelKey: string; adminOnly?: boolean; feature?: Feature }[];
}[] = [
  {
    labelKey: 'nav.groups.execute',
    accent: 'text-violet-light',
    links: [
      { to: '/run', labelKey: 'nav.run', feature: 'run' },
      { to: '/exec', labelKey: 'nav.exec', feature: 'exec' },
    ],
  },
  {
    labelKey: 'nav.groups.observe',
    accent: 'text-amber-light',
    links: [
      { to: '/agents', labelKey: 'nav.agents' },
      // Fleet search lives as a tab *inside* the Inventory page
      // (/inventory/search), not a sibling nav row — it's a sub-view
      // of inventory, not a peer of it. Keeping it off the sidebar
      // avoids a lone nested entry that would read as inconsistent
      // next to the otherwise-flat list.
      { to: '/inventory', labelKey: 'nav.inventory', feature: 'inventory' },
      { to: '/compliance', labelKey: 'nav.compliance', feature: 'compliance' },
      { to: '/activity', labelKey: 'nav.activity', feature: 'activity' },
      { to: '/events', labelKey: 'nav.events', feature: 'events' },
      { to: '/audit', labelKey: 'nav.audit', feature: 'audit' },
      { to: '/logs', labelKey: 'nav.logs', feature: 'logs' },
      { to: '/collect', labelKey: 'nav.collect', feature: 'collect' },
      { to: '/analytics', labelKey: 'nav.analytics', feature: 'analytics' },
    ],
  },
  {
    labelKey: 'nav.groups.manage',
    accent: 'text-teal-light',
    links: [
      { to: '/jobs', labelKey: 'nav.jobs', feature: 'jobs' },
      { to: '/schedules', labelKey: 'nav.schedules', feature: 'schedules' },
      { to: '/views', labelKey: 'nav.views', feature: 'views' },
      { to: '/notifications', labelKey: 'nav.notifications', feature: 'notifications' },
      { to: '/rollout', labelKey: 'nav.rollout', feature: 'rollout' },
      { to: '/agent-install', labelKey: 'nav.agentInstall', feature: 'agent-install' },
      { to: '/apps', labelKey: 'nav.apps', feature: 'apps' },
      // `nav.agentGroups` (not `nav.groups`) — that key is already
      // taken by the sidebar section labels above.
      { to: '/groups', labelKey: 'nav.agentGroups', feature: 'groups' },
      { to: '/config', labelKey: 'nav.config', feature: 'config' },
      { to: '/jetstream', labelKey: 'nav.jetstream', feature: 'jetstream' },
      { to: '/accounts', labelKey: 'nav.accounts', adminOnly: true, feature: 'accounts' },
      { to: '/settings', labelKey: 'nav.settings', feature: 'settings' },
    ],
  },
];

// Backend build version, from the public `GET /api/version`. Shown dim in
// the sidebar footer so operators can confirm what's actually deployed
// (e.g. after a fleet-deploy) without leaving the SPA. Cached for the
// session — the version only changes on a backend restart.
function BackendVersion() {
  const { t } = useTranslation('common');
  const { data } = useQuery({
    queryKey: ['backend-version'],
    queryFn: () => apiFetch<{ version: string }>('/api/version'),
    staleTime: Infinity,
    // Cosmetic only — don't hammer the backend if it's mid-restart/offline.
    retry: false,
  });
  if (!data?.version) return null;
  return (
    <p className="px-2 pt-2 text-[10px] text-muted/70" title={t('backendVersionTitle')}>
      {t('backendVersion', { version: data.version })}
    </p>
  );
}

// The brand mark (baton + wordmark), shared by the desktop sidebar and the
// mobile top bar so the two never drift.
function LogoMark({ imgClass, textClass }: { imgClass: string; textClass: string }) {
  return (
    <>
      {/* Baton-only crop of the canonical mark (assets/icon.svg).
          Dark variant swaps via <picture> + prefers-color-scheme.
          The 奏 kanji lives in the title text below, not the icon. */}
      <picture>
        <source media="(prefers-color-scheme: dark)" srcSet="/icon-dark.svg" />
        <img src="/icon.svg" alt="kanade baton" className={imgClass} />
      </picture>
      <h1
        className={cn(
          'font-extrabold bg-gradient-to-br from-violet via-amber to-teal bg-clip-text text-transparent',
          textClass,
        )}
      >
        奏 kanade
      </h1>
    </>
  );
}

function SidebarBody({ onNavigate }: { onNavigate?: () => void }) {
  const { t } = useTranslation('common');
  const { hasRole, canSee, isRestricted } = useAuth();
  // Resolve each group's visible links once so a group whose links are all
  // filtered out (by role or page-visibility) doesn't render an empty header
  // + divider.
  const visibleGroups = groups
    .map((g) => ({
      ...g,
      links: g.links.filter(
        (l) =>
          (!l.adminOnly || hasRole('admin')) && (l.feature ? canSee(l.feature) : !isRestricted),
      ),
    }))
    .filter((g) => g.links.length > 0);
  return (
    <>
      {/* The logo links to the dashboard for unrestricted accounts only —
          a restricted account (e.g. the download-only installer user) is
          403'd on the commons APIs, so its logo stays an unlinked mark. */}
      {isRestricted ? (
        <span className="flex items-center gap-2 px-4 py-4">
          <LogoMark imgClass="h-7 w-auto" textClass="text-xl" />
        </span>
      ) : (
        <Link to="/" onClick={onNavigate} className="flex items-center gap-2 px-4 py-4 group">
          <LogoMark
            imgClass="h-7 w-auto transition-transform group-hover:rotate-3"
            textClass="text-xl"
          />
        </Link>
      )}

      <nav className="flex-1 overflow-y-auto px-2 pb-3">
        {visibleGroups.map((g, idx) => (
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
        <BackendVersion />
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
  const { isRestricted } = useAuth();

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
          {/* Same restricted-account unlinking as the desktop logo. */}
          {isRestricted ? (
            <span className="flex items-center gap-2">
              <LogoMark imgClass="h-6 w-auto" textClass="text-lg" />
            </span>
          ) : (
            <Link to="/" className="flex items-center gap-2">
              <LogoMark imgClass="h-6 w-auto" textClass="text-lg" />
            </Link>
          )}
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
