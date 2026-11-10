//! One search across every free catalogue.
//!
//! A reader types a title once. [`Client::find`] asks Project Gutenberg
//! (through the Gutendex API), Open Library and the Internet Archive, then
//! merges the answers: the same book from two places becomes one result,
//! and the best copy comes first. "Best" prefers a title that matches what
//! was typed, then the source whose EPUBs read well — Gutenberg's are
//! proofread text, the Archive's are mostly raw scans — then popularity.
//!
//! A catalogue that is down does not sink the search: its failure is
//! reported beside whatever the others found.

use std::path::{Path, PathBuf};

use serde_json::Value;
use url::Url;

use crate::{book_path, Client, Error, Fetcher, Result, SearchOptions};

pub const GUTENDEX_BASE: &str = "https://gutendex.com";
pub const OPEN_LIBRARY_BASE: &str = "https://openlibrary.org";
pub const GUTENBERG_BASE: &str = "https://www.gutenberg.org";

/// Where a result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    Gutenberg,
    OpenLibrary,
    InternetArchive,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Gutenberg => "Project Gutenberg",
            Source::OpenLibrary => "Open Library",
            Source::InternetArchive => "Internet Archive",
        }
    }

    /// How well this source's EPUBs tend to read; higher first.
    fn quality(self) -> u8 {
        match self {
            Source::Gutenberg => 2,
            Source::OpenLibrary => 1,
            Source::InternetArchive => 0,
        }
    }
}

/// One book, from whichever catalogue has the best copy of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub source: Source,
    pub title: String,
    pub author: Option<String>,
    pub year: Option<String>,
    pub language: Option<String>,
    /// The source's own popularity count (downloads), for ordering.
    pub popularity: u64,
    /// A direct EPUB link, when the catalogue gives one (Gutenberg).
    pub epub: Option<Url>,
    /// Internet Archive items that may hold the EPUB, best guess first
    /// (the Archive, and Open Library's public scans).
    pub archive_ids: Vec<String>,
}

impl Found {
    /// "Title — Author (year)" for a list row.
    pub fn label(&self) -> String {
        let mut s = self.title.clone();
        if let Some(a) = &self.author {
            s.push_str(" — ");
            s.push_str(a);
        }
        if let Some(y) = &self.year {
            s.push_str(&format!(" ({y})"));
        }
        s
    }

    /// The book, ignoring edition noise: lowercased main title (before a
    /// subtitle) and the author's last name.
    fn key(&self) -> (String, String) {
        let main = self
            .title
            .split([';', ':', '(', '['])
            .next()
            .unwrap_or(&self.title);
        let surname = self
            .author
            .as_deref()
            .and_then(|a| a.split(',').next())
            .and_then(|a| a.split_whitespace().last())
            .map(words)
            .unwrap_or_default();
        (words(main), surname)
    }
}

/// What a unified search found, and which catalogues could not answer.
#[derive(Debug, Default)]
pub struct FindReport {
    pub results: Vec<Found>,
    pub failed: Vec<(Source, Error)>,
}

/// Lowercase letters and digits, single-spaced; leading articles dropped.
fn words(s: &str) -> String {
    let lowered: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    let mut parts: Vec<&str> = lowered.split_whitespace().collect();
    if matches!(parts.first(), Some(&("the" | "a" | "an"))) && parts.len() > 1 {
        parts.remove(0);
    }
    parts.join(" ")
}

/// How well a result answers the query: 4 the title is the query, 3 the
/// query appears as a phrase in the title or author, 2 every query word is
/// there somewhere, 1 some are, 0 none.
fn relevance(query: &str, found: &Found) -> u8 {
    let q = words(query);
    let (main, _) = found.key();
    let whole = words(&format!(
        "{} {}",
        found.title,
        found.author.as_deref().unwrap_or("")
    ));
    if q.is_empty() {
        return 0;
    }
    if main == q || words(&found.title) == q {
        return 4;
    }
    if format!(" {whole} ").contains(&format!(" {q} ")) {
        return 3;
    }
    let have: Vec<&str> = whole.split(' ').collect();
    let hits = q.split(' ').filter(|w| have.contains(w)).count();
    match hits {
        n if n == q.split(' ').count() => 2,
        0 => 0,
        _ => 1,
    }
}

/// "Shelley, Mary Wollstonecraft" → "Mary Wollstonecraft Shelley".
fn natural_name(name: &str) -> String {
    match name.split_once(',') {
        Some((last, first)) if !first.trim().is_empty() => {
            format!("{} {}", first.trim(), last.trim())
        }
        _ => name.trim().to_string(),
    }
}

