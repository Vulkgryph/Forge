// Not a test — a demonstration that the four stages compose.
fn main() {
    use forge_search::{html, index::Index, rank};
    let pages = [
        ("https://doc.rust-lang.org/nomicon/", r#"<title>The Rustonomicon</title>
            <p>Writing unsafe Rust requires understanding the allocator and memory layout.</p>"#),
        ("https://doc.rust-lang.org/alloc/", r#"<title>The alloc crate</title>
            <p>In no_std builds the allocator must be provided. The allocator is a global.</p>"#),
        ("https://example.com/blog/2019/04/17/gardening/", r#"<title>Tomatoes</title>
            <p>Tomatoes need sunshine. Nothing here about memory at all.</p>"#),
    ];
    let mut ix = Index::new();
    for (url, raw) in pages {
        let page = html::parse(raw);
        ix.add(url, &page.title, &page.description, &page.text);
    }
    println!("indexed {} pages, average length {:.1} terms\n", ix.len(), ix.average_length());
    for query in ["no_std allocator", "allocator", "tomatoes", "quantum"] {
        println!("query: {query:?}");
        let hits = rank::search(&ix, query, 5);
        if hits.is_empty() { println!("    (no results)\n"); continue; }
        for h in &hits {
            let d = ix.document(h.doc).unwrap();
            println!("    {:.3}  {}", h.score, d.title);
            println!("           {}", d.url);
            println!("           bm25 {:.2}  title {:.2}  phrase {:.2}", h.features.bm25, h.features.title, h.features.phrase);
        }
        println!();
    }
}
