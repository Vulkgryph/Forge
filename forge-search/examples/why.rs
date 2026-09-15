fn main() {
    let path = std::env::args().nth(1).expect("index path");
    let q = std::env::args().nth(2).unwrap_or_else(|| "Ford 8N engine oil SAE 30 10W30 6 quarts".into());
    let ix = forge_search::index::Index::load(std::path::Path::new(&path)).expect("load");
    println!("index: {} pages, avg {} terms\nquery: {q:?}\n", ix.len(), ix.average_length() as u64);
    let terms = forge_search::tokenize::terms(&q);
    println!("terms {terms:?}");
    print!("df: ");
    for t in &terms { print!("{t}={} ", ix.document_frequency(t)); }
    println!("\n");
    println!("{:<46} {:>7} {:>7} {:>6} {:>6} {:>6} {:>6}", "url", "score", "bm25", "title", "phrase", "cover", "prose");
    for h in forge_search::rank::search(&ix, &terms.join(" "), 10) {
        let d = ix.document(h.doc).unwrap();
        let f = &h.features;
        println!("{:<46} {:>7.2} {:>7.2} {:>6.2} {:>6.2} {:>6.2} {:>6.2}",
            d.url.replace("https://www.", "").chars().take(44).collect::<String>(),
            h.score, f.bm25, f.title, f.phrase, f.coverage, f.prose);
    }
    println!("\n── which query terms each candidate actually has ──");
    for h in forge_search::rank::search(&ix, &terms.join(" "), 10) {
        let d = ix.document(h.doc).unwrap();
        let has: Vec<&String> = terms.iter().filter(|t| !ix.positions(t, h.doc).is_empty()).collect();
        println!("  {:<42} {:?}", d.url.replace("https://www.","").chars().take(40).collect::<String>(), has);
    }
}
