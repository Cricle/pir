# Custom providers

pir speaks every upstream protocol adapter natively (see [models.md](models.md) for `api`). A "custom provider" is
a models.json entry whose `baseUrl` points at your gateway — no code needed.

## OpenAI-compat auto-detect

An unknown endpoint can omit `api`: pir probes it and falls back to `openai-compat`, applying the upstream compat
fixes (stream options, tool-call shapes) when the probe reports them.

## Virtual models (registerVirtualModel)

JS extensions can register a virtual model that pir routes per-request:

```js
export default function (pi) {
  pi.registerVirtualModel({
    name: "my-router",
    route: () => ({ provider: "my-gateway", id: "my-model" }),
    // or full streaming control:
    // streamSimple: async (context, options) => { ... yield events ... }
  });
}
```

`streamSimple` receives `context` (messages, tools) and `options` (signal, apiKey) and yields the assistant event
shapes builtin adapters emit (`start`, `text_delta`, `toolcall_end`, `done`). The model appears in `pir models`,
participates in `/model` cycling, and its usage/cost flows through accounting.

API keys: `--api-key` flag → `models.json` `apiKey` (with `$ENV` / `!cmd` templates) → provider env keys
(`OPENAI_API_KEY`, ...) → stored OAuth tokens (`pir auth`). `settings.httpProxy` (or
`PI_HTTP_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`) applies as the request proxy.
