function decodePart(value) {
  return decodeURIComponent(value.replace("+", " "));
}

export function parseQuery(input) {
  if (typeof input !== "string") {
    throw new TypeError("query must be a string");
  }

  const query = input.startsWith("?") ? input.slice(1) : input;
  const result = {};

  for (const segment of query.split("&")) {
    if (segment === "") continue;
    const [rawKey, rawValue = ""] = segment.split("=");
    const key = decodePart(rawKey);
    result[key] = decodePart(rawValue);
  }

  return result;
}
