//! Does the challenge detector fire on the real web?
//!
//! The rail it feeds — refuse the page, offer it to a person, take back what a
//! browser fetched — is only reachable if this returns `Some`. So it is worth
//! asking the actual function, against actual sites, rather than reasoning
//! about the signatures it looks for.
use forge_search::fetch::{Fetched, Fetcher};

struct Curl {
    agent: String,
}

impl Fetcher for Curl {
    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        // Headers matter here: the strongest signals are `cf-mitigated` and
        // friends, and a fetcher that drops them can only fall back to reading
        // the body.
        let dir = std::env::temp_dir().join("forge-challenge-probe");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let head = dir.join("h.txt");
        let out = std::process::Command::new("curl")
            .args([
                "-sS", "-L", "--max-time", "25",
                "-D", head.to_str().unwrap(),
                "-w", "\n%{http_code}\t%{content_type}",
                "-A", &self.agent, url,
            ])
            .output()
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let cut = text.rfind('\n').ok_or("no status trailer")?;
        let fields: Vec<&str> = text[cut + 1..].split('\t').collect();

        let raw = std::fs::read_to_string(&head).unwrap_or_default();
        let headers: Vec<(String, String)> = raw
            .lines()
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
            .collect();

        Ok(Fetched {
            status: fields.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            final_url: url.to_string(),
            content_type: fields.get(1).unwrap_or(&"").split(';').next().unwrap_or("").trim().to_lowercase(),
            body: text[..cut].to_string(),
            headers,
        })
    }

    fn user_agent(&self) -> &str {
        &self.agent
    }
}

fn main() {
    let fetcher = Curl { agent: concat!("forge-search/", env!("CARGO_PKG_VERSION"), " (+https://vulkgryph.com/projects/forge/)").to_string() };
    // A spread of bot-management vendors, plus sites that should pass clean so
    // a detector that simply says "challenged" to everything is visible.
    let sites = [
        ("https://www.reuters.com/", "expect a challenge"),
        ("https://stackoverflow.com/", "expect a challenge"),
        ("https://www.g2.com/", "expect a challenge"),
        ("https://www.zillow.com/", "expect a challenge"),
        ("https://www.indeed.com/", "expect a challenge"),
        ("https://doc.rust-lang.org/std/", "expect clean"),
        ("https://ntractorclub.com/", "expect clean"),
        ("https://en.wikipedia.org/wiki/Tractor", "expect clean"),
    ];

    println!("{:<44} {:>5}  {:<26} {}", "url", "http", "challenge()", "expectation");
    let mut missed = Vec::new();
    let mut wrong = Vec::new();
    for (url, expectation) in sites {
        match fetcher.fetch(url) {
            Ok(got) => {
                let verdict = got.challenge();
                println!(
                    "{url:<44} {:>5}  {:<26} {expectation}",
                    got.status,
                    verdict.unwrap_or("— none —"),
                );
                let blocked = got.status == 401 || got.status == 403 || got.status == 429;
                if blocked && verdict.is_none() {
                    missed.push((url, got.status));
                }
                if !blocked && got.status == 200 && verdict.is_some() {
                    wrong.push((url, verdict.unwrap()));
                }
            }
            Err(e) => println!("{url:<44}  fetch failed: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }

    println!();
    if missed.is_empty() {
        println!("Every blocked response was recognised as a challenge.");
    } else {
        println!("REFUSED BUT NOT RECOGNISED — these would be indexed as pages:");
        for (url, status) in &missed {
            println!("  {status}  {url}");
        }
    }
    if !wrong.is_empty() {
        println!("SERVED 200 BUT CALLED A CHALLENGE — these pages would be thrown away:");
        for (url, vendor) in &wrong {
            println!("  {vendor}  {url}");
        }
    }
}
