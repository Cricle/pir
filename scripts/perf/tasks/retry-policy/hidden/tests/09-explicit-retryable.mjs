import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

// Boolean error.retryable overrides the status table in both directions.
{
  let calls = 0;
  const result = await withRetry(async () => {
    calls += 1;
    if (calls === 1) {
      const error = new Error('conflict');
      error.status = 409;
      error.retryable = true;
      throw error;
    }
    return 'retried';
  }, { maxRetries: 2, baseDelayMs: 5, sleep: async () => {} });
  assert.equal(result, 'retried');
  assert.equal(calls, 2);
}

{
  let calls = 0;
  const failure = new Error('do not retry this 503');
  failure.status = 503;
  failure.retryable = false;
  await assert.rejects(withRetry(async () => {
    calls += 1;
    throw failure;
  }, { maxRetries: 5, sleep: async () => {} }), (error) => error === failure);
  assert.equal(calls, 1);
}
console.log('ok - explicit retryable overrides status classification');
