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

pub async fn web_fetch(
    args: &serde_json::Value,
    summarizer: Option<(&ApiClient, &str)>,
) -> Result<String> {
    let url = args["url"].as_str().context("Missing 'url' argument")?;
    let prompt = args["prompt"]
        .as_str()
        .context("Missing 'prompt' argument")?;
    let max_length = args
        .get("max_length")
        .and_then(|v| v.as_u64())
        .unwrap_or(20000) as usize;

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

    if !response.status().is_success() {
        return Ok(format!(
            "Error: Request failed with status {}",
            response.status()
        ));
    }

    let final_url = response.url().to_string();
    let html = response
        .text()
        .await
        .context("Failed to read response body")?;

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
    /// Ignored for the same reason as the search above. This one passes today,
    /// which is exactly why it should not be a gate: it depends on a page on
    /// someone else's server keeping its wording, and it fails on a train.
    #[tokio::test]
    #[ignore = "hits the live web; depends on rust-lang.org's content"]
    async fn test_web_fetch_live() {
        let args = json!({"url": "https://www.rust-lang.org/", "prompt": "What is Rust?", "max_length": 2000});
        let result = web_fetch(&args, None).await.unwrap();
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
