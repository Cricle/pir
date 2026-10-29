import assert from 'node:assert/strict';
import { jsonLines, recordsFile, run } from './_util.mjs';

const file = recordsFile([
  { id: 1, level: 'info', service: 'api' },
  { id: 2, level: 'ERROR', service: 'api' },
  { id: 3, level: 'warn', service: 'worker' },
  { id: 4, level: 'Error', service: 'worker' },
  { id: 5, level: 'debug', service: 'api' },
]);
const ids = (args) => {
  const result = run(args);
  assert.equal(result.status, 0, result.stderr);
  return jsonLines(result.stdout).map((record) => record.id);
};
try {
  assert.deepEqual(ids(['--level', 'error', file.path]), [2, 4]);
  assert.deepEqual(ids(['--level', 'error', '--level', 'warn', file.path]), [2, 3, 4]);
  assert.deepEqual(ids(['--service', 'api', file.path]), [1, 2, 5]);
  assert.deepEqual(ids(['--service', 'api', '--service', 'worker', file.path]), [1, 2, 3, 4, 5]);
  assert.deepEqual(ids(['--level', 'error', '--service', 'worker', file.path]), [4]);
} finally {
  file.cleanup();
}
console.log('ok - level is case-insensitive and service is exact, both OR within a filter');