/// Gutendex's `/books/?search=` answer.
pub fn parse_gutendex(body: &[u8]) -> std::result::Result<Vec<Found>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let results = v["results"].as_array().ok_or("no results array")?;
    Ok(results
        .iter()
        .filter_map(|r| {
            let title = r["title"].as_str()?.trim().to_string();
            let epub = r["formats"]["application/epub+zip"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())?;
            let authors: Vec<String> = r["authors"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x["name"].as_str())
                        .map(natural_name)
                        .collect()
                })
                .unwrap_or_default();
            Some(Found {
                source: Source::Gutenberg,
                title,
                author: (!authors.is_empty()).then(|| authors.join(", ")),
                year: None,
                language: r["languages"][0].as_str().map(str::to_string),
                popularity: r["download_count"].as_u64().unwrap_or(0),
                epub: Some(epub),
                archive_ids: Vec::new(),
            })
        })
        .collect())
}

/// gutenberg.org's own OPDS search feed, the fallback when Gutendex is
/// down. Book entries are the ones whose id is `/ebooks/<number>.opds`;
/// the title and author are plain text, and the EPUB sits at a fixed
/// address for that number. Read with string scanning: the feed is flat
/// and regular, and this crate carries no XML parser.
pub fn parse_gutenberg_opds(body: &[u8], base: &Url) -> std::result::Result<Vec<Found>, String> {
    let text = String::from_utf8_lossy(body);
    if !text.contains("<feed") {
        return Err("not an OPDS feed".into());
    }
    let between = |hay: &str, open: &str, close: &str| -> Option<String> {
        let start = hay.find(open)?;
        let after = &hay[start..];
        let from = after.find('>')? + 1;
        let to = after[from..].find(close)?;
        Some(unescape(after[from..from + to].trim()))
    };
    Ok(text
        .split("<entry>")
        .skip(1)
        .filter_map(|entry| {
            let entry = entry.split("</entry>").next()?;
            let id = between(entry, "<id", "</id>")?;
            let number = id
                .rsplit('/')
                .next()?
                .strip_suffix(".opds")
                .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))?
                .to_string();
            let title = between(entry, "<title", "</title>")?;
            let author = between(entry, "<content", "</content>").filter(|a| !a.is_empty());
            let epub = base.join(&format!("ebooks/{number}.epub3.images")).ok()?;
            Some(Found {
                source: Source::Gutenberg,
                title,
                author,
                year: None,
                language: None,
                popularity: 0,
                epub: Some(epub),
                archive_ids: Vec::new(),
            })
        })
        .collect())
}

/// The five XML entities a feed's text can carry.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Open Library's `search.json` answer: only works whose scans are public
/// (not the borrow-only ones), each pointing at its Archive items.
pub fn parse_open_library(body: &[u8]) -> std::result::Result<Vec<Found>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let docs = v["docs"].as_array().ok_or("no docs array")?;
    Ok(docs
        .iter()
        .filter(|d| d["ebook_access"].as_str() == Some("public"))
        .filter_map(|d| {
            let title = d["title"].as_str()?.trim().to_string();
            let ids: Vec<String> = d["ia"]
                .as_array()?
                .iter()
                .filter_map(|x| x.as_str())
                .take(6)
                .map(str::to_string)
                .collect();
            if ids.is_empty() {
                return None;
            }
            let authors: Vec<&str> = d["author_name"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                .unwrap_or_default();
            Some(Found {
                source: Source::OpenLibrary,
                title,
                author: (!authors.is_empty()).then(|| authors.join(", ")),
                // Open Library writes an unknown year as 0.
                year: d["first_publish_year"]
                    .as_u64()
                    .filter(|y| *y > 0)
                    .map(|y| y.to_string()),
                language: None,
                popularity: 0,
                epub: None,
                archive_ids: ids,
            })
        })
        .collect())
}

