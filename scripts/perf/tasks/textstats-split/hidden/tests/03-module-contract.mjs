import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { SPLIT_MODULES } from './_contract.mjs';

const require = createRequire(import.meta.url);
const index = require('../lib/textstats/index.js');

for (const [file, expectedKeys] of Object.entries(SPLIT_MODULES)) {
  const moduleExports = require(`../lib/textstats/${file}`);
  assert.deepEqual(
    Object.keys(moduleExports).sort(),
    [...expectedKeys].sort(),
    `${file} must export exactly its assigned functions`,
  );
  for (const key of expectedKeys) {
    assert.equal(index[key], moduleExports[key], `index.js must re-export ${key} from ${file}`);
  }
}
console.log('ok - each split module exports only its assigned surface');
