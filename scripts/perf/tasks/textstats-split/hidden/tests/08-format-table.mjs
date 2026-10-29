import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { formatTable } = require('../lib/textstats/index.js');

assert.equal(
  formatTable([['a', 1], ['bbb', 22]], { headers: ['name', 'n'], align: ['left', 'right'] }),
  'name |  n\na    |  1\nbbb  | 22',
);
assert.equal(formatTable([['a'], ['bb', 2]]), 'a  |\nbb | 2');
assert.equal(formatTable([], { headers: ['a', 'bb'] }), 'a | bb');
assert.equal(formatTable([]), '');
assert.equal(formatTable([['a\nb']]), 'a\\nb');
assert.equal(
  formatTable([['x', 1], ['yy', 22]], { align: 'right' }),
  ' x |  1\nyy | 22',
);
assert.equal(formatTable([['x', 1]]), 'x | 1');
assert.equal(formatTable([[null, undefined]]), ' |');
assert.throws(() => formatTable('nope'), TypeError);
assert.throws(() => formatTable([1, 2]), TypeError);
assert.throws(() => formatTable([], { headers: 'x' }), TypeError);
assert.throws(() => formatTable([['x']], { align: 'center' }), RangeError);
console.log('ok - formatTable rendering and validation');
