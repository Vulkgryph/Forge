//! How the index scales — with a vocabulary shaped like a real one.
//!
//! The first version of this gave 50,000 pages a vocabulary of 4,000 words, so
//! every term appeared in ten thousand documents and every query intersected
//! posting lists ten thousand long. Real text is Zipfian: a few words are
//! everywhere and the overwhelming majority are rare, which is exactly why an
//! inverted index works at all. Measuring against the wrong distribution made
//! the design look far worse than it is.
fn main() {
    use forge_search::index::Index;

    // Zipfian: rank r appears with probability ~1/r. A page draws its words
    // from that, so "the" is on every page and "zdhw" is on one.
    let vocab: Vec<String> = (0..60_000).map(|i| format!("w{i}")).collect();
    // Zipf by inverse transform over the whole vocabulary: a uniform draw u in
    // (0,1] maps to rank floor(N^u), which is dense at rank 1 and sparse at
    // rank N. The first version mapped seeds to N/r for r in 1..1000, which
    // only ever produced a thousand distinct words — and then queried for ones
    // that were never in the corpus, so it measured how fast a miss is.
    let n = vocab.len() as f64;
    let pick = |seed: usize| -> usize {
        let u = ((seed % 100_000) as f64 + 1.0) / 100_000.0;
        (n.powf(u) as usize).min(vocab.len()) - 1
    };

    // `+60 pages` is the column the on-disk format exists for: what a crawler
    // pays to add a batch to an index it already has. The format used to be
    // one file rewritten in full, which made that column equal to `index` —
    // and a permanent crawler saving every sixty pages would have written
    // around 35 TB a day against a consumer SSD's 300-600 TB of endurance.
    println!(
        "{:>8} {:>9} {:>8} {:>10} {:>8} {:>9} {:>9}",
        "pages", "index", "save", "+60 pages", "load", "1 term", "3 terms"
    );
    for pages in [1_000usize, 5_000, 20_000, 50_000] {
        let mut ix = Index::new();
        for p in 0..pages {
            let body: String = (0..800)
                .map(|w| vocab[pick(p * 7919 + w * 104_729)].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            ix.add(&format!("https://h{}.test/{p}", p % 500), "Page", "", &body);
        }
        // A fresh directory per run, so the measured size is this run's.
        let path = std::env::temp_dir().join(format!("forge-scale-zipf-{pages}"));
        let _ = std::fs::remove_dir_all(&path);

        let t = std::time::Instant::now();
        ix.save(&path).unwrap();
        let save = t.elapsed();
        let size = bytes_in(&path);

        // Then the incremental case: sixty more pages onto an index of this
        // size, which is a crawl batch.
        for p in pages..pages + 60 {
            let body: String = (0..800)
                .map(|w| vocab[pick(p * 7919 + w * 104_729)].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            ix.add(&format!("https://h{}.test/{p}", p % 500), "Page", "", &body);
        }
        ix.save(&path).unwrap();
        let appended = bytes_in(&path) - size;

        let t = std::time::Instant::now();
        let back = Index::load(&path).unwrap();
        let load = t.elapsed();

        // Terms chosen from what the corpus actually contains, by document
        // frequency — the broad one is on a large share of pages, the narrow
        // ones on few. Asserted, because querying for a word that is not there
        // measures nothing.
        let mut by_df: Vec<(&str, u32)> = vocab
            .iter()
            .map(|w| (w.as_str(), back.document_frequency(w)))
            .filter(|(_, df)| *df > 0)
            .collect();
        by_df.sort_by_key(|(_, df)| std::cmp::Reverse(*df));

        // "Broad" has to mean informative-but-common, not universal.
        //
        // The previous version took the single most frequent term, which in
        // this corpus is in *every* document — and a term in every document
        // has an inverse document frequency of zero, so BM25 scores every
        // page identically and the top ten is arbitrary. That is a stop word,
        // not a broad query, and measuring against it measured nothing.
        // Something like "microsoft" in a real corpus sits near a tenth.
        let target = (pages / 10).max(1) as u32;
        let broad_idx = by_df
            .iter()
            .position(|(_, df)| *df <= target)
            .unwrap_or(by_df.len() - 1);
        let broad = by_df[broad_idx].0;
        let broad_df = by_df[broad_idx].1;
        let rare: Vec<&str> = by_df.iter().rev().take(2).map(|(w, _)| *w).collect();
        let narrow = format!("{broad} {} {}", rare[0], rare[1]);
        let t = std::time::Instant::now();
        let b = forge_search::query::search(&back, broad, 10);
        let one = t.elapsed();
        let t = std::time::Instant::now();
        let n = forge_search::query::search(&back, &narrow, 10);
        let three = t.elapsed();

        println!("{:>8} {:>6.1} MB {:>7.0?} {:>7.2} MB {:>7.0?} {:>8.2?} {:>8.2?}   broad term in {} of {} pages, {} hits / narrow {} hits",
            pages, size as f64 / 1048576.0, save, appended as f64 / 1048576.0, load, one, three,
            broad_df, pages, b.len(), n.len());
        std::fs::remove_dir_all(&path).ok();
    }
}

/// Every byte in an index directory.
fn bytes_in(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}
