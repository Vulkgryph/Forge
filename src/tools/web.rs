// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use scraper::{Html, Selector};

use crate::api::{ApiClient, Message};

fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

pub async fn web_search(args: &serde_json::Value) -> Result<String> {
    let query = args["query"].as_str().context("Missing 'query' argument")?;
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .min(20) as usize;

    let html = fetch_duckduckgo_html(query).await?;
    let results = parse_duckduckgo_results(&html, max_results);

    if results.is_empty() {
        let page_text = collapse_whitespace(
            &Html::parse_document(&html)
                .root_element()
                .text()
                .collect::<String>(),
        )
        .to_lowercase();

        return Ok(classify_ddg_empty_response(query, &page_text));
    }

    let mut output = format!(
        "Search results for '{}' ({} results):\n\n",
        query,
        results.len()
    );
    for (i, (title, url, snippet)) in results.iter().enumerate() {
        output.push_str(&format!("{}. {}\n", i + 1, title));
        if !url.is_empty() {
            output.push_str(&format!("   URL: {}\n", url));
        }
        if !snippet.is_empty() {
            output.push_str(&format!("   {}\n", snippet));
        }
        output.push('\n');
    }

    Ok(format!(
        "<web_content source=\"duckduckgo search: {}\">\n{}</web_content>",
        query, output
    ))
}

async fn fetch_duckduckgo_html(query: &str) -> Result<String> {
    // Try direct DuckDuckGo first. If it returns a bot challenge page, fall back
    // to r.jina.ai's mirrored fetch of DuckDuckGo HTML results.
    let direct = tokio::process::Command::new("curl")
        .args(&[
            "-s", "-X", "POST",
            "https://lite.duckduckgo.com/lite/",
            "-H", "User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            "-d", &format!("q={}", urlencod(query)),
            "--max-time", "10",
            "-L",
        ])
        .output()
        .await
        .context("Failed to run curl for DuckDuckGo search")?;

    if !direct.status.success() {
        let stderr = String::from_utf8_lossy(&direct.stderr);
        return Ok(format!("Error: Search request failed: {}", stderr.trim()));
    }

    let direct_html = String::from_utf8_lossy(&direct.stdout).to_string();
    let direct_page_text = collapse_whitespace(
        &Html::parse_document(&direct_html)
            .root_element()
            .text()
            .collect::<String>(),
    )
    .to_lowercase();

    if !is_ddg_challenge_page(&direct_html, &direct_page_text) {
        return Ok(direct_html);
    }

    let fallback_url = format!(
        "https://r.jina.ai/http://html.duckduckgo.com/html/?q={}",
        urlencod(query)
    );

    let fallback = tokio::process::Command::new("curl")
        .args(&["-s", &fallback_url, "--max-time", "20", "-L"])
        .output()
        .await
        .context("Failed to run curl for DuckDuckGo fallback search")?;

    if !fallback.status.success() {
        return Ok(direct_html);
    }

    let fallback_body = String::from_utf8_lossy(&fallback.stdout).to_string();
    if fallback_body.trim().is_empty() {
        return Ok(direct_html);
    }

    Ok(fallback_body)
}

