# Settings

The gear button at the bottom of the sidebar (or `Ctrl+,`) opens **Settings** in its own tab:

- **Appearance:** light, dark or the system theme.
- **Language:** English, Spanish or the system language. Translations are bundled with the
  application and work offline. Database and SSH server messages are shown as received.
- **History:** what query history stores and where to turn it off.
- **Editor:** whether restored tabs connect again at startup (off by default).

Preferences are stored in the personal `state.db` and apply to every workspace.

## Automatic updates

Automatic updates (ADR-021, from 0.4.0): the app checks for a new version when it starts
(you can turn that off in Settings → Updates) and shows a notice. The signed update is
downloaded and installed only when you click "Install and restart". Versions before 0.4.0
cannot update themselves: install 0.4.0 by hand once.

Use "Release notes…" in the notice or Settings → Updates to read what changed before
installing. Links in the notes are displayed as text; they do not open automatically.
