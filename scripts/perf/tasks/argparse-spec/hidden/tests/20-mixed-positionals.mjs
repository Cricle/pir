import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['file', '--verbose', '--count', '-2', '--', '-x'], baseSpec);
assert.deepEqual(parsed.positionals, ['file', '-x']);
assert.equal(parsed.options.verbose, true);
assert.equal(parsed.options.count, -2);

const values = parseArgs(['--name=-x', '-o-x', '--tag=-', '-n-x'], baseSpec);
assert.equal(values.options.name, '-x');
assert.equal(values.options.output, '-x');
assert.deepEqual(values.options.tag, ['-']);
assert.deepEqual(values.positionals, []);
console.log('ok - interspersed positionals and attached dash values');
