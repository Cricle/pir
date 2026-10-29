import assert from 'node:assert/strict';
import { jsonLines, recordsFile, run } from './_util.mjs';

const file = recordsFile([
  { id: 1, msg: 'Disk FULL', service: 'api' },
  { id: 2, msg: 'disk full again', service: 'worker' },
  { id: 3, msg: 'other', service: 'disk full service' },
  { id: 4, service: 'api' },
  { id: 5, msg: 42, service: 'api' },
]);
const ids = (args) => {
  const result = run(args);
  assert.equal(result.status, 0, result.stderr);
  return jsonLines(result.stdout).map((record) => record.id);
};
try {
  assert.deepEqual(ids(['--grep', '^disk full', file.path]), [1, 2]);
  assert.deepEqual(ids(['--grep', 'OTHER', file.path]), [3]);
  assert.deepEqual(ids(['--grep', 'api', file.path]), []);

  const bad = run(['--grep', '[', file.path]);
  assert.equal(bad.status, 2);
} finally {
  file.cleanup();
}
console.log('ok - grep is case-insensitive, msg-only and rejects invalid patterns');
