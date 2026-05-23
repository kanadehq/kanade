import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

import App from './App.tsx';
// Side-effect import: configures i18next + LanguageDetector before
// any component renders, so `useTranslation()` always finds a ready
// instance.
import './i18n';
import './index.css';

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
