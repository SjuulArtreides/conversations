The install hook patches `@xterm/addon-serialize` 0.14.0 from xterm.js commit
`f447274f430fd22513f6adbf9862d19524471c04` (MIT). Upstream serializes styles and
text but drops OSC 8 targets. Both distributed module formats receive the same
small change; upstream source files and license remain in the package.

The patch emits OSC 8 transitions between linked cell runs, closes links before
row separators, and restores the cursor's actual active link at the end. It
keeps upstream cursor positioning, wrapping, and style serialization. Labels are
never used to infer targets.

Xterm 6.0.0 does not expose hyperlink metadata in its public buffer API. The
patch reads `hasExtendedAttrs()`, `extended.urlId`, and
`_core._oscLinkService.getLinkData()`, matching xterm's native `OscLinkProvider`.
It reads the existing `_inputHandler._curAttrData` field already used by the
serializer to restore cursor styles. These version-specific fields are read only.

`npm ci` applies the patch. The hook checks exact package versions and SHA-256
hashes, and accepts repeated runs only when reversing the complete patch restores
the original hash. Unexpected package contents fail installation. When upgrading
either xterm package, review these internals and run the hyperlink replay tests
before updating the pins and expected hashes.
