//! Search the biomedical literature through Europe PMC and query what came
//! back.
//!
//! The question this answers is the one a crawler cannot: a measured value,
//! on a stated preparation, at a stated temperature. Those numbers are in the
//! Methods section of a paper, and PubMed Central's website forbids crawling —
//! so the text has to come through the API, and only for articles whose
//! licence permits keeping it.
//!
//! Run with:
//!
//! ```text
//! cargo run -p forge-search --example literature -- "pyramidal neuron temperature"
//! ```

use forge_search::fetch::{Fetched, Fetcher};
use forge_search::{crawl, epmc, index::Index, query};

/// A fetcher over `curl`, so the example needs nothing built.
///
/// Forge itself supplies its own HTTP client; this is only here so the
/// measurement can be reproduced from a checkout.
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
        })
    }

    fn user_agent(&self) -> &str {
        &self.agent
    }
}

fn main() {
    let topic = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#""pyramidal neuron" AND "patch clamp" AND temperature"#.to_string());
    let want: usize = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(6);

    let fetcher = Curl {
        agent: "forge-search/0.4.2 (+https://github.com/Vulkgryph/Forge)".into(),
    };
    let clock = crawl::SystemClock;
    let mut index = Index::new();

    println!("── asking Europe PMC ──");
    println!("  query    {topic}");
    let started = std::time::Instant::now();
    let report = epmc::Connector::new(&fetcher, &clock, 1.0).run(&topic, want, &mut index);
    let wall = started.elapsed().as_secs_f64();

    println!(
        "  {} match in total; examined {}, indexed {}, citation only {}",
        report.hit_count, report.examined, report.indexed, report.citation_only
    );
    if report.unreachable > 0 || report.empty > 0 {
        println!("  unreachable {}  empty {}", report.unreachable, report.empty);
    }
    println!(
        "  {wall:.1}s for {} request(s), {:.0}ms each",
        report.indexed + 1,
        wall * 1000.0 / (report.indexed + 1) as f64
    );
    println!(
        "  index    {} articles, {} terms each on average",
        index.len(),
        index.average_length() as u64
    );

    println!("\n── what was kept, and on what terms ──");
    for doc in (0..index.len() as u32).filter_map(|d| index.document(d)) {
        println!("  {}", doc.title.chars().take(88).collect::<String>());
        println!("      {}", doc.attribution);
        println!("      {}", doc.url);
    }

    let cited: Vec<&epmc::Found> = report
        .articles
        .iter()
        .filter(|a| !a.may_index_full_text())
        .collect();
    if !cited.is_empty() {
        println!("\n── not kept: licence does not permit it ──");
        for a in cited.iter().take(4) {
            println!("  {}", a.citation().chars().take(88).collect::<String>());
            println!("      {}", a.article_url());
        }
    }

    // The point of the exercise: a recording temperature is a number in a
    // Methods section, so ask for one.
    println!("\n── querying what was kept ──");
    for q in [
        "recorded at temperature",
        "\"whole-cell\" recordings temperature",
        "bath temperature degrees",
        "resting membrane potential",
        "artificial cerebrospinal fluid",
    ] {
        let hits = query::search(&index, q, 2);
        println!("\n  {q}  → {} hit(s)", hits.len());
        for hit in &hits {
            println!("     {:.2}  {}", hit.score, hit.title.chars().take(70).collect::<String>());
            println!("           {}", hit.snippet.replace('\n', " ").chars().take(220).collect::<String>());
        }
    }

    // Anything that looks like a temperature in the text that was kept.
    println!("\n── temperatures stated in the text ──");
    let mut shown = 0;
    for doc in (0..index.len() as u32).filter_map(|d| index.document(d)) {
        for (at, _) in doc.text.char_indices().filter(|(_, c)| *c == '°') {
            let from = doc.text[..at]
                .char_indices()
                .rev()
                .nth(110)
                .map_or(0, |(i, _)| i);
            let to = doc.text[at..]
                .char_indices()
                .nth(12)
                .map_or(doc.text.len(), |(i, _)| at + i);
            println!("  …{}…", doc.text[from..to].replace('\n', " ").trim());
            shown += 1;
            if shown >= 8 {
                break;
            }
        }
        if shown >= 8 {
            break;
        }
    }
    if shown == 0 {
        println!("  none found in the articles that were kept");
    }
}
