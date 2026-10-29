import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { deliverWebhook } = require('../src/index.js');

// Transient HTTP failures from the transport are retried.
{
  const delays = [];
  let calls = 0;
  const body = await deliverWebhook(async () => {
    calls += 1;
    if (calls === 1) return { status: 503, body: 'later' };
    return { status: 200, body: { id: 'wh_9' } };
  }, { event: 'invoice.paid' }, {
    maxRetries: 3,
    baseDelayMs: 20,
    sleep: async (ms) => delays.push(ms),
  });
  assert.deepEqual(body, { id: 'wh_9' });
  assert.equal(calls, 2);
  assert.deepEqual(delays, [20]);
}

// A 400 is surfaced once as an HttpError with the status attached.
{
  let calls = 0;
  let thrown;
  try {
    await deliverWebhook(async () => {
      calls += 1;
      return { status: 400, body: 'invalid signature' };
    }, {}, { maxRetries: 4, sleep: async () => {} });
  } catch (error) {
    thrown = error;
  }
  assert.ok(thrown);
  assert.equal(thrown.name, 'HttpError');
  assert.equal(thrown.status, 400);
  assert.equal(calls, 1);
}

// A malformed transport result is a TypeError and is not retried.
{
  let calls = 0;
  await assert.rejects(deliverWebhook(async () => {
    calls += 1;
    return 'not-a-response';
  }, {}, { maxRetries: 4, sleep: async () => {} }), TypeError);
  assert.equal(calls, 1);
}
console.log('ok - deliverWebhook applies the retry policy to HTTP responses');
