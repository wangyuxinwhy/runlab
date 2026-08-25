# Repair the query parser

Fix `parseQuery` in `src/query.js` so the package test suite passes.

Required behavior:

- accept a query with or without a leading `?`;
- decode percent escapes and treat every `+` as a space in keys and values;
- split each field at its first `=` so later `=` characters remain in the value;
- preserve repeated keys as an array in encounter order;
- ignore empty `&` segments and return an ordinary empty object for an empty query;
- reject non-string input with `TypeError` and preserve the native decoding error for malformed percent escapes.

Do not change the tests or package scripts. Run `npm test` before finishing.
