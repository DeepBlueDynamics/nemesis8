# nemesis8 v0.26.3 — Pinned means pinned 📌

`n8 build` failed on every machine as of 2026-09-26: the Hermes provider's installer script, fetched from Hermes's `main` branch, was rewritten upstream and no longer accepts the `--force-commit` flag n8 passes, so the provider-install step died with "unknown option: --force-commit". This release fetches the installer from the same pinned Hermes commit as the code it installs, so upstream changes to the script can't break the image again. Nothing else changed. To get it: `n8 update`, then `n8 build`.

Coming from further back? [v0.26.2](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.2) was the previous published build.
