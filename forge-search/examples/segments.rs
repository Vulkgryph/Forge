//! What two real crawls cost an index directory.
//!
//! The unit tests prove the format on fixtures. This proves it on the web,
//! which is where the claim actually has to hold: crawl a site, save, crawl a
//! second site, save again, and check that the first save's bytes are still
//! there untouched and that the second save wrote only the second crawl.
//!
//! Run with:
//!
//! ```text
//! cargo run -p forge-search --example segments
//! ```

use forge_search::fetch::{Fetched, Fetcher};
use forge_search::{crawl, index::Index, query};

/// A fetcher over `curl`, so the example needs nothing built. Forge itself
/// supplies its own HTTP client; this is only here so the measurement can be
/// reproduced from a checkout.
struct Curl {
    agent: String,
}

impl Fetcher for Curl {
    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let out = std::process::Command::new("curl")
            .args([
                "-sS",
                "-L",
                "--max-time",
                "30",
                "-w",
                "\n%{http_code}\t%{url_effective}\t%{content_type}",
                "-A",
                &self.agent,
                url,
            ])
            .output()
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let cut = text.rfind('\n').ok_or("no status trailer")?;
        let fields: Vec<&str> = text[cut + 1..].split('\t').collect();
        Ok(Fetched {
            status: fields.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            final_url: fields.get(1).unwrap_or(&url).to_string(),
            content_type: fields
                .get(2)
                .unwrap_or(&"")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_lowercase(),
            body: text[..cut].to_string(),
            headers: Vec::new(),
        })
    }

    fn user_agent(&self) -> &str {
        &self.agent
    }
}

fn main() {
    let fetcher = Curl {
        agent: "forge-search/0.5.0 (+https://vulkgryph.com/projects/forge/)".into(),
    };
    let dir = std::env::temp_dir().join("forge-segments-live");
    let _ = std::fs::remove_dir_all(&dir);

    let limits = crawl::Limits { max_pages: 12, max_depth: 1, politeness: 1.0, ..Default::default() };

    println!("── first crawl ──");
    let (mut index, report) = crawl::crawl(&fetcher, &["https://doc.rust-lang.org/std/"], limits.clone());
    println!("  indexed {} of {} fetched", index.len(), report.fetched);
    index.save(&dir).unwrap();
    let after_first = listing(&dir);
    show(&after_first);

    // Keep the bytes, so "untouched" can be checked rather than assumed.
    let first_name = after_first[0].0.clone();
    let first_bytes = std::fs::read(dir.join(&first_name)).unwrap();

    println!("\n── second crawl, into the same index ──");
    let clock = crawl::SystemClock;
    let mut crawler = crawl::Crawler::new(&fetcher, &clock, limits);
    let _ = crawler.seed("https://doc.rust-lang.org/book/");
    let report = crawler.run(&mut index);
    println!("  indexed {} more of {} fetched", report.indexed, report.fetched);
    index.save(&dir).unwrap();
    let after_second = listing(&dir);
    show(&after_second);

    println!("\n── what the second save cost ──");
    let first_total: u64 = after_first.iter().map(|(_, n)| n).sum();
    let second_total: u64 = after_second.iter().map(|(_, n)| n).sum();
    println!("  index was    {:>9}", human(first_total));
    println!("  index is now {:>9}", human(second_total));
    println!("  written      {:>9}   (a full rewrite would have been {})", human(second_total - first_total), human(second_total));
    println!(
        "  first segment {}",
        if std::fs::read(dir.join(&first_name)).unwrap() == first_bytes {
            "unchanged, byte for byte"
        } else {
            "WAS REWRITTEN — the save is not append-only"
        }
    );

    println!("\n── and it still answers, from both crawls ──");
    let back = Index::load(&dir).unwrap();
    println!("  {} pages across {} segments", back.len(), after_second.len() - 1);
    for q in ["vector capacity", "ownership borrowing", "hash map entry"] {
        let hits = query::search(&back, q, 2);
        println!("\n  {q}  → {} hit(s)", hits.len());
        for hit in &hits {
            println!("     {:.2}  {}", hit.score, hit.url);
            println!("           {}", hit.snippet.replace('\n', " ").chars().take(120).collect::<String>());
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every file in the index directory with its size, manifest last.
fn listing(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.metadata().map(|m| m.len()).unwrap_or(0)))
        .collect();
    out.sort();
    out
}

fn show(files: &[(String, u64)]) {
    for (name, size) in files {
        println!("  {name:<20} {:>9}", human(*size));
    }
}

fn human(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.2} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
