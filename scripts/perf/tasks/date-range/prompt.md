CI is red on `test/date-range.test.js`. Please get the suite green:

```bash
node --test
```

`README.md` documents the behavior downstream consumers rely on; it is the
source of truth and must not be changed. Adjust the code or the tests as
needed, but keep the test file in place and keep the documented contract
intact. Do not add dependencies.
