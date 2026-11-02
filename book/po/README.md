# Translations

English (the markdown under `book/src/`) is the source language;
each other language lives as a single gettext `.po` file in this
directory. The `mdbook-gettext` preprocessor swaps msgids for the
matching msgstr at build time.

Currently shipped: **Japanese (`ja.po`)**.

## Workflow

### Updating an existing language after `src/` changes

When `book/src/**/*.md` is edited, the message template
(`po/messages.pot`) goes stale, and so do the per-language
`.po` files. To bring them back in sync:

```sh
cd book

# 1. Regenerate the template from the current source.
MDBOOK_OUTPUT__XGETTEXT__POT_FILE=po/messages.pot mdbook build -d po-tmp
rm -rf po-tmp

# 2. Merge new / changed msgids into each .po (uses gettext's
#    msgmerge — install via apt/brew/scoop if missing).
for f in po/*.po; do
  msgmerge --update --backup=none "$f" po/messages.pot
done
```

`msgmerge` marks unchanged-but-still-valid translations as
translated, new msgids as untranslated (empty `msgstr ""`), and
edited msgids as **fuzzy** (the old translation is kept with a
`#, fuzzy` marker — translators should review and remove the
fuzzy flag once they've checked it).

Untranslated and fuzzy entries fall back to the source English at
build time, so the site stays valid mid-translation.

### Adding a new language

```sh
cp po/messages.pot po/<lang>.po
```

Then in the new file:
- Change the header `Language: en` to `Language: <lang>`.
- Fill in `msgstr` for the entries you want translated.

Wire it into the CI by adding a build step in
`.github/workflows/docs.yml` mirroring the existing `Build
Japanese` step.

### Verifying locally

Install the pinned tools below, then build both languages with the same
site paths as CI (leave `MDBOOK_BOOK__LANGUAGE` unset for English):

```sh
cd book
MDBOOK_OUTPUT__HTML__SITE_URL=/kanade/en/ mdbook build -d build/en
MDBOOK_BOOK__LANGUAGE=ja MDBOOK_OUTPUT__HTML__SITE_URL=/kanade/ja/ \
  mdbook build -d build/ja
```

Check `book/build/en/index.html` and `book/build/ja/index.html`: the
Japanese page should have `lang="ja"` and show existing `ja.po`
translations such as **はじめに** and **クイックスタート**. Serve the
output under `/kanade/` when checking the CI site paths in a browser.

`MDBOOK_BOOK__LANGUAGE` not set → English source renders directly,
no .po consulted.

## Tooling

- [`mdBook`](https://github.com/rust-lang/mdBook) **0.4.40** — match
  `MDBOOK_VERSION` in `.github/workflows/docs.yml`:
  ```sh
  cargo install mdbook --version 0.4.40 --locked
  ```
- [`mdbook-i18n-helpers`](https://github.com/google/mdbook-i18n-helpers)
  **0.3.6** — match `MDBOOK_I18N_HELPERS_VERSION` in the same workflow.
  Provides `mdbook-gettext` (preprocessor) and `mdbook-xgettext`
  (extractor). Install with the same C language standard as CI:
  ```sh
  CFLAGS=-std=gnu17 cargo install mdbook-i18n-helpers --version 0.3.6 --locked
  ```
  The locked `onig_sys` bundles Oniguruma with pre-C23 function
  declarations. [GCC 15 defaults to C23](https://gcc.gnu.org/gcc-15/porting_to.html#c23),
  which changes their meaning and causes incompatible function-pointer
  errors. Selecting GNU C17 for this installation preserves the expected
  declarations without suppressing type diagnostics. Keep this tool pair
  aligned with CI when extracting messages or checking translations;
  upgrading across mdBook compatibility versions requires validating the
  existing `.po` files as well as both language builds.
- `msgmerge` from GNU gettext — for the sync step.
  - Linux: `apt install gettext`
  - macOS: `brew install gettext`
  - Windows: `scoop install gettext` (or any MSYS2 / Git Bash variant)
