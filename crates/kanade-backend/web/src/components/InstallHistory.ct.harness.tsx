import { useEffect } from 'react';
import i18n from '@/i18n';
import { Toaster } from 'sonner';
import { InstallHistory } from './InstallHistory';

export function InstallHistoryHost({ language = 'en' }: { language?: string }) {
  useEffect(() => { void i18n.changeLanguage(language); }, [language]);
  return <div className="p-4"><InstallHistory /><Toaster /></div>;
}
