import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';

import { Inventory } from './Inventory';

function Location() {
  const location = useLocation();
  return <output data-testid="location">{location.pathname}{location.search}</output>;
}

export function InventoryRecoveryHarness({ initial }: { initial: string }) {
  return (
    <MemoryRouter initialEntries={[initial]}>
      <Location />
      <Routes>
        <Route path="/inventory/*" element={<Inventory />} />
      </Routes>
    </MemoryRouter>
  );
}
