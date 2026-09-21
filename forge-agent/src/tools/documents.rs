// SPDX-License-Identifier: Apache-2.0
//! `search_documents` — ranked retrieval over a directory of documents.
//!
//! The same engine as `web_search`, pointed at a disk instead of a network.
//! That is the whole idea: an inverted index does not care where the words
//! came from, and the corpus most people actually have is a folder of reports,
//! notes, papers or exported logs rather than a website.
//!
//! ## Sections, not files
//!
//! A file is indexed as the sections it is made of, each keyed by the lines it
//! occupies — `file:///path/to/runbook.md#L120-186`. That is what makes a
//! result worth having to an agent: the passage is named by the headings
//! leading to it, and the range is what `read_file` takes, so getting the rest
//! of the section costs sixty lines rather than the whole file. Context is the
//! budget, and spending three thousand lines of it to reach forty is the thing
//! retrieval was supposed to avoid. See `forge_search::document`.
//!
//! ## Why this is not `search_code`
//!
//! `search_code` greps, and for source that is the better tool. An identifier
//! is an exact string; a regex over exact strings is precise, needs no index,
//! and cannot go stale. Ranked retrieval earns its place on a different
//! question — *which* of three thousand documents answers this — which grep
//! cannot answer at all: it returns every file containing the word, in
//! whatever order the filesystem offered them, and it misses the document that
//! says "Windows Management Instrumentation" when the query said "WMI".
//!
//! So this is for prose at a scale where reading the matches is not an option.
//! For a fifty-file repository, `search_code` and `read_file` are better and
//! this tool should not be reached for.
//!
//! ## Why a separate index from the crawled one
//!
//! Because BM25 scores a term by how rare it is *in the corpus being
//! searched*, so what else is in the index changes every score. Five hundred
//! crawled pages of a vendor's documentation, where "bucket" appears on four
//! hundred of them, make "bucket" worthless as a discriminator — and then
//! three of your own incident reports that mention a bucket are scored as
//! though the word meant nothing. Kept apart, "bucket" is on three of fifty
//! documents, which is rare, and those three win outright.
//!
//! Length normalisation has the same problem in the other direction: BM25
//! marks down a document that is short *for its corpus*, so mixing
//! two-thousand-term reference pages with two-hundred-term notes penalises the
//! notes for being the length that notes are.

use anyhow::{Context, Result};
use forge_search::document;
use forge_search::index::Index;
use forge_search::query;

/// How many passages to return.
///
/// Not a parameter, for the reason given on `search::MAX_RESULTS`: a knob the
/// model has no basis for choosing is a knob it sets arbitrarily.
const MAX_RESULTS: usize = 5;

/// Largest single file read.
///
/// A log can be gigabytes and a document cannot. Past this the file is skipped
/// rather than truncated, since half a document indexes as a document and
/// would then answer for the whole of it.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Most files indexed in one call.
///
/// A bound on the surprise rather than on the corpus: somebody who points this
/// at their home directory should get a result and a note, not a wait of
/// unknown length. Reached, the call says so and says what it stopped at.
const MAX_FILES_PER_CALL: usize = 5_000;

/// Directory names the walk does not descend into on its own.
///
/// A heuristic, and worth saying so. It is also the difference between working
/// and not: this repository's `target/` is 83 GB across 663,234 files and
/// holds ten readable documents, and walking it took thirteen seconds of the
/// thirteen and a half a real run spent — almost all of it a `symlink_metadata`
/// syscall per entry, to reach nothing anybody asked for.
///
/// Build output and vendored dependencies, then, by the names every ecosystem
/// happens to use. `build` and `dist` are the arguable ones, since they are
/// ordinary English words that a person's documents could reasonably live
/// under — so this only applies to directories the walk *discovers*. A
/// directory named in `paths` is read whatever it is called, which makes the
/// guess a default rather than a rule.
const NOT_DOCUMENTS: &[&str] = &[
    "target", "node_modules", "vendor", "__pycache__", "venv", "build", "dist",
];

/// How deep to walk below the given directory.
///
/// Deep enough for a documents tree organised by year and topic, shallow
/// enough that pointing at a home directory does not descend into everything
/// ever installed.
const MAX_DEPTH: usize = 8;

