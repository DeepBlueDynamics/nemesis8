# nemesis8 v0.29.8 — Keyboard menus and New session lists that scroll ⌨️

You can now open the home screen's menus from the keyboard, and the New session lists scroll so every provider and model can be reached in a short terminal. Get it: `n8 update`. This is a host-only change: no `n8 build`, and running containers aren't affected.

## What changed

- **Open the menus from the keyboard** (#188). Press `m` or F10 to open the menu bar, ←→ to move between menus, ↑↓ to pick an item, and Enter to run it. Esc, `m` or F10 closes it. Alt plus a menu's first letter (S, C, T, H) opens that menu directly, and that letter is now underlined in the menu bar. Before, the only hint was "Alt+S/H", and in a terminal that swallows Alt the mouse was the only way in. Help ▸ Keys lists all of this.
- **The New session lists scroll** (#188). In a short terminal, the Provider, Model, Host and App lists in the home screen's New session form were cut off at the bottom of the window. Providers past the first few never showed, but ↑↓ still moved onto them, so you picked blind. The lists now scroll to keep the highlighted item in view, with ▲ and ▼ on the edge when there's more above or below. The mouse wheel moves the highlight in an open list, and a click picks the row you see. Clicking below a list no longer picks a hidden item.
- **The home screen no longer crashes when you open the Model list in a very short terminal** (#188). At about 8 rows, the list could draw past the bottom of the window and crash the home screen.
- **Troubleshoot no longer offers "Antigravity: wipe stale config"** (#188). It cleared antigravity config left over from the retired gnosis tools, which are no longer in the image. Troubleshoot is now Doctor, Wipe image, and Refresh gateway status.

Coming from further back? [v0.29.7](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.7) was the previous published build (Ollama cloud models in opencode, telemetry that reaches lume).
