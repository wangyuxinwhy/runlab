import assert from "node:assert/strict";

const { parseQuery } = await import("file:///experiments/query/src/query.js");

assert.deepEqual(parseQuery(""), {});
assert.deepEqual(parseQuery("?x=%2B+%26&x=second&empty="), {
  x: ["+ &", "second"],
  empty: "",
});
assert.deepEqual(parseQuery("a+b+c=1+2+3"), { "a b c": "1 2 3" });
assert.deepEqual(parseQuery("value=left=middle=right"), {
  value: "left=middle=right",
});
assert.throws(() => parseQuery(null), TypeError);
assert.throws(() => parseQuery("bad=%E0%A4%A"), URIError);

console.log(JSON.stringify({ verifier: "query", valid: true }));
