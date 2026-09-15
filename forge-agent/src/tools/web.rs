// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use scraper::{Html, Selector};

use crate::api::{ApiClient, Message};

/// What Forge calls itself when it fetches a page.
///
/// This used to claim to be Chrome 120 on macOS. Pretending to be a browser is
/// how a scraper gets served, and it is also a lie told to someone else's
/// server — the same objection this project raises elsewhere about identifying
/// as a client you are not. An honest agent identifies itself and takes the
/// answer it gets; `web_search` is disabled by default and does not work in
/// practice anyway, so there is nothing to preserve by pretending.
const FORGE_USER_AGENT: &str = concat!("forge-agent/", env!("CARGO_PKG_VERSION"));

fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(FORGE_USER_AGENT)
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// How much of a page to hand the summariser.
///
/// Not a parameter. It was one, and a real headless run set it to 24,000,
/// 55,000 and 180,000 on consecutive calls with no reason to prefer any of
/// them — a number the caller cannot reason about is a number it fills in
/// arbitrarily.
const MAX_LENGTH: usize = 40_000;

/// What to say when a page was refused by a bot check rather than served.
///
/// Its own function because both paths need it — a refusal arrives as a 4xx
/// with telling headers, and also as a perfectly ordinary 200 with an
/// interstitial in the body.
///
/// The wording matters more than usual. Every other failure here invites
/// another attempt: a 404 might be the wrong address, an empty result might be
/// the wrong words. This one does not, and an agent that treats it as one
/// spends the turn rephrasing a request that cannot succeed. So it says the
/// address is right, that retrying will not help, and what the alternative is.
fn refused(url: &str, vendor: &str, status: Option<u16>) -> String {
    let how = match status {
        Some(code) => format!("status {code}"),
        // A 200 carrying an interstitial, which is the case nothing else
        // would have caught.
        None => "status 200, with the bot check in place of the page".to_string(),
    };
    format!(
        "Error: {url} was refused by a bot check ({vendor}) rather than served — {how}.\n\
         The address is fine; the access is not. No rewording and no retry reaches a page \
         behind a challenge — the server will serve it to a browser and not to this tool. \
         Use what you have from elsewhere and say that this source was unavailable, or ask \
         the user to open the page themselves."
    )
}

/// What the index actually holds on the host that was just guessed at.
///
/// Ranked against the caller's own `prompt`, since that says what it wanted
/// rather than what it typed. Three at most: this is a correction to a failed
/// call, not a directory listing, and a wall of URLs is its own kind of
/// unhelpful.
fn suggest_real_urls(index_path: &std::path::Path, asked: &str, prompt: &str) -> String {
    let Ok(wanted) = forge_search::url::Url::parse(asked) else {
        return String::new();
    };
    let Ok(index) = forge_search::index::Index::load(index_path) else {
        return String::new();
    };

    let same_host = |u: &str| {
        forge_search::url::Url::parse(u)
            .map(|p| p.host == wanted.host)
            .unwrap_or(false)
    };
    let held = index.urls().filter(|u| same_host(u)).count();
    if held == 0 {
        return format!(
            "The index has not read {} at all, so there is nothing to check this path \
             against. Call web_search with sites=[\"{}://{}\"] first — it will crawl the \
             site and return real URLs, which is more reliable than guessing a path.",
            wanted.host, wanted.scheme, wanted.host,
        );
    }

    // Best pages on that host for what the caller said it wanted.
    let mut found: Vec<String> = forge_search::rank::search(&index, prompt, 60)
        .into_iter()
        .filter_map(|hit| index.document(hit.doc).map(|d| d.url.clone()))
        .filter(|u| same_host(u) && u != asked)
        .collect();
    found.truncate(3);

    if found.is_empty() {
        return format!(
            "The index holds {held} page(s) from {} but none matching that description. \
             Query them with web_search rather than guessing another path.",
            wanted.host,
        );
    }

    let mut out = format!(
        "The index holds {held} page(s) from {}. These are real URLs on it, closest to \
         what you asked for:\n",
        wanted.host,
    );
    for u in &found {
        out.push_str(&format!("   {u}\n"));
    }
    out.push_str(
        "Fetch one of those, or query the site with web_search. Do not guess another path — \
         a guessed path is the usual reason this call fails.",
    );
    out
}