/// Merge every catalogue's answers: best first, one result per book.
pub fn merge(query: &str, mut all: Vec<Found>, limit: usize) -> Vec<Found> {
    all.sort_by(|a, b| {
        relevance(query, b)
            .cmp(&relevance(query, a))
            .then(b.source.quality().cmp(&a.source.quality()))
            .then(b.popularity.cmp(&a.popularity))
    });
    // Partial matches are only worth showing when nothing matches fully:
    // "the time machine" should not list a book about sewing machines.
    let best = all.iter().map(|f| relevance(query, f)).max().unwrap_or(0);
    let floor = best.clamp(1, 3);
    let mut out: Vec<Found> = Vec::new();
    for f in all {
        if relevance(query, &f) < floor {
            continue;
        }
        match out.iter_mut().find(|o| o.key() == f.key()) {
            // The same book again: keep the better copy, remember the
            // other's Archive items as fallbacks.
            Some(kept) => {
                for id in f.archive_ids {
                    if !kept.archive_ids.contains(&id) {
                        kept.archive_ids.push(id);
                    }
                }
                kept.year = kept.year.take().or(f.year);
            }
            None => out.push(f),
        }
    }
    out.truncate(limit);
    out
}

impl FindReport {
    /// Whether no catalogue answered at all (as opposed to answering
    /// "nothing"): the caller is offline, or everything is down.
    pub fn all_failed(&self) -> bool {
        self.results.is_empty() && self.failed.len() >= 3
    }
}

impl<F: Fetcher> Client<F> {
    /// Point the Gutenberg search at another host (tests).
    pub fn with_gutendex_base(mut self, base: Url) -> Self {
        self.gutendex = base;
        self
    }

    /// Point the Open Library search at another host (tests).
    pub fn with_open_library_base(mut self, base: Url) -> Self {
        self.open_library = base;
        self
    }

    pub fn gutendex_url(&self, query: &str) -> Result<Url> {
        let mut url = self.gutendex.join("books/")?;
        url.query_pairs_mut().append_pair("search", query.trim());
        Ok(url)
    }

    /// Point the gutenberg.org fallback at another host (tests).
    pub fn with_gutenberg_base(mut self, base: Url) -> Self {
        self.gutenberg = base;
        self
    }

    pub fn gutenberg_opds_url(&self, query: &str) -> Result<Url> {
        let mut url = self.gutenberg.join("ebooks/search.opds/")?;
        url.query_pairs_mut().append_pair("query", query.trim());
        Ok(url)
    }

    pub fn open_library_url(&self, query: &str) -> Result<Url> {
        let mut url = self.open_library.join("search.json")?;
        url.query_pairs_mut()
            .append_pair("q", query.trim())
            .append_pair("has_fulltext", "true")
            .append_pair("limit", "10")
            .append_pair(
                "fields",
                "title,author_name,first_publish_year,ia,ebook_access",
            );
        Ok(url)
    }

    /// Search every catalogue for `query` and merge the answers.
    pub fn find(&self, query: &str, limit: usize) -> FindReport {
        let mut report = FindReport::default();
        let mut all = Vec::new();

        let gutenberg = self.gutendex_url(query).and_then(|url| {
            let body = self.fetcher.get(&url)?;
            parse_gutendex(&body).map_err(|message| Error::Parse {
                url: url.to_string(),
                message,
            })
        });
        // Gutendex is a volunteer mirror of Gutenberg's catalogue and is
        // often down; gutenberg.org's own search answers the same question.
        let gutenberg = gutenberg.or_else(|_| {
            let url = self.gutenberg_opds_url(query)?;
            let body = self.fetcher.get(&url)?;
            parse_gutenberg_opds(&body, &self.gutenberg).map_err(|message| Error::Parse {
                url: url.to_string(),
                message,
            })
        });
        match gutenberg {
            Ok(found) => all.extend(found),
            Err(e) => report.failed.push((Source::Gutenberg, e)),
        }

        let open_library = self.open_library_url(query).and_then(|url| {
            let body = self.fetcher.get(&url)?;
            parse_open_library(&body).map_err(|message| Error::Parse {
                url: url.to_string(),
                message,
            })
        });
        match open_library {
            Ok(found) => all.extend(found),
            Err(e) => report.failed.push((Source::OpenLibrary, e)),
        }

        let opts = SearchOptions {
            rows: 15,
            ..Default::default()
        };
        match self.search(query, &opts) {
            // archive.org's format filter counts derivatives, so check the
            // hit itself: a scan with only a PDF is not a book to offer.
            Ok(page) => all.extend(
                page.hits
                    .into_iter()
                    .filter(|h| h.has_epub())
                    .map(|h| Found {
                        source: Source::InternetArchive,
                        title: h.title,
                        author: h.creator,
                        year: h.year,
                        language: h.language,
                        popularity: h.downloads,
                        epub: None,
                        archive_ids: vec![h.identifier],
                    }),
            ),
            Err(e) => report.failed.push((Source::InternetArchive, e)),
        }

        report.results = merge(query, all, limit);
        report
    }

