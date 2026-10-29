import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

// Retryable transport codes are retried even though there is no status.
for (const code of ['ECONNRESET', 'ETIMEDOUT', 'EPIPE', 'EAI_AGAIN']) {
  const delays = [];
  let calls = 0;
  const result = await withRetry(async () => {
    calls += 1;
    if (calls === 1) {
      const error = new Error('transport');
      error.code = code;
      throw error;
    }
    return code;
  }, {
    maxRetries: 2,
    baseDelayMs: 10,
    sleep: async (ms) => delays.push(ms),
  });
  assert.equal(result, code);
  assert.equal(calls, 2, `${code} must be retried once`);
  assert.deepEqual(delays, [10]);
}

// Programming errors are not retried.
const delays = [];
let calls = 0;
await assert.rejects(
  withRetry(async () => {
    calls += 1;
    throw new TypeError('boom');
  }, { sleep: async (ms) => delays.push(ms) }),
  TypeError,
);
assert.equal(calls, 1);
assert.deepEqual(delays, []);
console.log('ok - retryable transport codes retry, TypeError does not');
