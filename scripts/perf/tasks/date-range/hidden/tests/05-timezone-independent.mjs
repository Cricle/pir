import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const root = resolve(HERE, '..');

const script = `
const { enumerateDates } = require('./src/date-range.js');
console.log(JSON.stringify([
  enumerateDates('2024-11-01', '2024-11-06'),
  enumerateDates('2024-03-08', '2024-03-12'),
]));
`;

const expected = [
  ['2024-11-01', '2024-11-02', '2024-11-03', '2024-11-04', '2024-11-05'],
  ['2024-03-08', '2024-03-09', '2024-03-10', '2024-03-11'],
];

for (const zone of ['UTC', 'America/New_York', 'Europe/Berlin', 'Australia/Lord_Howe']) {
  const result = spawnSync(process.execPath, ['-e', script], {
    cwd: root,
    encoding: 'utf8',
    timeout: 20_000,
    env: { ...process.env, NODE_OPTIONS: '', TZ: zone },
  });
  assert.equal(result.status, 0, `${zone}: ${result.stderr}`);
  assert.deepEqual(JSON.parse(result.stdout.trim()), expected, `wrong dates under TZ=${zone}`);
}
console.log('ok - results are identical across DST timezones');
