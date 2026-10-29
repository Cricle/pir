import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { enumerateDates } = require('../src/date-range.js');

for (const bad of [
  '2024-1-01',
  '2024-01-1',
  '099-01-01',
  '24-01-01',
  '2024/01/01',
  '2024-01-01T00:00:00Z',
  ' 2024-01-01',
  '2024-01-01 ',
  '',
]) {
  assert.throws(() => enumerateDates(bad, '2024-03-02'), RangeError, `${JSON.stringify(bad)} must be rejected`);
}

assert.deepEqual(enumerateDates('0001-01-01', '0001-01-03'), ['0001-01-01', '0001-01-02']);
assert.deepEqual(enumerateDates('0099-12-31', '0100-01-02'), ['0099-12-31', '0100-01-01']);
assert.deepEqual(enumerateDates('9999-12-30', '9999-12-31'), ['9999-12-30']);
console.log('ok - only zero-padded YYYY-MM-DD and years 0001-9999 are accepted');
