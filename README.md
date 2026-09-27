# md4less

Read Markdown and Org files in `less`, typeset rather than raw.

One Rust file, no dependencies, ~250 MB/s: a typical README renders faster
than the process starts.

## Install

```sh
cargo build --release
install -m 755 target/release/md4less ~/.local/bin/
```

## Use

Hook it into `less` from your shell rc:

```sh
export LESSOPEN="|$HOME/.local/bin/md4less %s"
export LESS=-R   # let less pass the styling through
```

Then `less notes.org` or `less README.md` renders the file. Anything else
produces no output, so less shows it as usual.

On its own, it writes to stdout; stdin is read as Markdown unless `--org`:

```sh
md4less README.md | less -R
curl -s https://example.com/doc.org | md4less --org | less -R
```

## Look

Ink on paper:

- greys only, with weight carrying the hierarchy; colour is kept for warnings
- headings flush with the text, their level (`H1`, `H2`, …) in the margin
- a rule under the title across the page, and under each H1 as wide as its text
- prose wrapped at 80 columns, lists and quotes hanging under their text
- markup and metadata faded: drawers, keywords, dates, tags, comments, URLs
- typographic bullets and check boxes, and booktabs tables

Code and tables are never wrapped.

The palette follows the terminal background (`COLORFGBG`). Set
`MD4LESS_LIGHT` to `day`, `night` or `plain` to choose one; `plain` uses
bold and dim only, for terminals without truecolor.
