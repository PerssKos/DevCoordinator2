// JSON.parse accepts duplicate members. Source catalogs must reject them before
// a later value can silently replace an earlier translation in the same file.
export function duplicateKeys(json) {
  JSON.parse(json);
  const tokens = json.match(/"(?:\\.|[^"\\])*"|-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?|true|false|null|[{}\[\]:,]/g);
  let cursor = 0; const duplicates = [];
  function value() {
    const token = tokens[cursor++];
    if (token === '{') {
      const keys = new Set();
      while (tokens[cursor] !== '}') {
        const key = JSON.parse(tokens[cursor++]); cursor++; // colon
        if (keys.has(key)) duplicates.push(key); keys.add(key);
        value(); if (tokens[cursor] === ',') cursor++;
      }
      cursor++;
    } else if (token === '[') {
      while (tokens[cursor] !== ']') { value(); if (tokens[cursor] === ',') cursor++; }
      cursor++;
    }
  }
  value(); return duplicates;
}
