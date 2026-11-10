# Grand Archives

`archivist`: find and download free books, as a Rust library and a small
CLI. One search asks [Project Gutenberg](https://www.gutenberg.org) (through
Gutendex), [Open Library](https://openlibrary.org) and the
[Internet Archive](https://archive.org), merges the answers into one list
with the best copy of each book first, and one command downloads it:

```
archivist find the time machine                   # every catalogue, one list
archivist grab the time machine --out /mnt/onboard/Books   # find + download the top match
archivist grab moby dick --pick 3 --out .         # or the third result
```

Gutenberg's proofread EPUBs rank above the Archive's raw scans, the same
book from two catalogues is one row, and a catalogue that is down is
reported while the others still answer. Only free, legally distributable
books are searched: public-domain texts and public scans, never the
lending library.

The single-catalogue commands below still work.
Built for [gideon](https://github.com/evanspn/gideon) on the Kobo, where it
backs the in-app "Internet Archive" catalogue (gideon issue #201); it began
life in gideon's tree (gideon PR #186) and has no gideon dependency.

```
cargo install --path .          # desktop
cross build --release --target armv7-unknown-linux-musleabihf   # Kobo
```

archive.org's OPDS feed (BookServer) is gone. Its JSON search and metadata
endpoints are not, and they serve direct EPUB downloads for everything that
isn't in the lending library — which is what this uses. Lending-library and
access-restricted items are filtered out: a reader cannot borrow.

```
archivist search "slaughterhouse five"            # title/author, most downloaded first
archivist search --anywhere --pdf dresden         # full text, PDFs allowed
archivist popular                                 # the most downloaded free EPUBs
archivist show slaughterhouse-five-kurt-vonnegut  # metadata + files
archivist get slaughterhouse-five-kurt-vonnegut --out /mnt/onboard/Books
```

`get` writes `Author/Title.epub` (the layout gideon's Books tab and Nickel
both read), through a temp file, and refuses a response that is not the
format it asked for (archive.org answers with an HTML page when throttling).

## Library

```rust
let client = archivist::Client::new();
let page = client.search("vonnegut", &archivist::SearchOptions::default())?;
let item = client.item(&page.hits[0].identifier)?;
let epub = item.best_epub().expect("search only returns items with one");
let (path, _existed) = client.download(&item, epub, std::path::Path::new("Books"))?;
```

All network access goes through the `Fetcher` trait; `FakeFetcher` serves
canned bodies for tests, and `tests/fixtures/` holds real captured responses.
