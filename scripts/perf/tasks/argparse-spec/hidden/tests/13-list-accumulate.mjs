import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, withOptions } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['--tag', 'a', '--tag=b', '-t', 'c', '-td'], baseSpec);
assert.deepEqual(parsed.options.tag, ['a', 'b', 'c', 'd']);

// A list default is copied per call, never shared.
const listSpec = withOptions({ labels: { type: 'list', default: ['x', 'y'] } });
const first = parseArgs([], listSpec);
const second = parseArgs([], listSpec);
first.options.labels.push('z');
assert.deepEqual(first.options.labels, ['x', 'y', 'z']);
assert.deepEqual(second.options.labels, ['x', 'y']);

const empty = parseArgs([], baseSpec);
assert.deepEqual(empty.options.tag, []);
console.log('ok - list options append, and defaults are per-call copies');