    /// Download a found book's EPUB into `dir` as `Author/Title.epub`.
    /// Returns the path and whether it was already there. A direct link is
    /// used when there is one; otherwise the Archive items are tried in
    /// order until one has an EPUB.
    pub fn grab(&self, found: &Found, dir: &Path) -> Result<(PathBuf, bool)> {
        if let Some(url) = &found.epub {
            let dest = book_path(dir, found.author.as_deref(), &found.title, "epub");
            if dest.exists() {
                return Ok((dest, true));
            }
            let bytes = self.fetcher.get(url)?;
            if !bytes.starts_with(b"PK") {
                return Err(Error::NotTheFile {
                    expected: "an EPUB",
                });
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = dest.with_extension("epub.part");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &dest)?;
            return Ok((dest, false));
        }
        let mut last = Error::NoFile {
            identifier: found.title.clone(),
            what: "EPUB",
        };
        for id in &found.archive_ids {
            let item = match self.item(id) {
                Ok(item) => item,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            match item.best_epub() {
                Some(file) => match self.download(&item, file, dir) {
                    Ok(done) => return Ok(done),
                    Err(e) => last = e,
                },
                None => {
                    last = Error::NoFile {
                        identifier: id.clone(),
                        what: "EPUB",
                    }
                }
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeFetcher;

    const GUTENDEX: &str = r#"{"count":2,"results":[
      {"id":35,"title":"The Time Machine","authors":[{"name":"Wells, H. G. (Herbert George)"}],"languages":["en"],"download_count":9000,
       "formats":{"application/epub+zip":"https://www.gutenberg.org/ebooks/35.epub3.images","text/html":"https://x/35.html"}},
      {"id":99,"title":"Audio only","authors":[],"languages":["en"],"download_count":5,"formats":{"audio/mpeg":"https://x/a.mp3"}}]}"#;
    const OPEN_LIBRARY: &str = r#"{"numFound":3,"docs":[
      {"title":"The Time Machine","author_name":["H. G. Wells"],"first_publish_year":1895,"ebook_access":"public","ia":["timemachine00well","timemachine01well"]},
      {"title":"The Time Machine Did It","author_name":["John Swartzwelder"],"first_publish_year":2004,"ebook_access":"borrowable","ia":["timemachinedidit"]},
      {"title":"The Chronic Argonauts","author_name":["H. G. Wells"],"ebook_access":"public","ia":["chronicargonauts"]}]}"#;
    const ARCHIVE: &str = r#"{"response":{"numFound":2,"start":0,"docs":[
      {"identifier":"sewing1920","title":"The sewing machine: from its inception to the present time","creator":"Lyons","downloads":50,"format":["EPUB"]},
      {"identifier":"timemachine_scan","title":"The time machine","creator":"Wells, H. G.","downloads":700,"format":["EPUB"]}]}}"#;

    fn client() -> Client<FakeFetcher> {
        let probe = Client::with_fetcher(FakeFetcher::new());
        let q = "the time machine";
        let opts = SearchOptions {
            rows: 15,
            ..Default::default()
        };
        let fetcher = FakeFetcher::new()
            .with(probe.gutendex_url(q).unwrap().as_str(), GUTENDEX)
            .with(probe.open_library_url(q).unwrap().as_str(), OPEN_LIBRARY)
            .with(probe.search_url(q, &opts).unwrap().as_str(), ARCHIVE)
            .with(
                "https://www.gutenberg.org/ebooks/35.epub3.images",
                b"PK\x03\x04 an epub".to_vec(),
            );
        Client::with_fetcher(fetcher)
    }

    #[test]
    fn one_search_merges_the_catalogues_best_copy_first() {
        let report = client().find("the time machine", 10);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let rows: Vec<(Source, &str)> = report
            .results
            .iter()
            .map(|f| (f.source, f.title.as_str()))
            .collect();
        // One row for the book: Gutenberg's copy, with the other
        // catalogues' Archive items kept as fallbacks. The borrow-only
        // work, the audio-only record, and the books that merely contain
        // the words are not offered.
        assert_eq!(rows, [(Source::Gutenberg, "The Time Machine")]);
        let top = &report.results[0];
        assert_eq!(top.author.as_deref(), Some("H. G. (Herbert George) Wells"));
        assert_eq!(
            top.year.as_deref(),
            Some("1895"),
            "the year comes from Open Library"
        );
        assert_eq!(
            top.archive_ids,
            ["timemachine00well", "timemachine01well", "timemachine_scan"]
        );
    }

    #[test]
    fn a_catalogue_that_is_down_does_not_sink_the_search() {
        let probe = Client::with_fetcher(FakeFetcher::new());
        let fetcher = FakeFetcher::new().with(
            probe.open_library_url("the time machine").unwrap().as_str(),
            OPEN_LIBRARY,
        );
        let report = Client::with_fetcher(fetcher).find("the time machine", 10);
        let failed: Vec<Source> = report.failed.iter().map(|f| f.0).collect();
        assert_eq!(failed, [Source::Gutenberg, Source::InternetArchive]);
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].source, Source::OpenLibrary);
    }

    #[test]
    fn grab_saves_the_direct_epub_as_author_title_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let c = client();
        let report = c.find("the time machine", 10);
        let (path, existed) = c.grab(&report.results[0], dir.path()).unwrap();
        assert!(!existed);
        assert_eq!(
            path,
            dir.path()
                .join("H. G. (Herbert George) Wells/The Time Machine.epub")
        );
        assert!(std::fs::read(&path).unwrap().starts_with(b"PK"));
        let (_, existed) = c.grab(&report.results[0], dir.path()).unwrap();
        assert!(existed, "the second time nothing is fetched");
    }

    #[test]
    fn gutenberg_org_answers_when_gutendex_is_down() {
        let feed = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
<entry><id>https://www.gutenberg.org/ebooks/subjects/search.opds/?query=x</id><title>Subjects</title><content type="text">One subject</content></entry>
<entry><updated>2026</updated><id>https://www.gutenberg.org/ebooks/35.opds</id><title>The Time Machine</title><content type="text">H. G. Wells</content></entry>
<entry><id>https://www.gutenberg.org/ebooks/1234.opds</id><title>Tom &amp; Jerry&apos;s Time Machine</title><content type="text"></content></entry>
</feed>"#;
        let base = Url::parse(GUTENBERG_BASE).unwrap();
        let found = parse_gutenberg_opds(feed.as_bytes(), &base).unwrap();
        assert_eq!(found.len(), 2, "the Subjects row is not a book");
        assert_eq!(found[0].title, "The Time Machine");
        assert_eq!(found[0].author.as_deref(), Some("H. G. Wells"));
        assert_eq!(
            found[0].epub.as_ref().unwrap().as_str(),
            "https://www.gutenberg.org/ebooks/35.epub3.images"
        );
        assert_eq!(found[1].title, "Tom & Jerry's Time Machine");
        assert_eq!(found[1].author, None);
        assert!(parse_gutenberg_opds(b"<html>busy</html>", &base).is_err());

        // In a search: Gutendex has no canned answer (down), the feed does.
        let probe = Client::with_fetcher(FakeFetcher::new());
        let fetcher = FakeFetcher::new().with(
            probe
                .gutenberg_opds_url("the time machine")
                .unwrap()
                .as_str(),
            feed,
        );
        let report = Client::with_fetcher(fetcher).find("the time machine", 10);
        assert_eq!(report.results[0].source, Source::Gutenberg);
        assert_eq!(report.results[0].title, "The Time Machine");
        assert!(!report.failed.iter().any(|f| f.0 == Source::Gutenberg));
    }

    #[test]
    fn relevance_ranks_title_then_phrase_then_words() {
        let f = |title: &str, author: Option<&str>| Found {
            source: Source::InternetArchive,
            title: title.into(),
            author: author.map(str::to_string),
            year: None,
            language: None,
            popularity: 0,
            epub: None,
            archive_ids: vec![],
        };
        assert_eq!(
            relevance("The Time Machine", &f("The time machine", None)),
            4
        );
        assert_eq!(
            relevance("moby dick", &f("Moby Dick; Or, The Whale", None)),
            4
        );
        assert_eq!(relevance("jane austen", &f("Emma", Some("Jane Austen"))), 3);
        assert_eq!(
            relevance("time machine", &f("The sewing machine in our time", None)),
            2
        );
        assert_eq!(relevance("time machine", &f("Machine knitting", None)), 1);
        assert_eq!(relevance("time machine", &f("Emma", None)), 0);
        assert_eq!(
            natural_name("Shelley, Mary Wollstonecraft"),
            "Mary Wollstonecraft Shelley"
        );
        assert_eq!(natural_name("Homer"), "Homer");
    }
}
