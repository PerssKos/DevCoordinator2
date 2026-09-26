# Console message catalogs

`manifest.json` owns the locale registry and maps each namespace to **an array
of files**. A namespace can be split into smaller files without changing message
IDs or page code. English is the source locale. Source IDs are stable: changing
copy does not require renaming its ID.

A file is a UTF-8 JSON object mapping IDs to plain-text messages. Parameters use
`{name}`. Plurals use `{ "argument": "count", "forms": { "one": "…", "other": "…" } }`;
include every category returned by `Intl.PluralRules` for that locale. Exact
numeric forms such as `=0` are supported. Translations contain no HTML. DOM
insertion and attribute escaping belong to the renderer.

Product templates mark their own text with `data-i18n="namespace.id"` and JSON
`data-i18n-args`; attributes use `data-i18n-attrs`. Dynamic code can use the shared
runtime's `text` and `markup` bindings. Language changes update those explicit
bindings in place. Never translate by matching arbitrary DOM text: tasks, user
names, comments, log output and artifact content are not application messages.

The shared namespaces load first; feature fragments load on demand. Each source
and translated namespace is assembled atomically. Failed downloads remain
retryable; missing messages use English. Simplified and Traditional Chinese and
Serbian script choices are matched separately. Browser preferences are language
preferences, not claims that the browser has installed a Console translation.

To add a language, add its canonical tag, English/native names, direction,
one to three representative speaking countries, and namespace file arrays.
Country flags are supplementary and do not identify a language. Use licensed
local flag assets. Begin with `status: "draft"`. Translate all required fragments
and run `node scripts/locales/validate.mjs --locale TAG` for that locale, then
`node scripts/locales/validate.mjs --all` for complete rollout admission.
Ordinary validation checks enabled locales and explicitly reports drafts.
An optional `intlFallbacks` array declares formatting and sorting fallbacks for
languages missing from an Intl service. The selected language is tried first
and English last; the browser or server default never decides the fallback.
Romansh explicitly falls back to English collation where native collation is
unavailable. Missing English-fallback messages keep English plural grammar.
Linguistic review is separate from key parity; copying English into a locale is
not a completed translation. Keep the locale draft until review is complete.
Run `node scripts/locales/audit-content.mjs --locale TAG` to find exact English
product messages and `node scripts/locales/audit-contamination.mjs --locale TAG`
to review mixed-language phrases and partial substitutions. Review every
candidate in context; technical identifiers and brand names may remain, but a
mixed-language sentence must be rewritten in the target language. These audits
are admission evidence alongside rendered route checks, not replacements for
linguistic review. The contamination check includes narrow regression guards
for observed German donor fragments; it is not a general language detector.
Read every namespace and every plural branch before admitting a locale.

The first rollout covers nationally official/co-official written languages in
the agreed Council of Europe scope, plus Russian and four East Asian entries.
The registry includes both Norwegian written standards, both Serbian and
Montenegrin scripts, and the two Chinese scripts. It is extensible; regional and
minority languages outside that initial boundary can be added through the same
workflow. English, Chinese scripts and country flags do not follow one-to-one
country/language assumptions.
