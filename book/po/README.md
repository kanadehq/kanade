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

```sh
cd book
MDBOOK_BOOK__LANGUAGE=ja mdbook build -d build/ja
# Then open book/build/ja/index.html in a browser.
```

`MDBOOK_BOOK__LANGUAGE` not set → English source renders directly,
no .po consulted.

## Tooling

- [`mdbook-i18n-helpers`](https://github.com/google/mdbook-i18n-helpers)
  — provides `mdbook-gettext` (preprocessor) and `mdbook-xgettext`
  (extractor). Install via:
  ```sh
  cargo install mdbook-i18n-helpers --locked
  ```
- `msgmerge` from GNU gettext — for the sync step.
  - Linux: `apt install gettext`
  - macOS: `brew install gettext`
  - Windows: `scoop install gettext` (or any MSYS2 / Git Bash variant)
