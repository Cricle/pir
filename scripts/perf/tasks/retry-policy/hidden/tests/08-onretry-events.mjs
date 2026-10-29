import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

const events = [];
let calls = 0;
const result = await withRetry(async () => {
  calls += 1;
  if (calls <= 2) {
    const error = new Error(`failure-${calls}`);
    error.status = 500;
    throw error;
  }
  return 'done';
}, {
  maxRetries: 3,
  baseDelayMs: 100,
  onRetry: (event) => events.push({
    attempt: event.attempt,
    delayMs: event.delayMs,
    message: event.error.message,
  }),
  sleep: async () => {},
});

assert.equal(result, 'done');
assert.deepEqual(events, [
  { attempt: 1, delayMs: 100, message: 'failure-1' },
  { attempt: 2, delayMs: 200, message: 'failure-2' },
]);
console.log('ok - onRetry reports 1-based retry numbers and delays');
