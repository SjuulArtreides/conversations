/* global __dirname */
// @xterm/addon-serialize 0.14.0, upstream commit f447274f430fd22513f6adbf9862d19524471c04.
// MIT-licensed upstream source remains in the package. Its public cell API lacks
// hyperlink data; use the same read-only xterm6 fields as its native OSC provider.
// Keep the upstream cursor, SGR and wrapping algorithms. Only emit OSC8 runs.
const { createHash } = require("node:crypto");
const { readFileSync, writeFileSync } = require("node:fs");
const { join } = require("node:path");
const root = join(__dirname, "../node_modules/@xterm/addon-serialize");
if (JSON.parse(readFileSync(join(root, "package.json"))).version !== "0.14.0") {
  throw new Error(
    "Review the OSC8 serializer patch before upgrading addon-serialize",
  );
}
const xterm = JSON.parse(
  readFileSync(join(__dirname, "../node_modules/@xterm/xterm/package.json")),
);
if (xterm.version !== "6.0.0") {
  throw new Error("Review xterm hyperlink internals before upgrading xterm");
}
const close = "\\x1b]8;;\\x1b\\\\";
const methods = `
_osc8(cell) {
  const id = cell.hasExtendedAttrs() && cell.extended?.urlId;
  const link = id && this._terminal._core._oscLinkService.getLinkData(id);
  return link
    ? "\\x1b]8;id=" + (link.id ?? ("serialize-" + id)) + ";" + link.uri + "\\x1b\\\\"
    : "${close}";
}
_serializeChars(cell) {
  const next = this._osc8(cell);
  const prefix = next === this._serializedLink ? "" : next;
  this._serializedLink = next;
  return prefix + cell.getChars();
}
_closeLink() {
  if (this._serializedLink && this._serializedLink !== "${close}") {
    this._currentRow += "${close}";
  }
  this._serializedLink = undefined;
}
`;
const patches = {
  "lib/addon-serialize.mjs": {
    hash: "6c93dff90bef3b7aac8664fa4c1cb4026bf4b701f3014a1fb88edde943dab906",
    edits: [
      [
        '_nextCell(e,r,t,a){if(e.getWidth()===0)return;let i=e.getChars()==="",n=this._diffStyle(e,this._cursorStyle);',
        methods +
          '_nextCell(e,r,t,a){if(e.getWidth()===0)return;let i=e.getChars()==="",n=this._diffStyle(e,this._cursorStyle);',
      ],
      [
        "this._currentRow+=e.getChars()",
        "this._currentRow+=this._serializeChars(e)",
      ],
      [
        "this._allRows[this._rowIndex]=this._currentRow",
        "this._closeLink(),this._allRows[this._rowIndex]=this._currentRow",
      ],
      [
        'return s.length>0&&(t+=`\\x1B[${s.join(";")}m`),t}',
        'return s.length>0&&(t+=`\\x1B[${s.join(";")}m`),t+this._osc8(a)}',
      ],
    ],
  },
  "lib/addon-serialize.js": {
    hash: "65d8dd7c2b3b37a77f583ea5e0d14a23bbcc846e4ae0d6bb534ae12a380aea0d",
    edits: [
      [
        '_nextCell(t,e,s,r){if(0===t.getWidth())return;const i=""===t.getChars(),o=this._diffStyle(t,this._cursorStyle);',
        methods +
          '_nextCell(t,e,s,r){if(0===t.getWidth())return;const i=""===t.getChars(),o=this._diffStyle(t,this._cursorStyle);',
      ],
      [
        "this._currentRow+=t.getChars()",
        "this._currentRow+=this._serializeChars(t)",
      ],
      [
        "this._allRows[this._rowIndex]=this._currentRow",
        "this._closeLink(),this._allRows[this._rowIndex]=this._currentRow",
      ],
      [
        'return o.length>0&&(s+=`\x1b[${o.join(";")}m`),s}',
        'return o.length>0&&(s+=`\x1b[${o.join(";")}m`),s+this._osc8(i)}',
      ],
    ],
  },
};
for (const [name, { hash, edits }] of Object.entries(patches)) {
  const path = join(root, name);
  let source = readFileSync(path, "utf8");
  // Accept only the complete known patch, never a substring of a partial edit.
  let original = source;
  for (const [before, after] of [...edits].reverse())
    original = original.replace(after, before);
  if (
    edits.every(([, after]) => source.split(after).length === 2) &&
    source !== original &&
    createHash("sha256").update(original).digest("hex") === hash
  )
    continue;
  if (createHash("sha256").update(source).digest("hex") !== hash) {
    throw new Error(`Unexpected ${name}; review the OSC8 serializer patch`);
  }
  for (const [before, after] of edits) {
    if (source.split(before).length !== 2)
      throw new Error(`Patch target changed: ${before}`);
    source = source.replace(before, after);
  }
  writeFileSync(path, source);
}
