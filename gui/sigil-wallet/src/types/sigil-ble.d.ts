// The framework-agnostic Bluetooth engine served at /sigil-ble.js (source: gui/sigil-ble.js).
// It mounts its own modal; a React host sets SIGIL_BLE_NO_FAB to suppress the floating
// launcher and drives it from its own Settings button.
export {};
declare global {
  interface Window {
    SIGIL_BLE_NO_FAB?: boolean;
    SigilBLE?: { open: () => void; available?: boolean };
  }
}
