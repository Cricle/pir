We use this package as the webhook delivery core. Two production reports came in:

- a customer saw four delivery attempts for a request that kept returning HTTP 400;
- the first retry arrived after ~2s even though `baseDelayMs` is 1000ms.

The exact retry contract is in `README.md`. The current `node --test` suite is
green, but `node repro.js` fails. Please make the implementation match
`README.md` exactly and make both commands pass:

```bash
node --test
node repro.js
```

Keep the package dependency-free and CommonJS. `src/retry_legacy.js` is a stale,
unused copy kept for an old pinned worker — do not wire it back in. Only change
what is needed.
