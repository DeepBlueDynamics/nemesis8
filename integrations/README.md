# Agent integrations

Provider-owned integrations are baked into `/opt/defaults/integrations` and installed into the provider config home at session startup through `provider.hooks.bundled_config_dirs`.

`hermes/nemesis8` is a native Hermes plugin for the n8 gateway. Its provider defaults enable it automatically while retaining other enabled plugins and honoring an explicit Hermes plugin deny-list.
