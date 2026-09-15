// Measures an on-demand crawl against a real site: how long, how much, and
// what can be answered afterwards.
//
// Needs the network. Not a test — a measurement, run by hand.
use forge_search::{crawl, fetch::{Fetched, Fetcher}, index::Index, query};

struct Curl { agent: String }

impl Fetcher for Curl {
    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let out = std::process::Command::new("curl")
            .args(["-sS", "-L", "--max-time", "15", "-w", "\n%{http_code}\t%{url_effective}\t%{content_type}", "-A", &self.agent, url])
            .output().map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let cut = text.rfind('\n').ok_or("no trailer")?;
        let (body, trailer) = (&text[..cut], &text[cut + 1..]);
        let f: Vec<&str> = trailer.split('\t').collect();
        Ok(Fetched {
            status: f.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            final_url: f.get(1).unwrap_or(&url).to_string(),
            content_type: f.get(2).unwrap_or(&"").split(';').next().unwrap_or("").trim().to_lowercase(),
            body: body.to_string(), headers: Vec::new() })
    }
    fn user_agent(&self) -> &str { &self.agent }
}

fn main() {
    let seed = std::env::args().nth(1).unwrap_or_else(|| "https://doc.rust-lang.org/nomicon/".into());
    let pages: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(25);

    let fetcher = Curl { agent: "forge-search/0.1 (measurement)".into() };
    let limits = crawl::Limits {
        max_pages: pages, max_depth: 4,
        politeness: std::env::var("POLITENESS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5),
        stay_on_host: true, max_page_bytes: 2 * 1024 * 1024,
        max_seconds: Some(120.0),
    };

    println!("seed      {seed}");
    println!("limit     {pages} pages, {}s politeness\n", limits.politeness);

    let start = std::time::Instant::now();
    let clock = crawl::SystemClock;
    let mut index = Index::new();
    let report = {
        let mut c = crawl::Crawler::new(&fetcher, &clock, limits);
        c.seed(&seed).expect("seed");
        c.run(&mut index)
    };
    let elapsed = start.elapsed();

    println!("── crawl ──");
    println!("  wall clock      {:.1}s", elapsed.as_secs_f64());
    println!("  fetched         {}", report.fetched);
    println!("  indexed         {}", report.indexed);
    println!("  robots refused  {}", report.disallowed);
    println!("  not html        {}", report.not_html);
    println!("  missing         {}", report.missing);
    println!("  unreachable     {}", report.unreachable);
    println!("  too large       {}", report.too_large);
    println!("  timed out       {}", report.timed_out);
    println!("  left in queue   {}", report.remaining);
    if report.fetched > 0 {
        println!("  per page        {:.0}ms", elapsed.as_millis() as f64 / report.fetched as f64);
    }

    println!("\n── index ──");
    println!("  pages           {}", index.len());
    println!("  average length  {:.0} terms", index.average_length());
    let dir = std::env::temp_dir().join("forge-search-measure");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("index.bin");
    let t = std::time::Instant::now();
    index.save(&path).expect("save");
    let save_ms = t.elapsed().as_millis();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let t = std::time::Instant::now();
    let reloaded = Index::load(&path).expect("load");
    println!("  on disk         {:.1} MB  (saved in {save_ms}ms, loaded in {}ms)", size as f64 / 1e6, t.elapsed().as_millis());
    println!("  bytes per page  {:.0}", size as f64 / index.len().max(1) as f64);

    println!("\n── queries against it ──");
    for q in std::env::args().skip(3).collect::<Vec<_>>().iter().map(|s| s.as_str())
        .chain(["ownership", "unsafe", "lifetime", "\"data race\""].into_iter().take(if std::env::args().count() > 3 { 0 } else { 4 })) {
        let t = std::time::Instant::now();
        let hits = query::search(&reloaded, q, 3);
        let us = t.elapsed().as_micros();
        println!("  {:<18} {:>3} hit(s) in {:>5}µs", q, hits.len(), us);
        for h in hits.iter().take(2) {
            let title = if h.title.is_empty() { &h.url } else { &h.title };
            println!("        {:.2}  {}", h.score, title.chars().take(64).collect::<String>());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
