fn main() {
    use forge_search::xml;
    let s = std::fs::read_to_string("/tmp/s.xml").unwrap();
    println!("hitCount = {:?}", xml::child_text(&s, "responseWrapper")
        .map(|_| ()).and_then(|_| {
            let w = xml::elements(&s, "responseWrapper");
            xml::child_text(w[0], "hitCount")
        }));
    let results = xml::elements(&s, "result");
    println!("results: {}", results.len());
    for r in &results {
        println!("\n  title   {:?}", xml::child_text(r, "title").map(|t| t.chars().take(70).collect::<String>()));
        println!("  pmcid   {:?}  doi {:?}", xml::child_text(r, "pmcid"), xml::child_text(r, "doi"));
        println!("  OA      {:?}  license {:?}  year {:?}",
            xml::child_text(r, "isOpenAccess"), xml::child_text(r, "license"), xml::child_text(r, "pubYear"));
        println!("  abstract {} chars", xml::child_text(r, "abstractText").map_or(0, |a| a.len()));
        // journalInfo is nested: journalInfo > journal > title
        let j = xml::child(r, "journalInfo").and_then(|ji| xml::child(ji, "journal")).and_then(|jj| xml::child_text(jj, "title"));
        println!("  journal {:?}", j);
    }

    let ft = std::fs::read_to_string("/tmp/ft.xml").unwrap();
    println!("\n=== JATS ({} bytes) ===", ft.len());
    let front = xml::elements(&ft, "front");
    let at = front.first().and_then(|f| {
        let tg = xml::elements(f, "title-group");
        tg.first().and_then(|t| xml::child_text(t, "article-title"))
    });
    println!("article-title: {:?}", at.map(|t| t.chars().take(80).collect::<String>()));
    let abs = xml::elements(&ft, "abstract");
    println!("abstract: {} sections, first {} chars", abs.len(), abs.first().map_or(0, |a| xml::text(a).len()));
    let body = xml::elements(&ft, "body");
    let body_text = body.first().map(|b| xml::text(b)).unwrap_or_default();
    println!("body text: {} chars, {} terms", body_text.len(), forge_search::tokenize::terms(&body_text).len());
    let low = body_text.to_lowercase();
    for w in ["temperature", "acsf", "clamp", "°c"] { println!("   {w:?}: {}", low.matches(w).count()); }
    if let Some(i) = low.find("temperature") {
        println!("\ncontext: …{}…", &body_text[i.saturating_sub(200)..(i+220).min(body_text.len())]);
    }
    println!("\nsection titles:");
    for sec in xml::elements(&ft, "sec").iter().take(6) {
        println!("   {:?}", xml::child_text(sec, "title"));
    }
}
