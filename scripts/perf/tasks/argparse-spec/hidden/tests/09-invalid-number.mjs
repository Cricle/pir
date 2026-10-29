import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

for (const raw of ['', 'abc', '1,000', '0x10', ' 5', 'Infinity', '1e309']) {
  expectArgError(() => parseArgs([`--count=${raw}`], baseSpec), 'INVALID_NUMBER', {
    option: 'count',
    value: raw,
  });
}
expectArgError(() => parseArgs(['-cNaN'], baseSpec), 'INVALID_NUMBER', { option: 'c', value: 'NaN' });
assert.equal(parseArgs(['--count=1e3'], baseSpec).options.count, 1000);
assert.equal(parseArgs(['--count=+7'], baseSpec).options.count, 7);
console.log('ok - number validation is strict and reports the raw value');