pub async fn web_fetch(
    args: &serde_json::Value,
    summarizer: Option<(&ApiClient, &str)>,
    index_path: Option<&std::path::Path>,
) -> Result<String> {
    let url = args["url"].as_str().context("Missing 'url' argument")?;
    let prompt = args["prompt"]
        .as_str()
        .context("Missing 'prompt' argument")?;
    let max_length = MAX_LENGTH;

    let client = build_http_client();

    let response = client
        .get(url)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .send()
        .await
        .context("Failed to fetch URL")?;

    let status = response.status();
    // The response headers, so the same challenge detection the crawler uses
    // applies here. Collected before the body is consumed.
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str().ok().map(|v| (k.as_str().to_ascii_lowercase(), v.to_string()))
        })
        .collect();
    let content_type = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .map(|(_, v)| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .unwrap_or_default();

    if !status.is_success() {
        // A refusal before a failure: the headers alone identify most
        // challenges, and it is a different thing from the page being gone.
        let probe = forge_search::fetch::Fetched {
            status: status.as_u16(),
            final_url: url.to_string(),
            content_type: content_type.clone(),
            headers: headers.clone(),
            body: String::new(),
        };
        if let Some(vendor) = probe.challenge() {
            return Ok(refused(url, vendor, Some(status.as_u16())));
        }

        let mut out = format!("Error: Request failed with status {}", response.status());
        // A failed fetch is nearly always a guessed path, and the index
        // usually knows the real ones.
        //
        // Measured over five headless runs on one question: 21 of 35
        // `web_fetch` calls returned 404, every one of them a URL the model
        // invented, while every fetch that produced something useful used a
        // URL `web_search` had returned. Two of the guesses even had a space
        // in them. So the most useful thing a failed call can say is what
        // would have worked.
        if let Some(path) = index_path {
            out.push('\n');
            out.push_str(&suggest_real_urls(path, url, prompt));
        }
        return Ok(out);
    }

    let final_url = response.url().to_string();
    let html = response
        .text()
        .await
        .context("Failed to read response body")?;

    // The dangerous case: a challenge served with status 200. Nothing about
    // the status says anything is wrong, so without this the interstitial is
    // summarised as though it were the page — and the answer comes back as
    // "the page asks you to enable JavaScript", which reads like a fact about
    // the site rather than a refusal.
    let served = forge_search::fetch::Fetched {
        status: status.as_u16(),
        final_url: final_url.clone(),
        content_type,
        headers,
        body: html.clone(),
    };
    if let Some(vendor) = served.challenge() {
        return Ok(refused(url, vendor, None));
    }

    // Parse HTML and extract text in a block so `document` (non-Send) is dropped
    // before any subsequent .await points.
    let (title, truncated, text_len) = {
        let document = Html::parse_document(&html);

        let title = Selector::parse("title")
            .ok()
            .and_then(|sel| document.select(&sel).next())
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let text = extract_readable_text(&document);
        let text_len = text.len();
        let truncated: String = text.chars().take(max_length).collect();

        (title, truncated, text_len)
    };

    // If we have a summarizer, route through LLM
    if let Some((api_client, model_id)) = summarizer {
        let system = "You are a web content extraction assistant. Given a web page's text content \
            and a user's question, provide a focused, accurate answer based only on the \
            page content. Be concise. If the page doesn't contain relevant information, say so.";

        let mut page_context = String::new();
        if !title.is_empty() {
            page_context.push_str(&format!("Page title: {}\n", title));
        }
        page_context.push_str(&format!("URL: {}\n\n", final_url));
        page_context.push_str(&truncated);

        let user_msg = format!(
            "<page_content>\n{}\n</page_content>\n\nQuestion: {}",
            page_context, prompt
        );

        let messages = vec![Message::system(system), Message::user(&user_msg)];
        let empty_tools = vec![];

        match api_client.chat(model_id, &messages, &empty_tools).await {
            Ok(response) => {
                if let Some(choice) = response.choices.first() {
                    if let Some(ref content) = choice.message.content {
                        return Ok(format!(
                            "<web_content source=\"{}\">\n{}</web_content>",
                            final_url, content
                        ));
                    }
                }
                // Fallback if no content in response
                Ok(format!(
                    "<web_content source=\"{}\">\nError: Summarizer returned no content\n\nRaw excerpt:\n{}</web_content>",
                    final_url, &truncated.chars().take(2000).collect::<String>()
                ))
            }
            Err(e) => {
                // Summarizer failed — return raw text as fallback
                let mut output = format!("[Summarizer error: {}]\n\n", e);
                if !title.is_empty() {
                    output.push_str(&format!("Title: {}\n", title));
                }
                output.push_str(&format!("URL: {}\n\n", final_url));
                output.push_str(&truncated);
                Ok(format!(
                    "<web_content source=\"{}\">\n{}</web_content>",
                    final_url, output
                ))
            }
        }
    } else {
        // No summarizer — return raw text (fallback)
        let was_truncated = text_len > max_length;
        let mut output = String::new();
        if !title.is_empty() {
            output.push_str(&format!("Title: {}\n", title));
        }
        output.push_str(&format!("URL: {}\n\n", final_url));
        output.push_str(&truncated);
        if was_truncated {
            output.push_str("\n\n...(truncated)");
        }

        Ok(format!(
            "<web_content source=\"{}\">\n{}</web_content>",
            final_url, output
        ))
    }
}

