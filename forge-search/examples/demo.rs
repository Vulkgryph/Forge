// The whole engine: crawl a site, then search it with operators and snippets.
// Against a fixed fetcher, so it needs no network and gives the same answer
// every time.
fn main() {
    use forge_search::{crawl, fetch::{Fetched, StaticFetcher}, query};

    let site = StaticFetcher::new()
        .with_response("https://docs.example/robots.txt", Fetched {
            status: 200, final_url: "https://docs.example/robots.txt".into(),
            content_type: "text/plain".into(),
            body: "User-agent: *\nDisallow: /private/\n".into(), headers: Vec::new() })
        .with_page("https://docs.example/", r#"<title>Runtime docs</title>
            <p>Reference for the runtime.</p>
            <a href="/alloc">Allocation</a><a href="/no-std">no_std</a>
            <a href="/gc">Collection</a><a href="/private/internal">Internal</a>"#)
        .with_page("https://docs.example/alloc", r#"<title>Allocation</title>
            <meta name="description" content="How allocation works">
            <p>Navigation: home docs allocator index.</p>
            <p>The global allocator is chosen at link time. In no_std builds an
            allocator must be provided by the crate, or nothing can allocate.</p>"#)
        .with_page("https://docs.example/no-std", r#"<title>Writing no_std code</title>
            <p>In no_std builds there is no allocator unless you bring one.
            Core is always available.</p>"#)
        .with_page("https://docs.example/gc", r#"<title>Garbage collection</title>
            <p>This runtime has no garbage collector. Memory is freed by the
            allocator when a value is dropped.</p>"#)
        .with_page("https://docs.example/private/internal", "<p>never fetched</p>");

    let limits = crawl::Limits { politeness: 0.0, ..Default::default() };
    let (index, report) = crawl::crawl(&site, &["https://docs.example/"], limits);
    println!("crawl: fetched {}, indexed {}, robots refused {}\n",
             report.fetched, report.indexed, report.disallowed);

    for q in [
        "no_std allocator",
        "\"global allocator\"",
        "allocator -garbage",
        "internal",
    ] {
        println!("query  {q}");
        let hits = query::search(&index, q, 3);
        if hits.is_empty() { println!("       (nothing)\n"); continue; }
        for h in &hits {
            println!("  {:.2}  {}", h.score, h.title);
            println!("        {}", h.url);
            println!("        {}", h.snippet);
        }
        println!();
    }
}
