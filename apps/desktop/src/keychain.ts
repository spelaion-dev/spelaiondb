// Whether the system has a keychain to save passwords in. On Linux it needs a Secret
// Service (GNOME Keyring, KWallet, KeePassXC…) running; without one, "Save password" cannot
// be offered and passwords are asked when connecting.

import { useEffect, useState } from "react";
import { commands } from "./api";

/** `null` while it is asked. Asked each time a form opens: a Secret Service started in the
 *  meantime is found without restarting the app. */
export function useKeychainAvailable(): boolean | null {
  const [available, setAvailable] = useState<boolean | null>(null);
  useEffect(() => {
    let live = true;
    void commands.keychainAvailable().then(
      (value) => live && setAvailable(value),
      // Unknown: the form behaves as before, and a failure is reported when saving.
      () => live && setAvailable(true),
    );
    return () => {
      live = false;
    };
  }, []);
  return available;
}
