import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['-nAlice', '-c5', '-t', 'one', '-o=out.txt', '-t=two'], baseSpec);
assert.equal(parsed.options.name, 'Alice');
assert.equal(parsed.options.count, 5);
assert.deepEqual(parsed.options.tag, ['one', 'two']);
assert.equal(parsed.options.output, 'out.txt');
assert.deepEqual(parsed.positionals, []);

const empty = parseArgs(['-n='], baseSpec);
assert.equal(empty.options.name, '');
console.log('ok - short options support attached and = values');