/// Where the document index lives.
pub fn index_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root.join(".forge").join("doc-index")
}

pub async fn search_documents(
    args: &serde_json::Value,
    project_root: std::path::PathBuf,
) -> Result<String> {
    let query_text = args["query"]
        .as_str()
        .context("Missing 'query' argument")?
        .to_string();
    let paths: Vec<String> = args["paths"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let index_path = index_path(&project_root);
    // Reading and tokenising thousands of files is blocking work, and doing it
    // on a runtime worker thread stalls every other task in the process.
    tokio::task::spawn_blocking(move || run(&query_text, &paths, &project_root, index_path))
        .await
        .map_err(|e| anyhow::anyhow!("document search task failed: {e}"))?
}

/// What one call did.
#[derive(Debug, Default)]
struct Indexed {
    /// Files read and added this call.
    added: usize,
    /// Sections those files were indexed as.
    sections: usize,
    /// Files already in the index and unchanged since.
    unchanged: usize,
    /// Files skipped for being too large, with the largest seen.
    too_large: usize,
    /// Files whose extension is readable but whose content is not text.
    not_text: usize,
    /// Files that could not be read at all — permissions, a broken link.
    unreadable: usize,
    /// Whether the per-call file cap was reached.
    truncated: bool,
    /// Directories named that do not exist.
    missing: Vec<String>,
    /// Build and dependency directories the walk declined to descend into,
    /// by name and without repeats.
    ///
    /// Reported rather than skipped silently: a corpus that turned out to be
    /// missing a third of itself because it lives under `dist/` should be a
    /// visible fact, not a mystery about the ranking.
    skipped: Vec<String>,
}

fn run(
    query_text: &str,
    paths: &[String],
    project_root: &std::path::Path,
    index_path: std::path::PathBuf,
) -> Result<String> {
    // A corrupt index is a cache of files that can be read again, so it is
    // replaced rather than fatal.
    let mut index = Index::load(&index_path).unwrap_or_else(|_| Index::new());

    let mut report = Indexed::default();
    if !paths.is_empty() {
        // Which sections belong to which file, built once. A file is many
        // documents now, so the alternative is a scan of every URL per file —
        // three thousand files against three thousand sections is nine million
        // string comparisons to answer a question asked once.
        let mut held = sections_by_file(&index);
        for path in paths {
            let root = resolve(project_root, path);
            if !root.exists() {
                report.missing.push(path.clone());
                continue;
            }
            walk(&root, 0, &mut index, &mut held, &mut report);
            if report.truncated {
                break;
            }
        }
        if report.added > 0 {
            let _ = crate::workdir::ensure_parent_of(&index_path);
            let _ = index.save(&index_path);
        }
    }

    let hits = query::search(&index, query_text, MAX_RESULTS);
    Ok(render(query_text, &hits, &index, paths, &report))
}

/// A path as given, against the project root when it is relative.
fn resolve(project_root: &std::path::Path, given: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(given);
    let joined = if path.is_absolute() { path.to_path_buf() } else { project_root.join(path) };
    tidy(&joined)
}

/// A path with its `.` components dropped.
///
/// `paths: ["."]` is the ordinary way to say "this project", and joining it
/// produces `/project/./notes/a.md` — which works, and then appears in a
/// result and in a `read_file` call the agent is being told to make. Not
/// `canonicalize`, which also resolves symlinks: the walk deliberately does not
/// follow those, and a path that came back pointing somewhere else would
/// contradict it.
fn tidy(path: &std::path::Path) -> std::path::PathBuf {
    path.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// Every indexed section, grouped by the file it came from.
///
/// The key is the path out of a `file://` URL with its `#L…` range removed,
/// which is the identity of the file rather than of one section of it.
fn sections_by_file(index: &Index) -> std::collections::HashMap<String, Vec<u32>> {
    let mut out: std::collections::HashMap<String, Vec<u32>> = std::collections::HashMap::new();
    for id in 0..index.len_including_dead() as u32 {
        let Some(doc) = index.document(id) else { continue };
        let Some(rest) = doc.url.strip_prefix("file://") else { continue };
        let file = rest.split('#').next().unwrap_or(rest);
        out.entry(file.to_string()).or_default().push(id);
    }
    out
}

/// Read every readable document below `dir`, adding what has changed.
fn walk(
    dir: &std::path::Path,
    depth: usize,
    index: &mut Index,
    held: &mut std::collections::HashMap<String, Vec<u32>>,
    report: &mut Indexed,
) {
    if report.truncated {
        return;
    }
    if dir.is_file() {
        ingest(dir, index, held, report);
        return;
    }
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        report.unreadable += 1;
        return;
    };

    // Sorted, so two runs over the same tree assign ids in the same order and
    // a result set is reproducible. `read_dir` order is the filesystem's and
    // is not stable between machines or after a file is rewritten.
    let mut children: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();

    for child in children {
        if report.truncated {
            return;
        }
        let Some(name) = child.file_name().and_then(|n| n.to_str()) else { continue };
        // Dot directories and dot files. `.git` alone can hold more objects
        // than the corpus, and `.forge` holds the index this is writing to.
        if name.starts_with('.') {
            continue;
        }
        // Build output and vendored dependencies — see `NOT_DOCUMENTS`. Only
        // for directories found by walking; one named in `paths` is read.
        if child.is_dir() && NOT_DOCUMENTS.contains(&name) {
            if !report.skipped.iter().any(|s| s == name) {
                report.skipped.push(name.to_string());
            }
            continue;
        }
        // Symlinks are not followed, which is the cheap way to be sure a walk
        // terminates: one link back to an ancestor turns the tree into a cycle.
        if std::fs::symlink_metadata(&child).map(|m| m.is_symlink()).unwrap_or(false) {
            continue;
        }
        if child.is_dir() {
            walk(&child, depth + 1, index, held, report);
        } else if document::is_readable(&child) {
            ingest(&child, index, held, report);
        }
    }
}

/// Add one file's sections, unless it is already there and unchanged.
fn ingest(
    path: &std::path::Path,
    index: &mut Index,
    held: &mut std::collections::HashMap<String, Vec<u32>>,
    report: &mut Indexed,
) {
    if report.added + report.unchanged >= MAX_FILES_PER_CALL {
        report.truncated = true;
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        report.unreadable += 1;
        return;
    };
    if meta.len() > MAX_FILE_BYTES {
        report.too_large += 1;
        return;
    }

    let key = path.display().to_string();
    let existing = held.get(&key).cloned().unwrap_or_default();

    // Already indexed and untouched since. Skipping matters more than it
    // looks: re-adding a document marks the old one dead, and enough dead
    // documents trigger a full rewrite of the index — so re-indexing an
    // unchanged tree would rewrite the whole thing to produce exactly what was
    // already in it.
    if !existing.is_empty() {
        let read_at = existing.iter().map(|&id| index.read_time(id)).min().unwrap_or(0);
        let changed = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() > read_at)
            // No mtime is no evidence of freshness, so re-read.
            .unwrap_or(true);
        if !changed && read_at > 0 {
            report.unchanged += 1;
            return;
        }
    }

    let Ok(bytes) = std::fs::read(path) else {
        report.unreadable += 1;
        return;
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let Some(document) = document::read(&bytes, name, extension) else {
        report.not_text += 1;
        return;
    };

    // The old sections go first, and all of them, rather than relying on each
    // new one replacing its predecessor by URL. An edit moves the line ranges
    // and can rename or delete a heading, so the URLs do not line up: without
    // this, editing a document would leave its previous sections in the index
    // answering from text that is no longer in the file.
    for id in existing {
        index.remove(id);
    }

    let mut added = Vec::new();
    for section in &document.sections {
        let url = section_url(path, section);
        let title = match section.title() {
            trail if trail.is_empty() => document.title.clone(),
            trail => trail,
        };
        added.push(index.add(&url, &title, "", &section.text));
        report.sections += 1;
    }
    held.insert(key, added);
    report.added += 1;
}

/// A URL for one section: the file, then the lines it occupies.
///
/// The range is the identity *and* the instruction. It makes two sections of
/// one file distinct documents, and it is exactly what `read_file` takes — so
/// a result says where the passage is in a form the next call can use, rather
/// than naming a file and leaving the agent to read all of it.
///
/// Sections with no line numbers — HTML, whose parser collapses markup — are
/// numbered by their position instead, which distinguishes them without
/// claiming to locate them.
fn section_url(path: &std::path::Path, section: &forge_search::document::Section) -> String {
    let file = path.display().to_string().replace(' ', "%20");
    match section.lines {
        Some((first, last)) => format!("file://{file}#L{first}-{last}"),
        None => format!("file://{file}#p{}", section.text.len()),
    }
}

/// The tool result the model sees.
fn render(
    query_text: &str,
    hits: &[query::Result_],
    index: &Index,
    paths: &[String],
    report: &Indexed,
) -> String {
    let mut out = String::new();

    if hits.is_empty() {
        out.push_str(&format!("No results for {query_text:?}.\n"));
        render_unanswered(&mut out, index, query_text);
        if index.is_empty() {
            out.push_str(
                "No documents have been indexed for this project. Pass `paths` with a \
                 directory of documents to read — prose formats only: .md, .txt, .rst, \
                 .org, .html. For source code use search_code, which greps and needs no \
                 index.\n",
            );
        } else if paths.is_empty() {
            out.push_str(&format!(
                "{} section(s) are indexed and none matched. Try different terms, or pass \
                 `paths` to read somewhere new.\n",
                index.len(),
            ));
        } else {
            out.push_str(&format!(
                "{} section(s) are indexed and none matched.\n",
                index.len(),
            ));
        }
        render_indexing(&mut out, report);
        return out;
    }

    out.push_str(&format!("{} result(s) for {query_text:?}:\n\n", hits.len()));
    for (i, hit) in hits.iter().enumerate() {
        let title = if hit.title.is_empty() { &hit.url } else { &hit.title };
        out.push_str(&format!("{}. {}\n", i + 1, title));
        // The path and the line range, phrased as the next call rather than as
        // a location. The range is the point of sectioning: reading sixty lines
        // to get the rest of a passage is a different proposition from reading
        // three thousand, and an agent that has to work out the arguments will
        // often just read the file.
        let (path, lines) = split_location(&hit.url);
        match lines {
            Some((first, last)) => out.push_str(&format!(
                "   {path} lines {first}-{last}   read_file(path=\"{path}\", \
                 start_line={first}, end_line={last}) for the whole section\n",
            )),
            None => out.push_str(&format!("   {path}\n")),
        }
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
        out.push('\n');
    }
    render_unanswered(&mut out, index, query_text);
    render_indexing(&mut out, report);
    out.push_str(&format!(
        "[{} section(s) indexed across the documents read. Each result is one section, \
         not a whole file.]\n",
        index.len(),
    ));
    out
}

/// Query words no indexed document contains.
///
/// The one thing a result list cannot say about itself. A query whose
/// distinctive words are absent still matches on its grammar — `what oil does
/// a tractor take` against this corpus matched `what`, `does`, `a` and `take`
/// and returned sections about window management — and every number in the
/// result was then truthful and useless. Two words reported as blanks turn a
/// wrong answer into an obviously wrong one.
fn render_unanswered(out: &mut String, index: &Index, query_text: &str) {
    let missing = forge_search::query::unanswered_terms(index, query_text);
    if missing.is_empty() {
        return;
    }
    out.push_str(&format!(
        "No indexed document contains {} — so nothing above was matched on {}. \
         Either the documents holding that have not been read yet, or they use \
         different words for it.\n",
        missing.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>().join(", "),
        if missing.len() == 1 { "it" } else { "them" },
    ));
}

/// What the indexing pass did, when it did anything worth saying.
///
/// Every line here is a reason a document the caller expected is absent. A
/// search that silently indexed nine files out of four thousand looks like a
/// search that found nothing, and the model's next move would be to rephrase
/// the query rather than to raise the real problem.
fn render_indexing(out: &mut String, report: &Indexed) {
    if !report.missing.is_empty() {
        out.push_str(&format!(
            "Not found, so nothing was read from them: {}\n",
            report.missing.join(", "),
        ));
    }
    if report.added > 0 || report.unchanged > 0 {
        out.push_str(&format!(
            "Read {} new or changed document(s) as {} section(s); {} already indexed and \
             unchanged.\n",
            report.added, report.sections, report.unchanged,
        ));
    }
    for (count, why) in [
        (report.too_large, "larger than 4 MB"),
        (report.not_text, "not text, despite the extension"),
        (report.unreadable, "could not be read"),
    ] {
        if count > 0 {
            out.push_str(&format!("Skipped {count} file(s): {why}.\n"));
        }
    }
    if report.truncated {
        out.push_str(&format!(
            "Stopped at {MAX_FILES_PER_CALL} files, so the corpus is incomplete — name a \
             narrower directory in `paths` to reach the rest.\n",
        ));
    }
    if !report.skipped.is_empty() {
        out.push_str(&format!(
            "Did not descend into {} (build output or dependencies by convention). If the \
             documents are in there, name that directory in `paths` directly.\n",
            report.skipped.join(", "),
        ));
    }
}

/// A section URL split back into the path and the lines, which is what a
/// person and `read_file` both want.
fn split_location(url: &str) -> (String, Option<(usize, usize)>) {
    let rest = url.strip_prefix("file://").unwrap_or(url);
    let (path, fragment) = match rest.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (rest, None),
    };
    let path = path.replace("%20", " ");
    let lines = fragment
        .and_then(|f| f.strip_prefix('L'))
        .and_then(|range| range.split_once('-'))
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    (path, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree named for the test, since tests run as threads of one
    /// process and a directory keyed on the process id has had two of them
    /// delete each other's fixtures.
    fn tree(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("forge-docs-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for (path, body) in files {
            let at = root.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(&at, body).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn search(root: &std::path::Path, query: &str, paths: &[&str]) -> String {
        let owned: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        run(query, &owned, root, root.join(".forge").join("doc-index")).unwrap()
    }

    #[test]
    fn a_directory_of_notes_becomes_searchable() {
        let root = tree("basic", &[
            ("notes/keys.md", "# Rotating the signing key\n\nRotate the signing key every year.\n"),
            ("notes/oncall.md", "# On-call\n\nEscalate to the platform team after ten minutes.\n"),
            ("notes/lunch.txt", "Sandwiches are in the fridge.\n"),
        ]);

        let out = search(&root, "rotating the signing key", &["notes"]);
        assert!(out.contains("Read 3 new or changed document(s)"), "{out}");
        assert!(out.contains("Rotating the signing key"), "{out}");
        // The path, not a file:// URL — this is what read_file wants.
        assert!(out.contains("notes/keys.md"), "{out}");
        assert!(!out.contains("file://"), "a URL leaked into the result: {out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Re-indexing an untouched tree must read nothing. This is not only
    /// about time: re-adding a file marks its old document dead, and enough
    /// dead documents rewrite the whole index — so a no-op call would rewrite
    /// the index to produce exactly what was in it.
    #[test]
    fn an_unchanged_tree_is_not_read_twice() {
        let root = tree("unchanged", &[
            ("a.md", "# Alpha\n\nThe alpha document.\n"),
            ("b.md", "# Beta\n\nThe beta document.\n"),
        ]);

        let first = search(&root, "alpha", &["."]);
        assert!(first.contains("Read 2 new or changed document(s)"), "{first}");

        let second = search(&root, "alpha", &["."]);
        assert!(
            second.contains("Read 0 new or changed document(s) as 0 section(s); 2 already indexed"),
            "the tree was re-read: {second}"
        );
        // And it still answers.
        assert!(second.contains("Alpha"), "{second}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A changed file is re-read, and the new text is what answers.
    #[test]
    fn a_changed_file_is_read_again() {
        let root = tree("changed", &[("spec.md", "# Spec\n\nThe limit is forty units.\n")]);
        let first = search(&root, "limit units", &["."]);
        assert!(first.contains("forty"), "{first}");

        // The stored read time has one-second resolution, so the edit is
        // forced past it rather than assumed to land after it.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(root.join("spec.md"), "# Spec\n\nThe limit is ninety units.\n").unwrap();

        let second = search(&root, "limit units", &["."]);
        assert!(second.contains("Read 1 new or changed document(s)"), "{second}");
        assert!(second.contains("ninety"), "the stale text answered: {second}");
        assert!(!second.contains("forty"), "both versions are live: {second}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dot directories are skipped — `.git` alone can hold more objects than
    /// the corpus, and `.forge` holds the index being written.
    #[test]
    fn dot_directories_are_not_walked() {
        let root = tree("dots", &[
            ("real.md", "# Real\n\nA real document about kestrels.\n"),
            (".git/COMMIT_EDITMSG", "kestrels everywhere\n"),
            (".hidden/notes.md", "# Hidden\n\nAlso kestrels.\n"),
        ]);
        let out = search(&root, "kestrels", &["."]);
        assert!(out.contains("Read 1 new or changed document(s)"), "{out}");
        assert!(out.contains("real.md"), "{out}");
        assert!(!out.contains(".git"), "{out}");
        assert!(!out.contains(".hidden"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only prose extensions. Source goes to search_code, and a CSV of fifty
    /// thousand rows is fifty thousand documents rather than one.
    #[test]
    fn only_prose_formats_are_read() {
        let root = tree("formats", &[
            ("readme.md", "# Readme\n\nThe widget tolerance is tight.\n"),
            ("main.rs", "// the widget tolerance is tight\nfn main() {}\n"),
            ("data.csv", "widget,tolerance\n1,tight\n"),
            ("config.json", "{\"widget\": \"tolerance tight\"}\n"),
        ]);
        let out = search(&root, "widget tolerance", &["."]);
        assert!(out.contains("Read 1 new or changed document(s)"), "{out}");
        assert!(out.contains("readme.md"), "{out}");
        assert!(!out.contains("main.rs"), "{out}");
        assert!(!out.contains("data.csv"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A named directory that does not exist has to be said out loud. Silently
    /// indexing nothing looks identical to finding nothing, and the model
    /// would rephrase the query instead of fixing the path.
    #[test]
    fn a_missing_directory_is_reported_rather_than_ignored() {
        let root = tree("missing", &[("a.md", "# A\n\nSomething about herons.\n")]);
        let out = search(&root, "herons", &[".", "does-not-exist"]);
        assert!(out.contains("Not found"), "{out}");
        assert!(out.contains("does-not-exist"), "{out}");
        // And the directory that does exist was still read.
        assert!(out.contains("herons") || out.contains("a.md"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty index should say what to do, not just that it found nothing.
    #[test]
    fn an_empty_index_says_how_to_fill_it() {
        let root = tree("empty", &[]);
        let out = search(&root, "anything", &[]);
        assert!(out.contains("No documents have been indexed"), "{out}");
        assert!(out.contains("search_code"), "it should point at the better tool: {out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file whose extension lies about its content must not have its bytes
    /// indexed as words.
    #[test]
    fn a_binary_file_with_a_text_extension_is_skipped() {
        let root = tree("binary", &[("real.md", "# Real\n\nAbout otters.\n")]);
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0u8; 512]);
        std::fs::write(root.join("image.txt"), &png).unwrap();

        let out = search(&root, "otters", &["."]);
        assert!(out.contains("Skipped 1 file(s): not text"), "{out}");
        assert!(out.contains("Read 1 new or changed document(s)"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A single file is a legitimate thing to point at, not only a directory.
    #[test]
    fn a_single_file_can_be_named() {
        let root = tree("onefile", &[
            ("wanted.md", "# Wanted\n\nA note about badgers.\n"),
            ("other.md", "# Other\n\nAnother note about badgers.\n"),
        ]);
        let out = search(&root, "badgers", &["wanted.md"]);
        assert!(out.contains("Read 1 new or changed document(s)"), "{out}");
        assert!(out.contains("wanted.md"), "{out}");
        assert!(!out.contains("other.md"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A path with a space in it has to survive being used as an identity and
    /// then shown back — the round trip is what the index keys on.
    #[test]
    fn a_path_with_a_space_round_trips() {
        let root = tree("spaces", &[("my notes/a file.md", "# Spaced\n\nAbout puffins.\n")]);
        let out = search(&root, "puffins", &["."]);
        assert!(out.contains("my notes/a file.md"), "{out}");
        assert!(!out.contains("%20"), "escaping leaked into the result: {out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Searching with no `paths` reads nothing and answers from what is there,
    /// which is the cheap repeat case.
    #[test]
    fn a_query_with_no_paths_reads_nothing() {
        let root = tree("norepeat", &[("a.md", "# A\n\nAbout wolverines.\n")]);
        search(&root, "wolverines", &["."]);

        let out = search(&root, "wolverines", &[]);
        assert!(!out.contains("Read "), "a query with no paths read files: {out}");
        assert!(out.contains("wolverines") || out.contains("a.md"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole point of sectioning, from the caller's side: the result names
    /// the passage by its headings and gives the range, so the next call reads
    /// sixty lines instead of three thousand.
    #[test]
    fn a_result_names_the_section_and_the_lines_to_read() {
        let filler = |what: &str| format!("{what} ").repeat(400);
        let source = format!(
            "# Runbook\n\n{}\n\n## Recovery\n\n{}\n\n### Restarting the agent\n\n\
             {} Run systemctl restart forge-agent when the socket is stale.\n",
            filler("introduction"),
            filler("recovery"),
            filler("restarting"),
        );
        let root = tree("sections", &[("runbook.md", &source)]);

        let out = search(&root, "systemctl restart stale socket", &["."]);
        // Named by the trail, not by the file.
        assert!(
            out.contains("Runbook › Recovery › Restarting the agent"),
            "the section is not named by its heading trail: {out}"
        );
        assert!(out.contains(" lines "), "no line range: {out}");
        // And phrased as the call to make, arguments included.
        assert!(out.contains("read_file(path="), "{out}");
        assert!(out.contains("start_line="), "{out}");
        // The range is the section's, not the file's — the answer is at the
        // bottom of a long document.
        let start: usize = out
            .split("start_line=")
            .nth(1)
            .and_then(|r| r.split(&[',', ')'][..]).next())
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0);
        assert!(start > 1, "the range starts at line {start}, so it is the whole file: {out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// One file must not take every slot. Before sectioning this was free —
    /// one document per file — and afterwards a long document's five best
    /// sections would crowd out every other file unless the spread rule counts
    /// files rather than hosts, which a `file://` URL does not have.
    #[test]
    fn one_document_does_not_fill_the_whole_result() {
        let filler = |what: &str| format!("{what} ").repeat(400);
        let mut long = String::from("# Long\n\n");
        for i in 0..6 {
            long.push_str(&format!("## Part {i}\n\n{} kestrel sightings here.\n\n", filler("padding")));
        }
        let root = tree("spread", &[
            ("long.md", &long),
            ("short.md", &format!("# Short\n\n{} kestrel sightings here too.\n", filler("other"))),
        ]);

        let out = search(&root, "kestrel sightings", &["."]);
        assert!(out.contains("short.md"), "the other file was crowded out: {out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Editing a document must not leave its old sections answering. The URLs
    /// do not line up after an edit — the ranges move and a heading can be
    /// renamed — so the previous sections are removed rather than replaced one
    /// by one.
    #[test]
    fn editing_a_document_retires_its_old_sections() {
        let filler = |what: &str| format!("{what} ").repeat(400);
        let before = format!(
            "# Spec\n\n{}\n\n## Limits\n\n{} The ceiling is forty units.\n",
            filler("intro"), filler("limits"),
        );
        let root = tree("edited", &[("spec.md", &before)]);
        let first = search(&root, "ceiling units", &["."]);
        assert!(first.contains("forty"), "{first}");

        // The stored read time has one-second resolution, so the edit is
        // forced past it rather than assumed to land after it.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Rewritten with a section inserted above, so every later line moves.
        let after = format!(
            "# Spec\n\n{}\n\n## Preface\n\n{}\n\n## Limits\n\n{} The ceiling is ninety units.\n",
            filler("intro"), filler("preface"), filler("limits"),
        );
        std::fs::write(root.join("spec.md"), &after).unwrap();

        let second = search(&root, "ceiling units", &["."]);
        assert!(second.contains("ninety"), "the edit did not take: {second}");
        assert!(
            !second.contains("forty"),
            "a section from before the edit is still answering: {second}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A range has to survive the round trip through the URL, since that is
    /// what the result is built from.
    #[test]
    fn a_section_url_carries_the_range_and_gives_it_back() {
        let path = std::path::Path::new("/docs/my notes/a file.md");
        let section = forge_search::document::Section {
            trail: vec!["Top".into()],
            lines: Some((120, 186)),
            text: "something".into(),
        };
        let url = section_url(path, &section);
        assert_eq!(url, "file:///docs/my%20notes/a%20file.md#L120-186");

        let (back, lines) = split_location(&url);
        assert_eq!(back, "/docs/my notes/a file.md");
        assert_eq!(lines, Some((120, 186)));

        // And a section with no lines to give — HTML — is still distinct and
        // still claims nothing about where it is.
        let no_lines = forge_search::document::Section {
            lines: None,
            text: "something".into(),
            ..Default::default()
        };
        let url = section_url(std::path::Path::new("/a/b.html"), &no_lines);
        let (back, lines) = split_location(&url);
        assert_eq!(back, "/a/b.html");
        assert_eq!(lines, None);
    }

    /// `paths: ["."]` is the ordinary way to say "this project", and the path
    /// it produces ends up in a `read_file` call the agent is told to make.
    #[test]
    fn a_dot_path_does_not_end_up_in_the_result() {
        let root = tree("dotpath", &[("notes/a.md", "# A\n\nAbout marmots.\n")]);
        let out = search(&root, "marmots", &["."]);
        assert!(out.contains("notes/a.md"), "{out}");
        assert!(!out.contains("/./"), "a bare `.` component reached the result: {out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Build output is not a corpus. This repository's `target/` is 83 GB
    /// across 663,234 files and holds ten readable documents; walking it took
    /// thirteen of the thirteen and a half seconds a real run spent.
    #[test]
    fn build_output_is_not_walked_but_can_be_named() {
        let root = tree("skipdirs", &[
            ("real.md", "# Real\n\nAbout pangolins.\n"),
            ("target/generated.md", "# Generated\n\nAlso pangolins.\n"),
            ("node_modules/pkg/readme.md", "# Dependency\n\nPangolins again.\n"),
        ]);

        let out = search(&root, "pangolins", &["."]);
        assert!(out.contains("Read 1 new or changed document(s)"), "{out}");
        assert!(out.contains("real.md"), "{out}");
        // Checked by what was indexed, not by what the text mentions — the
        // report names the directories it declined, so the names appear.
        assert!(!out.contains("generated.md"), "build output was indexed: {out}");
        assert!(!out.contains("readme.md"), "a dependency was indexed: {out}");
        // And said out loud, so a corpus that turned out to live under one of
        // these is a visible fact rather than a mystery about the ranking.
        assert!(out.contains("Did not descend into"), "{out}");
        assert!(out.contains("target"), "{out}");

        // Named directly, it is read — which is what makes the list a default
        // rather than a rule.
        let out = search(&root, "pangolins", &["target"]);
        assert!(out.contains("generated.md"), "a named directory was still skipped: {out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The result has to say when the query's subject is absent. Asked about a
    /// tractor, a corpus of software notes matched `what`, `does` and `take`
    /// and answered with something about deployment — truthfully scored, since
    /// those were everything findable, and completely useless.
    #[test]
    fn words_the_corpus_has_never_seen_are_named_in_the_result() {
        let root = tree("gaps", &[
            ("notes.md", "# Deployment\n\nWhat does a deploy take? It takes a region.\n"),
        ]);

        let out = search(&root, "what oil does a tractor take", &["."]);
        assert!(out.contains("No indexed document contains"), "{out}");
        assert!(out.contains("\"oil\""), "{out}");
        assert!(out.contains("\"tractor\""), "{out}");

        // And says nothing when every word is known, rather than adding a line
        // to every result.
        let clean = search(&root, "deploy region", &[]);
        assert!(!clean.contains("No indexed document contains"), "{clean}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_index_lives_beside_the_web_one_not_in_it() {
        let root = std::path::Path::new("/tmp/project");
        let docs = index_path(root);
        assert_eq!(docs, std::path::Path::new("/tmp/project/.forge/doc-index"));
        assert_ne!(docs, crate::tools::search::index_path(root));
    }
}