/// Extract readable text from HTML, preferring <article> or <main> content,
/// and stripping navigation, scripts, styles, etc.
fn extract_readable_text(document: &Html) -> String {
    // Tags to skip entirely
    let skip_tags = [
        "script", "style", "nav", "header", "footer", "aside", "noscript", "svg", "form",
    ];

    // Try to find main content area first
    let content_selectors = ["article", "main", "[role=\"main\"]"];
    for sel_str in &content_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(element) = document.select(&sel).next() {
                let text = extract_text_from_element(element, &skip_tags);
                let cleaned = collapse_whitespace(&text);
                if cleaned.len() > 100 {
                    return cleaned;
                }
            }
        }
    }

    // Fallback to body
    if let Ok(sel) = Selector::parse("body") {
        if let Some(element) = document.select(&sel).next() {
            let text = extract_text_from_element(element, &skip_tags);
            return collapse_whitespace(&text);
        }
    }

    // Last resort: all text
    collapse_whitespace(&document.root_element().text().collect::<String>())
}

/// Recursively extract text from an element, skipping specified tags.
fn extract_text_from_element(element: scraper::ElementRef, skip_tags: &[&str]) -> String {
    let mut text = String::new();

    for node in element.children() {
        match node.value() {
            scraper::node::Node::Text(t) => {
                text.push_str(t);
            }
            scraper::node::Node::Element(el) => {
                let tag = el.name();
                if skip_tags.contains(&tag) {
                    continue;
                }
                // Add newlines for block elements
                let is_block = matches!(
                    tag,
                    "p" | "div"
                        | "br"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "li"
                        | "tr"
                        | "blockquote"
                        | "pre"
                        | "section"
                        | "dd"
                        | "dt"
                );
                if is_block {
                    text.push('\n');
                }
                if let Some(child_ref) = scraper::ElementRef::wrap(node) {
                    text.push_str(&extract_text_from_element(child_ref, skip_tags));
                }
                if is_block {
                    text.push('\n');
                }
            }
            _ => {}
        }
    }

    text
}