fn parse_duckduckgo_results(html: &str, max_results: usize) -> Vec<(String, String, String)> {
    if html.contains("URL Source:") && html.contains("Markdown Content:") {
        let markdown_results = parse_jina_markdown_results(html, max_results);
        if !markdown_results.is_empty() {
            return markdown_results;
        }
    }

    let document = Html::parse_document(html);
    let mut results = Vec::new();

    let primary_link_sel = Selector::parse("a.result-link").unwrap();
    let primary_snippet_sel = Selector::parse("td.result-snippet").unwrap();

    let primary_links: Vec<_> = document.select(&primary_link_sel).collect();
    let primary_snippets: Vec<_> = document.select(&primary_snippet_sel).collect();

    for i in 0..primary_links.len().min(max_results) {
        let title = collapse_whitespace(&primary_links[i].text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let url = primary_links[i]
            .value()
            .attr("href")
            .unwrap_or("")
            .to_string();
        let snippet = primary_snippets
            .get(i)
            .map(|el| collapse_whitespace(&el.text().collect::<String>()))
            .unwrap_or_default();

        if !url.is_empty() {
            results.push((title, url, snippet));
        }
    }

    if !results.is_empty() {
        return results;
    }

    let generic_link_sel = Selector::parse("a[href]").unwrap();
    let fallback_snippet_sel =
        Selector::parse("td.result-snippet, .result-snippet, .result__snippet").unwrap();
    let generic_links: Vec<_> = document.select(&generic_link_sel).collect();
    let fallback_snippets: Vec<_> = document.select(&fallback_snippet_sel).collect();

    for link in generic_links {
        if results.len() >= max_results {
            break;
        }

        let href = link.value().attr("href").unwrap_or("");
        let title = collapse_whitespace(&link.text().collect::<String>());

        if href.is_empty() || title.is_empty() {
            continue;
        }

        if href.starts_with('#')
            || href.starts_with('/')
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
        {
            continue;
        }

        if href.contains("duckduckgo.com") && !href.contains("uddg=") {
            continue;
        }

        let snippet = fallback_snippets
            .get(results.len())
            .map(|el| collapse_whitespace(&el.text().collect::<String>()))
            .unwrap_or_default();

        results.push((title, href.to_string(), snippet));
    }

    results
}

fn parse_jina_markdown_results(body: &str, max_results: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    let mut lines = body.lines().peekable();

    while let Some(line) = lines.next() {
        if results.len() >= max_results {
            break;
        }

        let line = line.trim();
        if !line.starts_with("## [") {
            continue;
        }

        let Some(title_end) = line.find("](http") else {
            continue;
        };
        let title = line[4..title_end].trim().to_string();
        let url_start = title_end + 2;
        let Some(url_end_rel) = line[url_start..].find(')') else {
            continue;
        };
        let url = line[url_start..url_start + url_end_rel].trim().to_string();

        if title.is_empty() || url.is_empty() {
            continue;
        }

        let mut snippet = String::new();
        while let Some(next_line) = lines.peek() {
            let trimmed = next_line.trim();
            if trimmed.is_empty() {
                lines.next();
                if !snippet.is_empty() {
                    break;
                }
                continue;
            }
            if trimmed.starts_with("## [") {
                break;
            }
            if trimmed.starts_with("[") && trimmed.contains("](http") {
                lines.next();
                continue;
            }

            if snippet.is_empty() {
                snippet = collapse_whitespace(trimmed);
            }
            lines.next();
        }

        results.push((title, url, snippet));
    }

    results
}

fn is_ddg_challenge_page(html: &str, page_text: &str) -> bool {
    let html_lower = html.to_lowercase();

    page_text.contains("captcha")
        || page_text.contains("unusual traffic")
        || page_text.contains("robot")
        || page_text.contains("verify you are human")
        || page_text.contains("bots use duckduckgo too")
        || page_text.contains("confirm this search was made by a human")
        || page_text.contains("select all squares containing")
        || page_text.contains("challenge-form")
        || page_text.contains("anomaly")
        || html_lower.contains("challenge-form")
        || html_lower.contains("/anomaly.js")
        || html_lower.contains("name=\"vqd\"")
}

fn classify_ddg_empty_response(query: &str, page_text: &str) -> String {
    if page_text.contains("no results") || page_text.contains("no  results") {
        format!("No results found for: {}", query)
    } else if page_text.contains("captcha")
        || page_text.contains("unusual traffic")
        || page_text.contains("robot")
        || page_text.contains("verify you are human")
        || page_text.contains("blocked")
        || page_text.contains("bots use duckduckgo too")
        || page_text.contains("confirm this search was made by a human")
        || page_text.contains("select all squares containing")
        || page_text.contains("challenge-form")
        || page_text.contains("anomaly")
    {
        format!(
            "Error: DuckDuckGo search appears to have been blocked or challenged for query: {}",
            query
        )
    } else {
        format!(
            "Error: DuckDuckGo search returned an unexpected page format for query: {}",
            query
        )
    }
}

/// Simple URL encoding for query strings.
fn urlencod(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push('%');
                result.push_str(&format!("{:02X}", b));
            }
        }
    }
    result
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

    #[tokio::test]
    async fn test_web_search_live() {
        let args = json!({"query": "rust programming language", "max_results": 3});
        let result = web_search(&args).await.unwrap();
        println!("SEARCH RESULT:\n{}", result);
        assert!(
            result.contains("web_content"),
            "Should contain web_content wrapper"
        );
        assert!(!result.contains("No results found"), "Should find results");
    }

    #[tokio::test]
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
