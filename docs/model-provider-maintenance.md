# Model Provider Maintenance

This page records official provider documentation links for future model preset updates.

## Xiaomi MiMo

Project preset location:

- `crates/mai-store/src/lib.rs`
- Built-in provider setup: `mimo_builtin_provider`
- Model limits: `mimo_context_tokens` and `mimo_output_tokens`

Official documentation links:

- Documentation index: <https://platform.xiaomimimo.com/llms.txt>
- Pricing, rate limits, context length, and maximum output length: <https://platform.xiaomimimo.com/static/docs/pricing.md>
- Browser pricing page: <https://platform.xiaomimimo.com/docs/zh-CN/pricing>
- Model hyperparameters such as `temperature` and `top_p`: <https://platform.xiaomimimo.com/static/docs/quick-start/model-hyperparameters.md>
- Model release and update log: <https://platform.xiaomimimo.com/static/docs/updates/model.md>
- OpenAI-compatible chat API reference: <https://platform.xiaomimimo.com/static/docs/api/chat/openai-api.md>

Update checklist:

- Check pricing/details for `Context Length` and `Maximum Output Length`.
- Check hyperparameters for default/range changes if model options are added later.
- Check the release log for renamed models, newly released models, or unchanged API names after backend upgrades.
- Update built-in presets only unless an explicit migration is requested; saved `config.toml` providers are user configuration.
- Add or update preset tests in `provider_presets_include_builtin_metadata`.

Last checked: 2026-05-12.