/// Collapse multiple whitespace/newlines into single spaces/newlines.
fn collapse_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_newline = false;
    let mut prev_space = false;

    for ch in s.chars() {
        if ch == '\n' {
            if !prev_newline {
                result.push('\n');
            }
            prev_newline = true;
            prev_space = false;
        } else if ch.is_whitespace() {
            if !prev_space && !prev_newline {
                result.push(' ');
            }
            prev_space = true;
        } else {
            prev_newline = false;
            prev_space = false;
            result.push(ch);
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Named per test, not per process: tests run in parallel threads of one
    /// process, so a path keyed on the pid alone has two tests deleting each
    /// other's fixture. Which is exactly what happened.
    fn index_with(name: &str, pages: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forge-fetch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut ix = forge_search::index::Index::new();
        for (url, title, body) in pages {
            ix.add(url, title, "", body);
        }
        let path = dir.join("search-index.bin");
        ix.save(&path).unwrap();
        path
    }

    /// The wording of a refusal, on both paths it can arrive by.
    ///
    /// Checked as text because that text is the whole point: it has to tell an
    /// agent that retrying is futile, which is the opposite of what every
    /// other failure here says.
    #[test]
    fn a_refusal_says_not_to_retry_and_why() {
        let out = refused("https://walled.test/p", "Cloudflare", Some(403));
        assert!(out.contains("refused by a bot check (Cloudflare)"), "{out}");
        assert!(out.contains("status 403"), "{out}");
        assert!(out.contains("address is fine"), "{out}");
        assert!(out.contains("no retry"), "{out}");
        assert!(out.contains("ask") && out.contains("user"), "{out}");
    }

    /// The 200 case has to read differently, because "status 200" on its own
    /// would look like success.
    #[test]
    fn a_refusal_served_as_200_says_so() {
        let out = refused("https://walled.test/p", "Cloudflare", None);
        assert!(out.contains("status 200"), "{out}");
        assert!(out.contains("in place of the page"), "{out}");
    }

    /// Ignored for the same reason as the other network tests. It is here
    /// because the detection was written against one real response and this
    /// is how to check it still matches: yesterdaystractors.com serves a
    /// Cloudflare interstitial with `cf-mitigated: challenge` on a 403.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn live_a_walled_site_is_reported_as_refused_not_missing() {
        let args = json!({
            "url": "https://www.yesterdaystractors.com/",
            "prompt": "anything at all",
        });
        let out = web_fetch(&args, None, None).await.expect("call");
        println!("{out}");
        assert!(out.contains("refused by a bot check"), "{out}");
        assert!(!out.contains("Request failed with status"), "a refusal was reported as a plain failure: {out}");
    }

    /// A guessed path is the usual reason a fetch fails, and the index
    /// usually holds the real ones. Measured over five headless runs on one
    /// question: 21 of 35 `web_fetch` calls returned 404, every one a URL the
    /// model invented, while every useful fetch used a URL `web_search` had
    /// returned. So a failed call should say what would have worked.
    #[test]
    fn a_failed_guess_is_answered_with_real_urls_from_that_host() {
        let path = index_with("guess", &[
            ("https://myfordtractors.com/tune.shtml", "Tune Up and Maintenance",
             "motor oil straight 30 weight for temperatures above ninety degrees"),
            ("https://myfordtractors.com/backhoe.shtml", "Backhoe Project", "front end loader axle"),
            ("https://elsewhere.test/oil", "Oil", "motor oil weight temperature"),
        ]);
        let out = suggest_real_urls(
            &path,
            "https://myfordtractors.com/lubrication.shtml", // never existed
            "engine oil weight and temperature recommendations",
        );
        assert!(out.contains("tune.shtml"), "the real page was not suggested: {out}");
        assert!(
            !out.contains("elsewhere.test"),
            "a different host was suggested: {out}",
        );
        assert!(out.contains("Do not guess"), "{out}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// When the host has never been read there is nothing to check against,
    /// and the useful advice is different — crawl it first.
    #[test]
    fn an_unread_host_is_told_to_search_first() {
        let path = index_with("unread", &[("https://other.test/p", "P", "text")]);
        let out = suggest_real_urls(&path, "https://unread.test/guessed.html", "anything");
        assert!(out.contains("has not read unread.test"), "{out}");
        assert!(out.contains("web_search"), "{out}");
        assert!(out.contains("sites="), "it should say how: {out}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// No index, or an unreadable one, must not turn a 404 into an error.
    #[test]
    fn a_missing_index_costs_nothing() {
        let out = suggest_real_urls(
            std::path::Path::new("/nonexistent/forge/index.bin"),
            "https://a.test/x",
            "anything",
        );
        assert!(out.is_empty(), "{out}");
    }
    /// Ignored for the same reason as the search above. This one passes today,
    /// which is exactly why it should not be a gate: it depends on a page on
    /// someone else's server keeping its wording, and it fails on a train.
    #[tokio::test]
    #[ignore = "hits the live web; depends on rust-lang.org's content"]
    async fn test_web_fetch_live() {
        let args = json!({"url": "https://www.rust-lang.org/", "prompt": "What is Rust?", "max_length": 2000});
        let result = web_fetch(&args, None, None).await.unwrap();
        println!("FETCH RESULT:\n{}", result);
        assert!(
            result.contains("web_content"),
            "Should contain web_content wrapper"
        );
        assert!(result.contains("Rust"), "Should contain Rust");
    }
}

#[cfg(test)]
mod user_agent_tests {
    /// Forge identifies itself when it fetches a page. It used to claim to be
    /// Chrome 120 on macOS, in two places — the reqwest client and a `curl`
    /// fallback — which is how a scraper gets served, and also a lie told to
    /// someone else's server. This project declines to identify as a client it
    /// is not elsewhere; the same rule applies here.
    #[test]
    fn forge_does_not_claim_to_be_a_browser() {
        let src = include_str!("web.rs");
        // Needles assembled rather than written out: a scan for a literal that
        // this test also spells would find itself and fail forever.
        for needle in [concat!("Mozilla", "/5.0"), concat!("Chrome", "/1"), concat!("Apple", "WebKit")] {
            assert!(!src.contains(needle),
                "a spoofed browser user-agent is back in web.rs ({needle})");
        }
        assert!(super::FORGE_USER_AGENT.starts_with("forge-agent/"),
            "the user-agent should say what this is: {}", super::FORGE_USER_AGENT);
    }
}
