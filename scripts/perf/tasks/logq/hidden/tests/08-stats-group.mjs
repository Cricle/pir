import assert from 'node:assert/strict';
import { jsonLines, recordsFile, run } from './_util.mjs';

const file = recordsFile([
  { id: 1, service: 'api', latency_ms: 10, user: { region: 'eu' } },
  { id: 2, service: 'api', latency_ms: 20, user: { region: 'us' } },
  { id: 3, service: 'worker', latency_ms: 5, user: { region: 'eu' } },
  { id: 4, service: 'worker', latency_ms: 'slow', user: { region: 'us' } },
  { id: 5, service: 'web', user: { region: 'eu' } },
]);
const parseOne = (args) => {
  const result = run(args);
  assert.equal(result.status, 0, result.stderr);
  return JSON.parse(result.stdout.trim());
};
try {
  const stats = parseOne(['--stats', 'latency_ms', file.path]);
  assert.deepEqual(stats, { count: 3, min: 5, max: 20, avg: 35 / 3, sum: 35 });

  const empty = parseOne(['--stats', 'missing', file.path]);
  assert.deepEqual(empty, { count: 0, min: null, max: null, avg: null, sum: null });

  const groupedResult = run(['--stats', 'latency_ms', '--group-by', 'service', file.path]);
  assert.equal(groupedResult.status, 0, groupedResult.stderr);
  const grouped = jsonLines(groupedResult.stdout);
  assert.deepEqual(grouped, [
    { service: 'api', count: 2, min: 10, max: 20, avg: 15, sum: 30 },
    { service: 'web', count: 0, min: null, max: null, avg: null, sum: null },
    { service: 'worker', count: 1, min: 5, max: 5, avg: 5, sum: 5 },
  ]);

  const nestedResult = run(['--stats', 'latency_ms', '--group-by', 'user.region', file.path]);
  assert.equal(nestedResult.status, 0, nestedResult.stderr);
  const nested = jsonLines(nestedResult.stdout);
  assert.deepEqual(nested.map((group) => group.region), ['eu', 'us']);
  assert.deepEqual(nested[0], { region: 'eu', count: 2, min: 5, max: 10, avg: 7.5, sum: 15 });

  assert.equal(run(['--group-by', 'service', file.path]).status, 2);
} finally {
  file.cleanup();
}

const missing = recordsFile([
  { id: 1, service: 'beta', latency_ms: 1 },
  { id: 2, latency_ms: 2 },
]);
try {
  const result = run(['--stats', 'latency_ms', '--group-by', 'service', missing.path]);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(jsonLines(result.stdout), [
    { service: 'beta', count: 1, min: 1, max: 1, avg: 1, sum: 1 },
    { service: null, count: 1, min: 2, max: 2, avg: 2, sum: 2 },
  ]);
} finally {
  missing.cleanup();
}
console.log('ok - stats and grouped stats aggregate finite numbers');
