import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { enumerateDates } = require('../src/date-range.js');

assert.deepEqual(enumerateDates('2024-12-30', '2025-01-02'), [
  '2024-12-30',
  '2024-12-31',
  '2025-01-01',
]);
assert.deepEqual(enumerateDates('1999-12-30', '2000-01-02'), [
  '1999-12-30',
  '1999-12-31',
  '2000-01-01',
]);
assert.deepEqual(enumerateDates('2000-02-28', '2000-03-01'), [
  '2000-02-28',
  '2000-02-29',
]);
assert.deepEqual(enumerateDates('1900-02-28', '1900-03-01'), ['1900-02-28']);
console.log('ok - year boundaries and century leap rules');
