# webhook-delivery

Tiny, dependency-free webhook delivery core used by a billing service.

## Public API

```js
const { withRetry, deliverWebhook, isRetryable, httpError, HttpError } = require('./src/index.js');
```

### `withRetry(operation, options)`

`operation(attempt)` is invoked with a 1-based attempt number and may return a
value or a Promise. `withRetry` resolves with the first successful value.

Options:

| option | default | meaning |
| --- | --- | --- |
| `maxRetries` | `3` | number of retries *after* the first attempt. Total attempts are at most `maxRetries + 1`. |
| `baseDelayMs` | `1000` | base for exponential backoff |
| `maxDelayMs` | `30000` | cap for each delay |
| `sleep` | `setTimeout` wrapper | `sleep(ms) => Promise<void>`, injectable for tests |
| `onRetry` | none | `onRetry({ attempt, delayMs, error })`, called after a retryable failure and before the sleep; `attempt` is the 1-based *retry* number about to happen |
| `shouldRetry` | `isRetryable` | predicate deciding whether an error is retryable |

Retry semantics:

* The delay before retry number `n` (1-based) is
  `min(baseDelayMs * 2 ** (n - 1), maxDelayMs)`.
* When retries are exhausted, the last error is thrown and its `attempts`
  property is set to the total number of attempts that were made.
* A non-retryable error is rethrown as-is after the first attempt; `attempts`
  is not added.
* `onRetry` is not called for a non-retryable error, nor after the final
  allowed attempt.

### Retry classification (`isRetryable(error)`)

1. If `error.retryable` is a boolean, it wins.
2. Otherwise, if `error.status` is an integer, retry only `408`, `429`, `500`,
   `502`, `503` and `504`.
3. Otherwise, if `error.code` is one of `ECONNRESET`, `ETIMEDOUT`, `EPIPE`,
   `EAI_AGAIN`, retry.
4. Everything else is not retryable (including plain `TypeError`s).

### `deliverWebhook(transport, payload, options)`

Calls `transport(payload, attempt)`. The transport may throw or return
`{ status, body }`:

* `status >= 400` becomes an `HttpError` (with `status`), which is then subject
  to the retry policy.
* a `2xx` response resolves `deliverWebhook` with `response.body`.
* a transport result without a numeric `status` is a `TypeError` and is not
  retried.

## Tests

```bash
node --test
node repro.js
```

`node --test` currently passes. The issue tracker says production deliveries do
not match the contract above.
