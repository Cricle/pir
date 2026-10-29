import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { EXPECTED_EXPORTS } from './_contract.mjs';

const require = createRequire(import.meta.url);
const index = require('../lib/textstats/index.js');
const old = require('../lib/textstats.js');

assert.deepEqual(Object.keys(index).sort(), EXPECTED_EXPORTS);
for (const name of EXPECTED_EXPORTS) {
  assert.equal(index[name], old[name], `old entry must re-export the same ${name}`);
}
console.log('ok - the new index exports the same objects as the old entry');
