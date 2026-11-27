# nemesis8 v0.29.8 — New session lists that scroll in short terminals ↕️

The New session form's lists now scroll, so every provider and model can be reached in a short terminal. Get it: `n8 update`. This is a host-only change: no `n8 build`, and running containers aren't affected.

## What changed

- **The New session lists scroll** (#188). In a short terminal, the Provider, Model, Host and App lists in the home screen's New session form were cut off at the bottom of the window. Providers past the first few never showed, but ↑↓ still moved onto them, so you picked blind. The lists now scroll to keep the highlighted item in view, with ▲ and ▼ on the edge when there's more above or below. The mouse wheel moves the highlight in an open list, and a click picks the row you see. Clicking below a list no longer picks a hidden item.
- **The home screen no longer crashes when you open the Model list in a very short terminal** (#188). At about 8 rows, the list could draw past the bottom of the window and crash the home screen.

Coming from further back? [v0.29.7](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.7) was the previous published build (Ollama cloud models in opencode, telemetry that reaches lume).
