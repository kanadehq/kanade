import { LogIn } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { useAuth } from '@/lib/auth';

type LocationState = { from?: { pathname?: string } };

export function Login() {
  const { setToken, isAuthenticated } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const [draft, setDraft] = useState('');

  const from = (location.state as LocationState | null)?.from?.pathname ?? '/dashboard';

  // If the user lands here while already authenticated (e.g. typed
  // /login by hand), kick them to the dashboard instead of showing
  // the form. This also catches the post-setToken render.
  useEffect(() => {
    if (isAuthenticated) {
      navigate(from, { replace: true });
    }
  }, [isAuthenticated, from, navigate]);

  return (
    <main className="min-h-screen flex items-center justify-center px-4 py-8 bg-bg">
      <Card className="w-full max-w-md">
        <CardHeader className="text-center">
          <div className="flex justify-center mb-3">
            <picture>
              <source media="(prefers-color-scheme: dark)" srcSet="/icon-dark.svg" />
              <img src="/icon.svg" alt="kanade" className="h-12 w-auto" />
            </picture>
          </div>
          <CardTitle className="text-2xl">
            <span className="bg-gradient-to-br from-violet via-amber to-teal bg-clip-text text-transparent">
              奏 kanade
            </span>
          </CardTitle>
          <CardDescription>Sign in with your bearer token</CardDescription>
        </CardHeader>
        <CardContent>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              const t = draft.trim();
              if (!t) return;
              setToken(t);
              navigate(from, { replace: true });
            }}
            className="space-y-4"
          >
            <div className="space-y-1">
              <Label htmlFor="login-token">bearer token</Label>
              <Input
                id="login-token"
                type="password"
                autoFocus
                autoComplete="off"
                placeholder="paste token here"
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
              />
            </div>
            <Button type="submit" disabled={!draft.trim()} className="w-full">
              <LogIn className="size-4 mr-2" />
              Sign in
            </Button>
          </form>
          <p className="mt-4 text-xs text-muted">
            The token matches whatever the backend was deployed with —
            either <code>StaticToken</code> (passed to <code>deploy-backend.ps1</code>) or
            a signed JWT (<code>aud=kanade</code>). Stored in <code>localStorage</code>;
            log out from the nav to clear.
          </p>
        </CardContent>
      </Card>
    </main>
  );
}
