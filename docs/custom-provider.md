# Custom providers

pir speaks every upstream protocol adapter natively (see [models.md](models.md) for `api`). A "custom provider" is
a models.json entry whose `baseUrl` points at your gateway — no code needed.

## OpenAI-compat auto-detect

An unknown endpoint can omit `api`: pir falls back to the provider config, then a catalog model default, then
`openai-chat` (`openai-completions`), the OpenAI-compatible adapter. Its compat knobs (stream options, tool-call
shapes) are auto-detected from the provider id and base URL — pir does not probe the endpoint.

## Virtual models (registerVirtualModel)

JS extensions can register a virtual model that pir routes per-request. A registration names the provider it is
listed under (`provider`), a model `id`, and the `physical` model it routes to; an optional `route(request)`
callback may pick the physical model per request instead:

```js
export default function (pi) {
  pi.registerVirtualModel({
    provider: "my-gateway",
    id: "my-router",
    name: "My Router",
    physical: { provider: "my-gateway", id: "my-model" },
    // Optional per-request route, overriding `physical`; falls back to it on error:
    route: (request) => ({ model: { provider: "my-gateway", id: "my-model" }, thinkingLevel: "high" }),
  });
}
```

`route(request)` receives `model` (the virtual entry), `thinkingLevel`, and `messages` (plus any `state` returned
on a previous route), and returns `{ model: { provider, id }, thinkingLevel?, state? }`. The model joins the
session's model registry (so `/model` cycling and the TUI model list see it), and its usage/cost flows through
the routed provider's accounting. `state` is held in memory and fed back as `request.state` on later routes.

API keys: `--api-key` flag → `models.json` `apiKey` (with `$ENV` / `!cmd` templates) → a stored credential
(`pir auth`: OAuth, then a stored API key) → provider env keys (`OPENAI_API_KEY`, ...). `settings.httpProxy` (or
`PI_HTTP_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`) applies as the request proxy.
