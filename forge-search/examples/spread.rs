//! Crawl the sites given on the command line and report not just the best
//! passage but the spread of what the sources say.
use forge_search::fetch::{Fetched, Fetcher};
use forge_search::{crawl, index::Index, query};

struct Curl { agent: String }
impl Fetcher for Curl {
    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let out = std::process::Command::new("curl")
            .args(["-sS","-L","--max-time","20","-w","\n%{http_code}\t%{url_effective}\t%{content_type}","-A",&self.agent,url])
            .output().map_err(|e| e.to_string())?;
        let t = String::from_utf8_lossy(&out.stdout).to_string();
        let cut = t.rfind('\n').ok_or("no trailer")?;
        let f: Vec<&str> = t[cut+1..].split('\t').collect();
        Ok(Fetched { status: f.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            final_url: f.get(1).unwrap_or(&url).to_string(),
            content_type: f.get(2).unwrap_or(&"").split(';').next().unwrap_or("").trim().to_lowercase(),
            body: t[..cut].to_string() })
    }
    fn user_agent(&self) -> &str { &self.agent }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let topic = args.next().unwrap_or_else(|| "engine oil".into());
    let seeds: Vec<String> = args.collect();
    if seeds.is_empty() { eprintln!("usage: spread <query> <seed url>..."); return; }

    let fetcher = Curl { agent: "forge-search/0.4.2 (+https://github.com/Vulkgryph/Forge)".into() };
    let limits = crawl::Limits { max_pages: 25, max_depth: 2, politeness: 1.0,
        stay_on_host: false, max_page_bytes: 4*1024*1024, max_seconds: Some(120.0) };
    let clock = crawl::SystemClock;
    let mut index = Index::new();
    let started = std::time::Instant::now();
    let report = { let mut c = crawl::Crawler::new(&fetcher, &clock, limits);
        for s in &seeds { if let Err(e) = c.seed(s) { eprintln!("bad seed {s}: {e}"); } }
        c.run(&mut index) };

    println!("── crawl ──  {:.1}s  fetched {} indexed {} refused {} missing {}",
        started.elapsed().as_secs_f64(), report.fetched, report.indexed, report.disallowed, report.missing);
    let mut hosts: Vec<String> = index.urls()
        .filter_map(|u| forge_search::url::Url::parse(u).ok().map(|p| p.host)).collect();
    hosts.sort(); hosts.dedup();
    println!("  hosts indexed: {hosts:?}");

    println!("\n── best passages for {topic:?} (spread across hosts) ──");
    for (i, hit) in query::search(&index, &topic, 5).iter().enumerate() {
        let host = forge_search::url::Url::parse(&hit.url).map(|u| u.host).unwrap_or_default();
        println!("{}. [{host}] {}", i+1, hit.title.chars().take(62).collect::<String>());
        println!("   {}", hit.snippet.replace('\n', " ").chars().take(170).collect::<String>());
    }

    println!("\n── what the sources say, by number of independent sources ──");
    let spread = query::spread(&index, &topic, 25);
    for m in spread.iter().filter(|m| m.sources.len() > 1).take(14) {
        println!("  {:<22} {} source(s), {} passage(s)  {:?}",
            m.term, m.sources.len(), m.passages, m.sources);
    }
    let singles = spread.iter().filter(|m| m.sources.len() == 1).count();
    println!("  ({singles} more term(s) from a single source)");
}
