# nemesis8 v0.25.2 — Keys in plain sight 🔑

Starting a Hermes backend now tells you which LLM keys it will have, which it could have, and the one command to add one. And OpenRouter — Hermes's most common cloud provider — can finally be keyed at all. To get it: `n8 update`.

## What keys does the backend have?

`n8 --provider hermes serve-backend` prints this before it launches anything, so it shows even if the launch fails:

```
LLM keys for hermes — forwarded into the container from `n8 secrets` (or your env):
  set:      ANTHROPIC_API_KEY
  not set:  OPENROUTER_API_KEY, OPENAI_API_KEY, XAI_API_KEY, GEMINI_API_KEY, GOOGLE_API_KEY, MINIMAX_API_KEY, KIMI_API_KEY, ZAI_API_KEY
  add one:  n8 secrets set OPENROUTER_API_KEY    (keys are injected at launch — restart this backend after)
```

It resolves the keys exactly the way n8 forwards them into the container — your OS keychain first (`n8 secrets set <NAME>`), then the host environment — and prints names only, never values. The flow to give Hermes a new provider is what the output says: set the key, restart the backend, and the `set:` line confirms it. Providers that declare no keys print nothing.

## OpenRouter and friends

n8 forwards into a container the union of every provider's declared key chain plus a fixed list. Nobody had declared `OPENROUTER_API_KEY`, so `n8 secrets set OPENROUTER_API_KEY` was accepted and then silently ignored at launch. Hermes now declares the keys it reads natively — OpenRouter, OpenAI, Anthropic, xAI, Gemini, Google, MiniMax, Kimi, Z.AI — so each one you set is forwarded. Nothing is copied into another variable name; Hermes reads each as-is. None is required: local Ollama needs no key, and `hermes login` still covers OAuth providers.

Coming from further back? [v0.25.1](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.25.1) was the previous published build.
