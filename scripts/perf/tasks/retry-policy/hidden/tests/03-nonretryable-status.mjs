import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

for (const status of [400, 401, 403, 404, 409, 422]) {
  const delays = [];
  let calls = 0;
  const failure = new Error(`status ${status}`);
  failure.status = status;
  let thrown;
  try {
    await withRetry(async () => {
      calls += 1;
      throw failure;
    }, {
      maxRetries: 5,
      baseDelayMs: 10,
      sleep: async (ms) => delays.push(ms),
    });
  } catch (error) {
    thrown = error;
  }
  assert.equal(thrown, failure, `status ${status} must rethrow the original error`);
  assert.equal(calls, 1, `status ${status} must not be attempted twice`);
  assert.deepEqual(delays, [], `status ${status} must not sleep`);
  assert.equal(thrown.attempts, undefined, `status ${status} must not gain attempts`);
}
console.log('ok - non-retryable HTTP statuses are rethrown immediately');
