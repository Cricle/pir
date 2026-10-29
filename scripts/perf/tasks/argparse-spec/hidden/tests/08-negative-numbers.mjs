import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

assert.equal(parseArgs(['--count', '-5'], baseSpec).options.count, -5);
assert.equal(parseArgs(['-c', '-.5'], baseSpec).options.count, -0.5);
assert.equal(parseArgs(['-c-2.5e3'], baseSpec).options.count, -2500);
assert.equal(parseArgs(['--count=-0.25'], baseSpec).options.count, -0.25);

const positional = parseArgs(['-5', '-.5', '--name=-5'], baseSpec);
assert.deepEqual(positional.positionals, ['-5', '-.5']);
assert.equal(positional.options.name, '-5');

expectArgError(() => parseArgs(['--name', '-5'], baseSpec), 'MISSING_VALUE', { option: 'name' });
expectArgError(() => parseArgs(['--count', '-'], baseSpec), 'MISSING_VALUE', { option: 'count' });
console.log('ok - negative numbers are values only for numeric options');
