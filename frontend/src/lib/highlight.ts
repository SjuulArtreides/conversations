import hljs from "highlight.js/lib/core";
import type { HLJSApi, LanguageFn } from "highlight.js";

// Common languages the app emits (see `langForPath` in BlockRenderer.tsx).
// Each is registered on a shared highlight.js core instance so both the
// direct-highlight path (CodeBlock) and the rehype-highlight path (Markdown)
// colorize the same set of languages with the same grammars.
import typescript from "highlight.js/lib/languages/typescript";
import javascript from "highlight.js/lib/languages/javascript";
import rust from "highlight.js/lib/languages/rust";
import python from "highlight.js/lib/languages/python";
import json from "highlight.js/lib/languages/json";
import markdown from "highlight.js/lib/languages/markdown";
import yaml from "highlight.js/lib/languages/yaml";
import bash from "highlight.js/lib/languages/bash";
import css from "highlight.js/lib/languages/css";
import sql from "highlight.js/lib/languages/sql";
import dockerfile from "highlight.js/lib/languages/dockerfile";
import xml from "highlight.js/lib/languages/xml"; // provides the `html` alias
// `ini` carries an internal `toml` alias, so registering it covers TOML too.
import ini from "highlight.js/lib/languages/ini";
import go from "highlight.js/lib/languages/go";
import nix from "highlight.js/lib/languages/nix";
import diff from "highlight.js/lib/languages/diff";
import graphql from "highlight.js/lib/languages/graphql";
import java from "highlight.js/lib/languages/java";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import csharp from "highlight.js/lib/languages/csharp";
import ruby from "highlight.js/lib/languages/ruby";
import php from "highlight.js/lib/languages/php";
import kotlin from "highlight.js/lib/languages/kotlin";
import swift from "highlight.js/lib/languages/swift";
import scala from "highlight.js/lib/languages/scala";
import makefile from "highlight.js/lib/languages/makefile";
import protobuf from "highlight.js/lib/languages/protobuf";

/**
 * Minimal HCL / Terraform grammar. highlight.js core ships no `hcl` and no
 * lightweight grammar package was installable in this environment, so this is
 * a small hand-rolled definition: comments (hash, slash-slash, block),
 * strings (with `${…}` interpolation), numbers, booleans/null, keywords, and
 * attribute assignments. Enough to colorize `.hcl` / `.tf` rather than leave
 * them plain.
 */
const hcl: LanguageFn = (hl: HLJSApi) => {
  const KEYWORDS = {
    keyword:
      "resource variable provider output module data terraform locals " +
      "for_each count depends_on dynamic provisioner connection backend",
    literal: "true false null",
  };
  return {
    name: "HCL",
    aliases: ["tf", "terraform"],
    case_insensitive: false,
    keywords: KEYWORDS,
    contains: [
      hl.COMMENT("#", "$"),
      hl.COMMENT("//", "$"),
      hl.COMMENT("/\\*", "\\*/"),
      {
        className: "string",
        begin: '"',
        end: '"',
        contains: [
          { className: "subst", begin: /\$\{/, end: /\}/ },
          hl.BACKSLASH_ESCAPE,
        ],
      },
      // Heredoc strings: <<EOF ... EOF / <<-EOF ... EOF
      {
        className: "string",
        begin: /<<-?\s*["']?\w+["']?/,
        end: /^\s*\w+$/,
        relevance: 0,
      },
      hl.NUMBER_MODE,
      {
        // Attribute name on the left of an `=`.
        className: "attr",
        begin: /[\w-]+(?=\s*=)/,
        relevance: 0,
      },
    ],
  };
};

/**
 * The SINGLE source of truth for the languages the app colorizes. Keys are the
 * names/aliases used in markdown fences (and produced by `langForPath` in
 * BlockRenderer.tsx); values are highlight.js grammar functions. Both render
 * paths consume this map — `ensureLanguagesRegistered` (the direct CodeBlock
 * path) registers every entry on the shared core instance, and it IS the
 * `languages` option handed to `rehype-highlight` — so the two paths can never
 * drift.
 *
 * Beyond the always-present web/data languages, this covers the stacks that
 * show up in this monorepo's transcripts: Go, Nix, unified diffs, GraphQL,
 * Makefiles, protobuf, and the common compiled/scripting languages.
 */
const grammars: Record<string, LanguageFn> = {
  typescript,
  javascript,
  rust,
  python,
  json,
  markdown,
  yaml,
  bash,
  css,
  sql,
  dockerfile,
  xml,
  ini,
  toml: ini, // alias the toml fence to the ini grammar explicitly
  hcl,
  go,
  nix,
  diff,
  graphql,
  java,
  c,
  cpp,
  csharp,
  ruby,
  php,
  kotlin,
  swift,
  scala,
  makefile,
  protobuf,
};

let registered = false;

/** Register every app language on the shared core instance exactly once. */
export function ensureLanguagesRegistered(): HLJSApi {
  if (registered) return hljs;
  for (const [name, grammar] of Object.entries(grammars)) {
    hljs.registerLanguage(name, grammar);
  }
  registered = true;
  return hljs;
}

/**
 * The language map for `rehype-highlight`'s `languages` option — the same
 * {@link grammars} map the direct-highlight path registers, so a fence and a
 * file-path-derived block colorize identically.
 */
export const rehypeLanguages: Record<string, LanguageFn> = grammars;

/**
 * Highlight `code` as `language` using the shared instance. Returns the
 * hljs-escaped HTML string, or `null` if the language isn't registered (the
 * caller should then render plain text under the `.hljs` base class).
 */
export function highlightCode(code: string, language: string): string | null {
  const hl = ensureLanguagesRegistered();
  if (!language || !hl.getLanguage(language)) return null;
  return hl.highlight(code, { language, ignoreIllegals: true }).value;
}
