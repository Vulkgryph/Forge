# Changelog

All notable changes to Forge are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Forge adheres to [Semantic Versioning](https://semver.org/).

## [0.5.1] — 2026-09-25

### Fixed

- **An approved plan could leave the agent with nothing it was allowed to do.** Approving a plan without clearing context cleared the plan-mode flag but left the "PLAN MODE ACTIVE — you CANNOT modify files" directive standing in the transcript. The flag decides which tools are offered; the directive is what the model reads. With the flag down, `exit_plan_mode` was no longer on offer — so the model was told it must not write anything, and the one tool that would have let it say otherwise was gone. It refused to implement the plan it had just written, asked to be switched out of a mode it was no longer in, and was right on both counts.

  Three of the four exit paths cleared the flag by hand and two forgot the directive. They all go through one place now, and a test fails if a new exit path clears the flag on its own.

### Changed

- **A rejected credential now says where it came from.** A 401 from the Responses API named the key the provider disliked but nothing about which of the backend's several construction paths supplied it, so diagnosing one meant re-tracing every caller by hand. The error now carries the endpoint, the model, whether the account header went out, and the credential's *shape* — kind, length, and a six-character prefix, which is no more than the provider already echoes.

  `FORGE_AUTH_DEBUG=1` traces the same line on every Codex request, for catching the moment a working credential turns into a failing one. The model-catalog fetch, which used to swallow every failure and merely look like an empty catalog, now reports auth failures too.

- **ChatGPT Codex no longer mints an API key nobody asked for.** Forge exchanged your OAuth `id_token` for an `sk-` API key — on login, on every token refresh, and on every token check — wrote it to `chatgpt_auth.json`, and then preferred it over the OAuth token for one request. The backend that accepts the token rejects that key, so the failure read `Incorrect API key provided: sk-svcacct…`: a credential the user never supplied, cannot find in any config, and did not know existed.

  It only broke when the mint *succeeded*, which is why it came and went rather than failing consistently. The one request that used it — listing available models — now uses the OAuth token like everything else, verified live against the backend.

  Nothing mints the key any more, and a key left by an older build is dropped when the token file is read, so it leaves disk on the next save. A credential nobody asked for should not sit in a file.

- **Both user agents carry a contact URL**, in the `+`-prefixed form a crawler is expected to use: `forge-search/0.5.0 (+https://vulkgryph.com/projects/forge/)`, and the same for `forge-agent/`. Saying what you are is only half of it — a site operator seeing an unfamiliar name in their logs wants to know who it is and how to reach them, and a bare name does not answer that. The alternative to being findable is being blocked by reputation, which this project already declined when it stopped pretending to be a browser.

## [0.5.0] — 2026-09-23

### Fixed

- **A bot check on `robots.txt` is no longer read as permission to crawl.** Cloudflare answers `stackoverflow.com/robots.txt` with status **418** and the real file in the body. 418 falls inside "permanently gone", so the rules were discarded as absent — and a file reading `User-agent: * / Disallow: /` was treated as no restrictions at all. Forge crawled a site that forbids it, then reported the refusal it got as a bot check rather than as the site's own stated wish.

  RFC 9309 does permit reading a 4xx as unrestricted, and for a genuine 404 that is right. A refusal is not a 404: it is the site answering, not staying silent. A challenged `robots.txt` now denies, and only 404 or 410 counts as absent — other refusals (401, 403, 451) mean the site declined to show its rules, which is unreadable rather than unrestricted.

  When the refused response carries rules anyway, they are parsed and honoured, because it often does and respecting what a site actually said beats assuming. On Stack Overflow that reaches the same answer either way: the crawler now stops at `robots.txt` and never requests a page.

- **One page from a site no longer makes the agent claim it can read the whole site.** A bot check refused a crawl of `stackoverflow.com`; a person opened the page in the browser and handed it over, which put exactly one page in the index. From then on the host counted as *read*, so naming it skipped the crawl entirely and the tool answered from that single page — and the agent reported that it could still reach the site without help. It could not. It was reading what it had been given.

  A single `web_fetch` did the same thing. A host now counts as read only once the index holds at least five pages from it, which tells a crawl from an incidental page or two. A genuinely tiny site that falls under the line is merely re-crawled — a few seconds, and it refreshes what is held, since re-crawling replaces pages by URL rather than duplicating them.

  Nothing is lost from the handoff: the page stays in the index and still answers queries. What goes away is the false claim of access — the crawl is attempted, the refusal is reported honestly, and the browser handoff is offered again.

- **A page handed over from the browser actually resumes the work.** The last step of the bot-check rail threw its own result away. `absorb_browser_result` indexed the page and then sent an `AssistantMessage`, which is *display* — it reaches the transcript and never the conversation — so the model was never told the page existed and no turn started. Somebody cleared a challenge by hand, pressed **Send to agent**, watched a line appear, and nothing happened.

  The notice now goes into the conversation as a user message and runs a turn, exactly the way a finished background command does and for the reason that arm already documents: a user message is new information, while a tool result would need a matching tool call. A turn of its own is required because the page arrives *after* the turn that wanted it — the agent deliberately does not block on a person, it carries on with other sources and the request is withdrawn when the turn ends — so without one, the work somebody just did by hand reaches nothing.

  The notice also names the host to scope a query to, since the page is in the index under its own URL and `sites` narrows by host.

- **A crawl's refused pages are offered to a person, instead of only described to the model.** The browser handoff had one caller. `web_fetch` queued its refusals from the start, so a single blocked page raised a browser request — but `web_search`, which is where challenges actually happen, put its refusals in the crawl report and the tool rendered them as prose. The result told the model *"those pages need a browser… ask the user to open the page"* while queueing nothing, so no browser was ever offered and there was no page for the user to open.

  Every piece of the rail worked. Nothing called into it: the detection, the queue, the request, the withdrawal, the IDE's tab, the page handed back. The feature was built, reduced to challenge-handling only, and shipped twice before a real run against `stackoverflow.com` showed the challenge detected, reported, and silently dropped.

  Three tests now cover it, and each was checked by removing the fix and watching it fail. A unit test on the queueing; a structural one asserting that the crawl path which records challenges *for display* also offers them *to a person*, since the bug was a correct function nobody called; and a live one against `stackoverflow.com`, which is the only check that would have caught the original gap.

### Fixed

- **A command that was not waiting for input stops asking.** `cargo test -- --nocapture` streams a test's own `println!`s, and a test that prints a label before a slow assertion — `global storage:`, `struct-layout:` — produces a line ending in a colon followed by silence. That is exactly the shape of `Password:`, so it raised a real "Input needed" dialog over a test run.

  No reading of the text can separate those two; a list of prompt words would only move the false positives around. So the guess is retracted instead. The agent already worked out it had guessed wrong — output resuming meant the process was never blocked, and it reset its own flags and carried on — but it never told the client, so the dialog stayed up asking for input on behalf of a command that had moved on. Ignorable, which is how people learn to ignore the ones that are real.

  A new `process_input_withdrawn` message says so, sent when output resumes or the process exits. The evidence is the process carrying on, which is stronger than anything a heuristic could read off its text. The candidate also has to survive two seconds of silence now rather than 350 ms: the window is not a latency budget, since a person takes seconds to read a prompt and longer to answer, so offering input 1.65 s later costs nothing anybody can feel — and the pauses ordinary output takes between assertions or crates are short, while a process genuinely blocked on stdin is silent until answered.


### Added

- **`search_documents` — ranked retrieval over a folder of documents on this machine.** The engine's value was only reachable through a network fetch, which put it out of reach of the corpus most people actually have: a directory of reports, runbooks, notes or papers on their own disk. Nothing about an inverted index cares whether the words arrived over a socket.

  Pass `paths` with a folder and it reads every prose document under it — `.md`, `.txt`, `.rst`, `.org`, `.html` — keeps a local index, and answers from it; pass only `query` and it reads nothing and answers from what is already there.

  **A document is indexed as its sections, not as a file**, and that is what makes a result worth having to an agent. A file-per-document was wrong three ways at once: ranking normalises by length, so a three-thousand-line changelog mentioning a term once scored as a weak match while the same term scattered across twenty unrelated entries scored as a strong one; the title came from the top of the file, so a passage from deep inside was labelled "Changelog" — a real run returned three results with that title and nothing to tell them apart; and the answer was a snippet plus an implicit instruction to go and read the whole file, which is the expensive part, since context is the budget.

  So each section carries the heading trail leading to it and the lines it occupies. A result now reads `Runbook › Recovery › Restarting the agent`, `lines 120-186`, and the `read_file` call that fetches exactly that — sixty lines rather than three thousand. Boundaries are the document's own headings rather than a fixed word count, which is also why no overlapping windows are needed: overlap exists to stop a fixed-size cut landing mid-answer, and a cut that only lands on a heading has much less to protect against. Stub sections group so a changelog of forty one-line entries does not become forty documents that each answer nothing, and a section with no headings and three thousand words splits at paragraph boundaries so one badly structured file cannot dominate.

  Results also spread across **files** rather than hosts, because a `file://` URL has no host — without that, one long document's five best sections would take every slot.

  **Build output and vendored dependencies are not walked.** This was found by running it: indexing this repository took 13.1 seconds, of which 13 were spent walking `target/` — 83 GB across 663,234 files, holding ten readable documents — at one `symlink_metadata` syscall each. Skipping `target`, `node_modules`, `vendor`, `venv`, `__pycache__`, `build` and `dist` brought the same run to 105 ms. It is a heuristic, so it applies only to directories the walk discovers: one named in `paths` is read whatever it is called, and every skipped directory is reported rather than silently dropped.

  **Files unchanged since they were last read are skipped**, and that is not only about time. Re-adding a file marks its old document dead, and enough dead documents trigger a full rewrite of the index — so without this, re-indexing an untouched tree would rewrite the whole index to produce exactly what was already in it. Freshness comes from comparing the file's mtime against the segment that holds it, so it needs no extra field on disk.

  Deliberately **not** for source code: `search_code` greps, and for an identifier or an error string that is exact, needs no index and cannot go stale. This earns its place on the different question — which of three thousand documents answers this — which grep cannot answer at all, since it returns every file containing the word in filesystem order and misses the one that wrote "Windows Management Instrumentation" when the query said "WMI".

  Tabular and record formats are left out on purpose rather than forgotten. A CSV of fifty thousand alerts is fifty thousand documents, not one, and indexing it whole would produce a single document that matches every query and answers none of them; doing it properly means a record-level ingester, which is a different design.

  Its index is separate from `web_search`'s, at `.forge/doc-index/`, because BM25 scores a term by how rare it is in the corpus being searched. Five hundred crawled pages where "bucket" appears on four hundred of them make the word worthless as a discriminator, and three of your own incident reports that mention a bucket would then be scored as though it meant nothing. Kept apart, those three win outright. Length normalisation fails the same way in reverse: mixing two-thousand-term reference pages with two-hundred-term notes marks the notes down for being the length that notes are.

- **`search_papers` — the biomedical literature, through the channel published for it.** PubMed Central's `robots.txt` is `User-agent: *` / `Disallow: /`, so the primary literature cannot be crawled. That is why the engine answered nothing for a question about a measured value at a stated temperature: those numbers are in Methods sections, and no amount of crawling encyclopaedias reaches one.

  The new tool asks Europe PMC's REST API, which is the documented programmatic channel and on a different host from the website. It searches, fetches the full text of articles whose licence permits keeping it, indexes that, and answers from it. Anything not open access comes back as a citation and a link, and the result says so rather than leaving the model to assume there was nothing.

  Only open-access articles have their text kept — more conservative than necessary, on purpose, because it is one sentence to state and one condition to audit. Every result reports the licence its text is held under: `cc by` wants attribution, `cc by-nc` excludes commercial use, and the index now carries that with the document so it survives a save.

  Measured live: 2,636 matching articles, 9 examined, 3 indexed, 1 refused on licence, in 3.4 seconds. It then answers out of a Methods section — "We performed electrophysiological recordings at room temperature (20°C–25°C), but the recording chamber might be heated to near-physiological temperatures using a bath-controller" — which is what none of this could do before.

### Changed

- **Several pages handed over together become one turn, not one each.** A site that refuses the crawler can still be read by a person: clear the check, click through, and share what matters — which already worked, since nothing blocks a repeat share and the tab stays open after the agent's turn ends. What it cost was a model call per page, each turn told about one page, with the agent re-deciding what to do in between.

  A delivered page now absorbs anything already queued behind it and reports the batch once. Non-blocking, so it collects only what is already waiting rather than guessing whether somebody is still browsing. Anything on the channel that is not a page is parked rather than eaten — that loop owns the action channel for a moment, and that is exactly how a background command's completion was lost once before.

  Which also found a hole in the test that guards against it: `no_action_consumer_silently_discards` looked for `action_rx.recv()` and for bare `_ =>` arms, so a non-blocking drain written `Ok(_) => {}` passed it. It now sees `try_recv` and the `Ok`/`Some` spellings of a catch-all.

- **A page the user handed over is reachable by `web_fetch`, not only by `web_search`.** Clearing a bot check is a person deciding to share a page. Once they have opened it and pressed the button the content is here, and the agent should reach it with whichever tool it was going to use. `web_search` already could, because it reads the index. `web_fetch` went straight to the network, was refused again, and reported the page unreachable while holding a copy of it in the index beside it.

  It now falls back to the provided copy when — and only when — a bot check refuses the fetch. That order matters: a site that will serve the page should be asked for it, because the live copy is the current one. The result says where the content came from, and that other pages on the site are still behind the check, so the agent does not conclude the whole host opened up.

  Nothing is fetched on that path, so nothing is bypassed: no cookie is replayed, no identity is claimed, and no request reaches the site. `robots.txt` governs what a crawler may go and take; it does not govern what a person chose to read and pass on. That is why this route needs a human at the start of it, and why it is the one way through a refusal that is honest.

- **One data file can no longer swamp the document index, and sections of an untitled file are told apart.** Found by pointing `search_documents` at a machine-learning project: `data/wikitext-103/test_articles.txt` is a hundred thousand words of Wikipedia with no headings, so it divided into 380 sections, and four such files turned 303 documents into 1,974. A query about training a network was answered with the same title twice, from two datasets.

  A file may now contribute at most 50 sections — roughly seventeen thousand words, a long document by any measure — and what was dropped is reported by name, with a pointer to `search_code` for a file that really is data. On that project it took the index from 1,974 sections to 754 and halved the time to build it. Indexing a corpus as though it were notes does not make it findable; it makes everything else less so.

  Plain text has no headings, so every section fell back to the document's own name and they were indistinguishable in a result list — and one rendered as `" (part 2)"`, a part number and nothing else, because the empty heading trail had been joined with it. The part number now composes with whatever name the caller has, and a nameless part reads as `part 2` rather than as leading whitespace.

- **Local results spread across directories, and divided sections say which part they are.** Both found by running `search_documents` over a real corpus — 176 markdown notes organised as `<topic>/<aspect>.md` — after the well-structured corpus it was built against showed neither.

  Asked "new testament manuscript evidence", three of five slots went to one topic folder: its arguments, its texts and its textual reliability, three files saying one thing from one point of view, while the page most directly on the subject was pushed to fourth. Each result was relevant and together they were a monoculture. Results were spread by *file*, which treats one author's several notes as independent sources. A directory in a document tree is what a host is on the web — the best available proxy for one source — and with that the folder takes two slots and the displaced page rises to third. Nothing is dropped either way: past the cap results are deferred and still fill the list in score order, so a flat folder behaves exactly as before.

  A long section divided at a paragraph boundary also inherited one heading trail, so the same title appeared twice in one result list, at positions one and three, with nothing to say why. Parts are numbered now.

- **The agent knows whether the client it is talking to can open a page, and stops giving advice that only works in one of them.** It offered the browser handoff regardless. In the IDE that is right; in a terminal it is a dead end — there is nothing to show a page in and nothing to read one back from — so the agent sent a person off to do something impossible and then waited for a result that could not arrive.

  The host declares it at spawn with `--host-can-browse`. A flag rather than a protocol message, because of ordering: a capability that arrives over the wire can arrive *after* the agent has already been refused a page, and the advice it gave was then based on not knowing. Spawn time is the one moment this is certainly known and cannot change. Named for the capability rather than the client, because the agent has no business knowing whether it is an editor or a terminal — only whether asking somebody to open a page is a real option.

  Forge IDE passes it on both spawn paths, local and over SSH: the capability belongs to the client rather than the machine the agent runs on, since a refused page is a public URL that the IDE's own browser can fetch and hand back over the same protocol. The terminal client passes nothing, and defaults are off — a host that forgets gets told a page cannot be opened, which is merely pessimistic, where the other default promises a handoff that never comes.

  With no browser, nothing is queued at all: a request no client can satisfy is worse than no request. The agent says plainly that the site refused an automated request, names it so the user can look themselves, and answers from what else it has.

- **Coverage is weighted by how rare each query word is, so a near miss has to actually be near.** It counted words equally, which made the number say the opposite of what it means. Asked "how does the agent handle a cloudflare bot check", a section about shell timeouts matched `how`, `does`, `the`, `agent`, `handle` and `check` while missing only `cloudflare` — six words of seven — so it reported 0.75 coverage and sat in the results looking like a close call. It was not a close call. One word was the question and the rest was grammar. Weighted by inverse document frequency, that section reports 0.35 and the decay below the real answer is steep instead of gradual.

  Terms the corpus contains nowhere are still excluded from the denominator, which is a separate and still-correct fix: a word in no document cannot separate one document from another.

- **A result says which of the query's words the index has never seen.** The thing no result can say for itself. A query whose subject is absent still matches on its grammar, and every number attached to it is then truthful and useless — asked "what oil does a tractor take" of a corpus with no `oil` and no `tractor` in it, the engine matched `what`, `does`, `a` and `take`, scored a perfect coverage because those were genuinely everything findable, and answered with five sections about window management.

  Both search tools now name the words that drew a blank, which turns a wrong answer into an obviously wrong one and distinguishes "nothing here answers that" from "this corpus has never heard of the thing you asked about". Stop words are excluded: `the` missing from an index would be a fact about the index rather than the question.

  Three earlier attempts at this were wrong, and each was refuted by measurement rather than by argument. Requiring the query's rarest term would have excluded the right answer in an existing labelled case, where the page that answers says "30-weight" and the query said "SAE 30" — the widening exists for exactly that. A relative score cutoff had nothing to cut at, since the unanswerable query's results decayed from 100% to 51% with no cliff. And an "at least one informative term" floor still passed the garbage, because `take` legitimately appears in only 9 of 134 sections.

- **`web_search` is a crawler with a library, not a search engine.** It was described as "Search the web", which it cannot do: it has no index of the web and no way to discover a site nobody pointed it at. A tool whose description promises more than it does gets called wrongly and then blamed for the result. It now says plainly that deciding where to look is the caller's job, that naming sites is how it learns anything, and that a first call on a site is slow while every later one is instant and offline.

  Naming `sites` also **narrows the answer to them**. Before, every page Forge had ever read competed in one ranking — so the hundred and twenty pages of Rust documentation a misaimed crawl left behind stayed in the running for every question afterwards. The real cost of aiming badly was not the wasted minutes, it was that the mistake outlived them. A host is matched however it was spelled, since the caller is a model repeating back what somebody typed: `docs.example`, `https://www.docs.example/page` and `DOCS.EXAMPLE` all narrow to the same shelf.

  And a result that finds nothing now **says what has been read** — site, page count, how long ago — instead of only "no results". A dead end that makes the model guess a site costs a two-minute crawl; one that lists the shelves lets it pick from them, or conclude honestly that nothing on hand can answer.

- **The search index is a directory of append-only segments, so growing it no longer rewrites it.** The index was one file, written in full on every save. At the fifty-thousand-page cap that is 556 MB written to add sixty pages, and a crawler saving each batch would have written on the order of 35 TB a day — a consumer SSD is rated for 300–600 TB in total, so a fortnight of that ends the drive.

  A save now writes only what it added, as a new file, and appends a line to a small text manifest naming the segments in order. Measured on the scale benchmark: adding sixty pages writes 0.98 MB whether the index holds a thousand pages or fifty thousand, against 11.7 MB and 556.8 MB for the old full rewrite. The figure is flat because the cost tracks what was added rather than what was already there, which is the whole property.

  Segments are never edited, so removal is expressed by name: a segment carries the URLs it retires, and a load applies them. Compacting on removal was the first attempt and it defeated the format — refreshing one stale page removes one document, which would have made that save rewrite everything. A real compaction happens when more than a quarter of the index is dead, which is where it earns the write.

  The index lives at `.forge/search-index/` rather than `.forge/search-index.bin`; an old file is simply not found, and a missing index already means "crawl again". Verified on two live crawls: the first segment comes back byte for byte after the second save, and queries answer out of both.

- **`web_search` is on by default, and no longer describes itself as broken.** Its schema still told the model it scraped DuckDuckGo and was "UNRELIABLE", with instructions not to retry. All true of the scrape; none of it true of the index Forge now crawls itself, and left in place it steered the model away from a tool that works. The description now says what the tool is, and documents `sites` and `max_pages` — which existed and were undocumented, so the model could not aim a crawl.

  The config default that disabled it is gone too; its own note said "off until there is a real search behind it", and there is. This reaches fresh installs only: the app serialises `disabled_tools`, so anyone who has run Forge before has `["web_search"]` on disk, which is indistinguishable from having chosen it and is therefore left alone. The tools menu turns it back on.

## [0.4.2] — 2026-09-14

### Fixed

- **A background command's result is actually delivered.** `shell_exec` tells the model "the result will be delivered automatically when it finishes" — and it was not. Ten places in the agent receive from the action channel (the streaming select, each approval wait, the `shell_exec` loop), and every nested one ended in a catch-all that *consumed and discarded* whatever it did not recognise. A background command's completion arrives while the model is still working, which is the normal case rather than a race, so it was always swallowed: the turn ended, nothing followed, and a watcher or a long build reported nothing ever. Reproduced against a real agent, where a three-second background command produced one turn and silence.

  Nested loops now park what they cannot handle, and the main loop — the only place with an arm for every action — drains that queue *before* awaiting the channel. Draining afterwards would leave a finished command waiting on whatever the user next happened to type. Verified the same way it was found: the agent now receives the output and acts on it.

  The regression test is structural rather than behavioural, because the property is: it asserts no action consumer discards silently, which catches the next one added rather than only the case exercised. A first version of it matched a bare `_ => {}` and so passed against the exact code it was written to catch — the real line carried a trailing comment.

## [0.4.1] — 2026-09-13

No changes in this component; released with `forge-ide`, which shares its version.

## [0.4.0] — 2026-09-07

_Includes everything prepared for 0.3.2. That version was written up and its manifests committed, but it was never tagged and never released, so it existed only as a commit on `main` — nobody could install it. Its notes are here rather than under a heading for a version that never shipped._

### Added

- **The agent has a working area of its own.** Everything it wrote previously landed in the user's project, so a throwaway probe script, a scratch copy of a file, or a one-off reproduction either became litter in a real repository or did not get written at all. Each session now gets a directory under the system's temporary directory, created on demand and named after the session, and the model is told where it is. Subagents are handed the same one rather than making their own, so scratch work carries between them.

  Writes into it skip the approval prompt — a scratch area that asks permission for every throwaway file is not a scratch area — and the exemption is deliberately narrow. It covers `write_file` and `edit_file`, whose target is a single named path that can be checked. `apply_patch` names its files inside the diff and can carry several at once, so it is approved as before, and `shell_exec` is untouched: a command is free to go anywhere once it is running. Copying a file *out* of the area into a real directory is an ordinary write and is approved like one.

  The containment check is the whole boundary, so it resolves paths rather than comparing strings — `lab/../../etc/passwd` starts with the area's path as text while naming somewhere else entirely — and compares by path component, so a sibling directory named `forge-lab-old` is not inside `forge-lab`.

  On Linux the base is usually `/tmp`, shared with every account on the machine, which matters more here than for an ordinary temporary file because writes land without asking. The directories are created `0700`, and one that is a symlink or belongs to another user is refused outright: left alone, a pre-planted symlink would redirect every auto-approved write the agent makes. Verified on Linux as well as macOS.

  Areas are removed by age at startup rather than on exit, because a session ends in every way a process can and one cleaned up only on a clean exit accumulates. Age is read from the newest file inside, since a directory's own mtime does not move while its contents are edited. Seven days by default; all three switches live under `[agent.scratchpad]`, and older config files still parse.

### Fixed

- **A conversation far past the context limit can recover.** Two things had to be true for a session to wedge itself, and both were. Nothing capped a single tool result — `read_file` with no line range returns the whole file — so one call could put the history several times over the window. And the emergency path that sheds oldest messages was told the current size by way of the token count from the last *successful* request, which by definition describes the conversation *before* whatever overflowed it. Measured on a history at 1432% of the window: it decided the conversation was comfortably under budget and dropped nothing at all. The turn then failed with an API error, the oversized message stayed in history, and every following turn failed the same way. The further past the limit the conversation was, the less likely it became that anything was shed.

  Size is now taken from the history's own text whenever it may have grown since the last request, so the trigger and the recovery both see what is actually there. Recovery drops oldest turns first and then shortens the largest message still left, because dropping cannot help when the oversized message is also the newest one — and it keeps both ends of what it shortens, since a file says what it is at the top while a command run says whether it passed at the bottom. Compaction's result is fitted to the window too: the summary is small, but the recent messages kept beside it are whatever they were.

  Single results are also bounded as they arrive, at a quarter of the window, so the recovery path is a backstop rather than the thing keeping the session alive. What was cut is stated in the text, so the model knows it is holding a fragment and can read the rest by range.

- **Running a test suite is no longer mistaken for a REPL.** The guard that stops an interactive program being started inside a tool call asked whether a command matched a short list of things that counted as work — `-c`, `--version`, or an argument ending in `.py`, `.rb` or `.js`. Everything else was called a REPL and refused, which caught a great deal of ordinary work: `python3 -m unittest discover -s tests` matches none of those patterns, so an agent asked to fix a failing test was told its own test command was interactive and could not verify the fix. `python3 manage.py`, `ruby -e`, and the same commands with `< /dev/null` already on them failed the same way.

  The question is now asked the other way round: an interpreter is a REPL when it is given *no* work — no module, no program, no script — and `-i` still asks for a prompt on purpose. Naming the ways an interpreter is given work is a closed set; naming the ways work can look is not.

- **The rendered task list is pinned by a test.** Forge IDE and the TUI both parse `todo_write`'s output to draw it as a checklist, and neither can import the function that writes it. Changing the markers or the indentation would have quietly turned both back into flat grey text.

## [0.3.1] — 2026-08-28

### Fixed

- **A wide character in the first message could crash the session.** Reported live: `end byte index 77 is not a char boundary; it is inside '▎'`, panicking the agent's runtime thread. A session's title is the first message cut to length, and the cut was made by byte index — `&first_msg[..77]` — which panics whenever byte 77 lands inside a multi-byte character. Nothing exotic is required to hit it: an emoji in a prompt, a CJK identifier, an accented word, or the box-drawing characters models reach for when they sketch a diagram. The same pattern turned out to be in seven places across the agent and the editor — session titles, rewind previews, conversation titles in two panels, and three LSP hover truncations — each of them cutting text a person or a model produced, each one crashable. All seven now cut on a character boundary through a shared `truncate_chars`, and count their limits in characters rather than bytes, so a cap of 80 means eighty characters as a reader would expect. A property test walks every cut point of a mixed ASCII/emoji/CJK string and asserts the result is always a valid prefix.

## [0.3.0] — 2026-08-28

### Changed (`web_search` is off by default)

- **`web_search` is off by default.** It works by scraping DuckDuckGo's HTML rather than through a search API, and usually comes back empty — a tool that usually returns nothing is worse than one that is absent, because the model spends a turn on it and then reasons about the emptiness as if it meant something. Turn it on from the tools menu in either client if you want it as it stands; that choice is written to the config and kept. `web_fetch` stays on: it retrieves a URL it has been handed and summarises it, which does not depend on search working.

### Added (forced background ceiling on every top-level shell — default 5 minutes)

- **Any still-running top-level `shell_exec` is now force-moved to the background after `agent.forced_shell_background_secs` (default `300` / 5 minutes), no matter what the model requested.** That includes `wait=true`, a huge `timeout_secs`, and the interactive-prompt heuristic pausing the normal timer. The command is **not** killed — it keeps running as `bg-N`, the turn unblocks with the usual `BACKGROUND:` tool result (poll via `background_id`, stop via `background_action=kill`, automatic `BgDone` when it finishes). Set the config value to `0` to disable (not recommended). Subagent/direct `shell_exec` still uses its own hard timeout (cannot own a parent `bg-N` slot); the hang-fix path above covers that case. Tool description updated so the model knows the ceiling exists and how to follow up on backgrounded work.

### Fixed (a stuck subagent `shell_exec` could freeze the parent chat forever)

- **The rolling window could empty the transcript instead of trimming it.** Reported from a live session: context usage fell from around 90% to around 11% in a single step and the agent had no memory of what it had been doing. The cause is a unit mismatch in `apply_rolling_window`. Its running total starts as the server's real prompt token count, but each message it dropped subtracted `tokens_per_message` — a *marginal* figure measured across the last two turns. Those are not the same measure. A session that has just exchanged a few short messages reports a small marginal cost, while the messages at the front of the history, which are the ones dropped, are the large ones: tool results and file dumps. Shedding 20k tokens then looked like it needed hundreds of drops, so the loop ran until the history was empty rather than until the budget was met. Each dropped message is now charged its own size, converted at the ratio the real total implies, with the marginal figure kept only as a floor. A test reconstructs the reported shape — a 200k window at 90%, twenty turns each holding a large tool result, and a small recent marginal cost — and pins both directions: it must shed enough to get under budget, and it must not drop more than a few turns to do it. Against the old arithmetic that test drops 40 of 42 messages.

- **A known defect is now written down**: a watcher started with `run_in_background=true` can die, or never start, while the agent proceeds as though it were running, with nothing in the tool result to say otherwise. Root cause is not established; the README says so rather than leaving it for someone to discover.

- **`apply_patch`'s forbidden-path list is now documented as the footgun guard it is, rather than reading like a security boundary.** It refuses patches touching `.git/`, `target/`, `node_modules/`, `__pycache__/` and `.env`, while `write_file` and `edit_file` have no equivalent — which invites the reading that one of them protects you. Neither does, deliberately: Forge has no sandbox and says so, the agent goes where the operating system lets the user go, and a partial block on the direct write tools would advertise a protection that does not exist. The comment now says that, and says the match is a plain prefix test that `./.git/config` walks straight past.

- **`web_search` claimed to be Chrome 120 on macOS.** Two places sent a spoofed browser User-Agent — the reqwest client and a `curl` fallback — which is how a scraper avoids being turned away, and also a lie told to someone else's server. It is the same objection this project raises when it declines to identify as a client it is not, so it now sends `forge-agent/<version>` and takes the answer it gets. There is nothing to lose by it: the tool ships disabled and does not work in practice regardless. A test scans the file for the browser tokens, with its own needles assembled from pieces so the scan cannot match itself.

- **`offline_mode` reached less than its own documentation said.** It claimed to force off "every network touchpoint that isn't the model API call itself", naming ChatGPT Codex's weekly version self-check among them. It could not: the poll lives in `auth.rs`, which cannot see `AppConfig`, so it was gated only on `FORGE_NO_AUTO_VERSION_CHECK`. Ticking offline mode on a Codex endpoint still reached `api.github.com` — and the people most likely to tick it are the ones the setting is advertised to, on restricted egress in "airgapped environments, secure facilities". The setting is now mirrored into `auth` at startup and again whenever it is toggled mid-session, so the poll honours it. What offline mode does *not* stop, and now says so instead of implying otherwise: refreshing the Codex OAuth token when Codex is the active endpoint, because the model call it authenticates cannot happen without it.

- **The features list still advertised web search**, which ships disabled because it does not work. Documented offline setup also still walked through four manual steps when `offline_mode = true` does all of them, and still listed Bun as a requirement — retired in this same release, and `install.sh` says so in as many words. `FORGE_BUN_SHA256` went with it: it was the second env var documented in a table that no code has ever read.

- **The `x-forge-session` header is now documented.** Requests carry a per-session id so a provider's logs can group one conversation together. It is the local session id — a timestamp plus three hex characters — it goes only to the endpoint you configured, and nothing comes back here. Not telemetry, but it was undocumented, and an undocumented header is indistinguishable from one at a glance.

- **Under `--dangerously-allow-all`, the agent was told it could install git on a remote machine unattended.** Remote revert runs on git, so the agent checks for it before touching files on a remote host — that part was right. The install policy was not: with the flag set, the instruction read "you may install git when needed using the appropriate non-interactive package-manager command." That flag waives *the user's own approval prompts*. It cannot waive the policy of a host that may belong to their employer or their client, and installing a package is a change to the machine rather than an edit inside a workspace. The agent now asks for explicit permission before installing git or running any package manager, in every mode. Declining does not leave anyone without a safety net: Forge snapshots every file its own tools write, independently of git, so the agent is told to say plainly that remote revert is unavailable for that path and then to prefer `write_file`/`edit_file`/`apply_patch` over shell commands that modify files — what the tools touch stays revertible, what a shell command changes cannot be recovered. The same carve-out was in the main system prompt too, and is gone from both.

- **Root cause of overnight "still running" chats:** subagents (and any other direct `ToolExecutor::execute("shell_exec")` caller) used a fallback `run_command` that blocked on `.output().await` with **no timeout at all**. A hung pipeline such as `rg … | head` inside an explore `delegate_task` therefore never returned a `tool_result`, so the parent turn stayed `Running` indefinitely — conversation log frozen after `tool_approved`, UI still alive, zero model progress. Compounding that: Unix shells are spawned with `setsid()` (PTY path, and now the piped path too), which makes the direct child a process-group leader, but every kill site only called `Child::kill()` on that one PID — pipeline grandchildren (`rg`, nested `sh`) survived. And when the interactive-prompt heuristic set `input_waiting = true`, the top-level wall-clock timeout was skipped entirely, so even streaming shells could lose their only escape hatch.
- **Fixes (agent/server side — `forge-agent`):**
  1. **Subagent/direct `shell_exec` now has a hard timeout** (default 300s, same as top-level `wait=true`; overridable via `timeout_secs`). On expiry the whole process group is killed and a `TIMEOUT:` result is returned instead of hanging.
  2. **`terminate_child` process-group kill** — all timeout/cancel/bg-kill paths now `kill(-pgid, SIGKILL)` before `Child::kill`, so `rg | head` grandchildren die with the outer `sh`. Piped and PTY spawns also set `kill_on_drop(true)` and (on Unix) `setsid()` so the group is well-defined.
  3. **`delegate_task` wall-clock ceiling** — new `agent.subagents.max_delegate_secs` (default **1800**). Phase 3's `JoinSet` wait aborts unfinished runners when the deadline hits and returns a timeout summary to the parent instead of blocking forever.
  4. **`input_waiting` no longer disables timeout forever** — a hard cap of `max(timeout_secs × 3, 600s)` still kills/escapes even while the prompt heuristic thinks the command is interactive.
- This is deliberately an agent-server patch: TUI/IDE clients only observe the existing tool-result / subagent-finished events; no wire-protocol change required.

### Added (`agent.min_shell_timeout_secs` — a floor under how aggressively the model can time out its own shell commands)

- **`shell_exec`'s `timeout_secs` (with `wait=true`) is entirely the model's own per-call choice, and it can simply guess wrong for a task that runs longer than expected — a build + long-running suite got killed at the 600s the model picked for it, well before it actually finished.** New `agent.min_shell_timeout_secs` config value (default `0`, i.e. no floor — today's behavior unchanged) raises the model's own requested timeout up to at least this many seconds when set higher, never lowers it, and only applies to `wait=true` (a detached/auto-backgrounded command was never killed by this value anyway). Verified directly: with the floor set above a deliberately-too-short model-requested `timeout_secs`, the command now runs to completion instead of being killed.

### Added (a project with no git repo now gets one automatically, before it needs it)

- **A brand-new project directory with no git repo never got one — nothing in forge or its clients ever initialized one — meaning rewind checkpoints for that project had no real git backing at all: nothing meaningful to actually restore file state to.** forge now auto-initializes a git repo at the project root at the start of any turn, if one doesn't already exist (a cheap no-op check once it does), with a plain notice in the conversation the first time it happens. Checked and initialized *before* the turn runs — not after, from inside checkpoint creation, where it was tried initially — so the notice arrives correctly ordered as the first thing that turn says, not something trailing in after that turn's own `Done` event already fired. Verified directly: a fresh directory with no `.git` gets one on the very first message, confirmed via `git rev-parse --is-inside-work-tree`, with the notice showing up in the right place in the event stream.

### Added (the reference TUI now has the xAI priority tier and provider-busy handling too)

- **The two features below were added to forge-agent's core protocol but only ever reached Forge IDE — the terminal UI (`ui/`) had no idea either existed.** Reconciled: `EndpointInfo.xai_priority_tier` and `update_xai_priority_tier` added to `ui/src/protocol.ts`; the `/thinking` menu for any xAI endpoint now shows a third "Priority tier" row (cycling it calls `update_xai_priority_tier`, same as the other reasoning toggles); a `Provider at capacity:`-tagged error for an xAI endpoint not already on priority now shows a dedicated dialog (`ProviderBusyDialog`) offering "Switch to priority tier (2x cost)" or "Dismiss", instead of just plain error text. Verified end-to-end in a live TUI session (a stub agent process standing in for forge-agent to reproduce the capacity error on demand): the menu row appears only for xAI endpoints, toggling it persists via the real `update_xai_priority_tier` message and reverts cleanly, and the busy dialog appears and resolves correctly both ways.

### Added (opt-in xAI priority processing tier)

- **`ModelEndpoint.xai_priority_tier` (default off) — when set, every request to that endpoint adds `service_tier: "priority"`, which xAI bills at 2x its standard per-token rate in exchange for higher scheduling priority during high demand.** Off by default and per-endpoint, since not every xAI key has priority access and it's a real, provider-billed cost increase, not a Forge one. New `update_xai_priority_tier` incoming message (mirrors `update_endpoint_reasoning`'s shape) to flip it at runtime; `EndpointInfo.xai_priority_tier` reports current state so a client can reflect it. Also fixed non-streaming OpenAI-compatible errors (`chat_openai`) to include the response body, not just the bare status code — the streaming path already did this, but a provider's actual rejection reason (e.g. "at capacity") was being silently dropped on this path.

### Added (rejections from an overloaded provider are now distinguishable from a generic API error)

- **A provider rejecting a request because it's at capacity (xAI's `resource-exhausted`/429 "at capacity", or an equivalent from another OpenAI-compatible provider) looked identical to any other API failure** — same generic "API error: ..." text, no way for a client to tell "this specific request was malformed" apart from "there was simply no room to serve it right now, try later or pay for priority." Now tagged with a distinct, stable `Provider at capacity: ...` prefix when detected, so a client can offer a relevant action (e.g. switching that endpoint to the priority tier above) instead of just showing a red error.

### Added (`ToolRequest` now says whether it actually needs approval)

- **`needs_approval: bool` added to `ToolRequest`, computed from the session's real trust settings** (`--dangerously-allow-all`, auto-mode, `auto_approve_writes`/`_reads`) at the exact same point the agent itself decides whether to block — not a guess. Previously a client had no way to know this and had to infer it from `kind` alone (typically "read = auto-approved, everything else = pending"), which had no visibility into those settings at all: a write/execute call under `--dangerously-allow-all` rendered as a permanently "awaiting approval" card that nothing was ever going to answer, even though the agent was never actually blocked on it — indistinguishable, from the user's side, from the agent genuinely being stuck. A client should now trust this field directly instead of re-deriving it.

### Fixed (`shell_exec` could falsely report "waiting for input" on ordinary command output)

- **The interactive-prompt heuristic (`looks_like_prompt`) fired immediately on any single PTY read chunk ending in a colon** — but a PTY read's chunk boundary can land anywhere, including right after a grep/ripgrep match or a compiler diagnostic's own "file:line:" (`src/main.rs:454:`), or mid-line after ordinary code content that happens to end in a colon (`ui.label(egui:`). Neither is an actual prompt, but the command just kept running normally regardless — which also meant `input_waiting` got stuck true (silently disabling the timeout/auto-background check) for the rest of that command's run, since the only other place it reset was a "user provided input" action no client sends yet. Fixed two ways: (1) `looks_like_prompt` now excludes the shape where the colon is preceded by a line number, and (2) more fundamentally, a prompt-shaped chunk is no longer confirmed immediately — it arms a candidate that's only actually reported via `ProcessInputNeeded` if 350ms pass with no further output, and cleared (self-healing `input_waiting` too) the moment more output arrives. A genuine interactive prompt is followed by silence; ordinary tool output isn't. Verified with a new permanent regression test (`prompt_heuristic_ignores_file_line_colons`) plus the existing suite (38 passing).

### Added (`SubagentStarted` now says which subagent nested it, if any)

- **`parent_id: Option<String>` added to `SubagentStarted`** — `Some(parent_slot_id)` when a subagent spawned this one via its own `delegate_task` call (nesting), `None` for a top-level one spawned by the main agent. The nested subagent's own id already encoded this implicitly (`"parent_slot_id:tool_call_id"`), but a client had no explicit, robust way to use that without parsing the id string. Omitted from the JSON when absent, so existing clients are unaffected.

### Fixed (a subagent's tool result was truncated to 200 characters before a client ever saw it)

- **`agent/subagent.rs` capped the `result` field of a subagent's `ToolResult` event at 200 characters** — a leftover from before that event was rendered as detailed content in a client; the top-level agent's equivalent event has never truncated it. A client showing the full result (a big read, a long search, a diff) would silently see it cut off no matter what it did on its own end, since the data was already gone by the time it arrived. Fixed by sending the full result, matching the top level. The short 200-char version is still used internally for the subagent's own final-summary bookkeeping, where a short version is actually correct.

### Fixed (a subagent's read-only tool calls never produced a `ToolRequest`/`ToolResult` at all)

- **A subagent doing pure read-only work (`read_file`, `search_code`, `list_directory`, `glob_files`) sent no `ToolRequest`/`ToolResult` events for any of it** — only Write/Execute/Unknown-kind calls did. A client following a subagent's activity through those events (rather than the coarser `SubagentStatus` line) would see it apparently do nothing at all, even while it was actively reading through the codebase. The top-level agent has never had this gap — `core.rs`'s own tool-call handling always sends both events regardless of kind. Fixed by sending them unconditionally from `agent/subagent.rs` too, same as the top level; approval is still only requested for Write/Execute/Unknown; reads are still auto-approved. Verified with the existing test suite (37 passing).

### Added (`ToolRequest`/`ToolResult` now say which subagent they belong to)

- **`subagent_id: Option<String>` added to the `ToolRequest`/`ToolResult` wire messages** — `Some(slot_id)` when the call came from a running subagent, `None` for the top-level agent's own calls. Previously a subagent's tool activity arrived over the same event stream as everything else with no way to tell it apart from the top-level agent's, or from a *different* concurrently-running subagent's — a client had no way to give a subagent its own dedicated view. Omitted from the JSON entirely when absent (`skip_serializing_if`), so existing clients that don't know about the field are unaffected.

### Fixed (a tool call's approval prompt could be answered by a click meant for a different one)

- **With two or more `delegate_task` subagents running concurrently (the normal way to parallelize multi-file work), approving or denying one subagent's pending write/execute could silently wave through a *different* subagent's next approval-gated call** — no real wait, no genuine decision for it, even though its own prompt had correctly shown up. Root cause: the parent has no cheap way to know which of several concurrently-running subagents a click was meant for, so it forwards every `approve`/`deny` to *all* of them (`agent/core.rs`'s Phase 3 select loop); each subagent's own wait then treated *any* approval message as approval for whatever it happened to be blocked on (`agent/subagent.rs`), regardless of which tool call actually requested it. `ApproveAction` already carried the real `tool_id` on the wire, but `DenyAction` didn't (only a `reason`), and nothing on either side actually checked the id against the pending call — a residue of the earlier "subagents could get stuck indefinitely" fix, which explicitly called this out as a known, not-yet-done gap. Fixed by adding `tool_id` to `DenyAction`'s wire shape and having every approval-wait site (top-level tool calls, `enter_plan_mode`, sequential and concurrent subagent dispatch) check the incoming id against the specific call it's blocked on, looping past anything addressed to someone else instead of treating it as an answer. Verified with the existing test suite (37 passing) plus manual review of all four wait sites; a client (Forge IDE included) must now send the `tool_id` it's actually responding to on both approve and deny.

### Added (xAI/Grok model auto-discovery)

- **Any `api.x.ai` endpoint configured in `config.toml` now gets its full model catalog auto-discovered on launch**, the same way ChatGPT Codex already does — instead of only ever showing the one model you'd manually typed in. Grouped by `(base_url, api_key)` (there's no OAuth/login concept for xAI, just whatever key a configured endpoint already has), queries the account's live `/v1/models`, adds any new models found (name auto-generated from the model id), updates context length on existing entries, and prunes ones no longer offered — only when the live query genuinely succeeded and returned at least one model, so a network hiccup can never wipe the list. Image/video-generation models (`grok-imagine-*`) are filtered out since they're not chat-completions models a text/tool-calling agent can use. Verified live against a real xAI account: discovered 6 chat models automatically, confirmed a synthetic stale model gets pruned, and confirmed `models.default` is untouched by any of this.
- **Generated display names now strip bare date-stamp segments** instead of showing them as a meaningless number — `grok-4.20-0309-reasoning` is now "Grok 4.20 Reasoning" rather than "Grok 4.20 0309 Reasoning". The rule (`looks_like_date_code`) is a catch-all, not a hardcoded xAI special case: a model-id segment that's a *bare* run of 4/6/8 digits (MMDD/YYMMDD/YYYYMMDD, however a provider chose to stamp it) is treated as a date and dropped, while dotted version numbers (`4.20`, `0.1`) are always left alone since a real version segment in these IDs is never dot-free. Existing config entries created by a prior run keep whatever name they already have (matching how Codex discovery already avoids clobbering a name you might have customized) — the 3 already-misnamed entries on the account this was found on were regenerated by hand, and any other affected setup needs the same one-time fix (delete the entry, let the next launch recreate it).

### Fixed (generic OpenAI-compatible endpoints never sent their API key)

- **Any endpoint with `endpoint_type = "open_ai"` and an `api_key` set — real OpenAI, OpenRouter, xAI/Grok, or any other authenticated cloud provider using the OpenAI wire format — silently never sent that key on any request.** `ApiClient::from_endpoint` built `Backend::OpenAi` from only `base_url`, dropping `api_key` entirely; every request (chat, streaming chat, `/models` auto-discovery, context-length probing) went out with no `Authorization` header at all. This was invisible for local/self-hosted servers (LM Studio, llama.cpp, Oxide) that don't require auth in the first place — which is presumably why it went unnoticed — but made it impossible to actually use any authenticated `open_ai`-type endpoint. Fixed by carrying `api_key` through `Backend::OpenAi` and adding it as a bearer token on every request that type makes.
- **Switching models mid-session (`switch_model`) also dropped the API key, independently of the bug above.** The client only ever receives `EndpointInfo` (deliberately excludes `api_key` — that field never leaves the server), but the `SwitchModel` handler was building a fresh `ModelEndpoint` straight from the incoming message's fields, hardcoding `api_key: None` instead of looking the real key up from the server's own config by name. Fixed to look it up from `app_config.models.endpoints` by name instead of trusting (and hardcoding around) the client-supplied message. Verified both fixes together against a local mock HTTP server requiring a bearer token: confirmed a 401 before either fix, still 401 after only the first fix (switching models re-lost the key), and a correctly-authenticated request after both.

### Fixed

- **Subagents could get stuck indefinitely, with no way to recover.** Three compounding bugs in `delegate_task`/subagent execution:
  - A subagent's own tool calls always required a fresh approval for write/execute tools, ignoring `--dangerously-allow-all`, auto-mode, and `auto_approve_writes` — the same trust settings the top-level agent already respects. Since every subagent (including read-only types like "explore") automatically gets `delegate_task` added to its own toolset, a subagent nesting another subagent would always block on an approval the session's trust settings should have skipped.
  - Cancelling a run while a subagent was active didn't actually stop it — it recorded a synthetic "cancelled" result but never aborted the real task, then unconditionally waited for that task to finish anyway. A genuinely stuck subagent (per the bug above) made Cancel hang too, with no recovery short of killing the process.
  - A nested subagent (a subagent calling `delegate_task` itself) reused its parent's id for its `subagent_started`/`subagent_finished` events. A client tracking subagents by id would see the *outer* subagent marked finished the moment the nested call completed, even though the outer subagent kept running afterward — surfacing as "subagent shows started, then nothing else happens."

  Subagents now respect the same trust settings as the top level, Cancel actually aborts stuck subagent tasks (`JoinSet::abort_all`), and nested subagents get their own unique id so they show up as independent, correctly-tracked entries. Verified live against all three scenarios (trusted-mode nesting, cancelling a genuinely stuck nested approval, and unique id assignment).

  **Known follow-up — since resolved, in this same release; see the concurrent-subagent approval entry above.** As written at the time: `ApproveAction`/`DenyAction` still don't carry a real per-request identifier server-side (the wire `tool_id` is accepted but unused — any approval unblocks whatever's currently pending). This works correctly as long as only one thing is pending approval at a time, which is the common case, but two simultaneous pending approvals (e.g. two concurrent subagents both awaiting approval) aren't distinguished. Fixing this properly means a `ToolRequest`/`ApproveAction` wire-shape change affecting every client (Forge's own UI included), so it's deliberately out of scope here.

## [0.2.1] — 2026-07-01

### Fixed

- **Break the edit_file "old_string not found" death-spiral.** When an `edit_file` target string doesn't match, Forge now returns bounded recovery hints instead of a bare error: it flags whitespace-only differences, shows the single closest-matching region with line numbers (capped — never dumps the file), and on multiple matches lists the occurrence lines to disambiguate. This stops weaker local models from looping after a failed edit.
- **Token usage and auto-compaction restored for OpenAI-compatible streaming.** Forge now sends `stream_options: { include_usage: true }`, so spec-compliant servers (mlx_lm, vLLM, llama.cpp, LM Studio, OpenAI…) report token counts while streaming. Without it those servers sent no usage, leaving `/usage` and the context footer stuck at 0 and silently disabling auto-compaction — on a long local session context would grow unbounded until the model's real window overflowed.
- **Recover tool calls that misbehaving servers leak as raw text.** Some OpenAI-compatible servers (notably mlx_lm at high context) fail to parse a model's `<tool_call>` block into structured `tool_calls`, instead leaking the raw markup into the content/reasoning stream and ending the turn with no tool to run — which made the agent appear to stall, loop, or return an empty turn. Forge now recovers a complete leaked `<tool_call>` block as a real tool call (handling both the JSON/Hermes form and Qwen3-Coder's `<function=…><parameter=…>` XML dialect), gated so a genuine text answer or a properly-structured call is never affected.

### Changed

- Installer/launcher hardening: the `forge` wrapper now locates `bun` robustly (`~/.bun/bin/bun` or `PATH`, with a clear error if absent), and `install.sh` checks for `curl` and `ripgrep` up front (the web tools and `search_code` need them).

## [0.2.0] — 2026-06-25

### Removed

- **Claude subscription (Pro/Max) OAuth login — removed entirely (breaking).** The Claude OAuth flow (`forge --login` / `--login-claude` / the in-TUI `/login --anthropic`), the embedded Claude Code OAuth client id, the `claude-cli` user-agent and `claude-code` beta-header impersonation, the Claude token store (`~/.config/forge/auth.json`), and the weekly Claude `client_version` self-check are all gone. Forge no longer contains any code path that authenticates to Anthropic with subscription credentials.

  **Why:** Anthropic's Consumer Terms and the Claude Code legal terms restrict subscription OAuth tokens to Anthropic's own applications and prohibit routing requests through Free, Pro, or Max plan credentials in any other product, tool, or service. Forge had been authenticating to Anthropic with the Claude Code OAuth client and a `claude-cli` user-agent — i.e. using subscription credentials outside a native Anthropic app. We were not aware of this restriction until recently; this release removes the behavior outright to respect Anthropic's terms. The risk it avoided lands on the end user's Claude account (which can be flagged or suspended without notice), so removal is the right call. We will not reintroduce Anthropic subscription sign-in unless and until Anthropic permits it.

  **Anthropic is still fully supported via an API key** — set `endpoint_type = "anthropic"` with `api_key = "sk-ant-…"` in `~/.config/forge/config.toml` (or pick **Claude** in the installer wizard). **ChatGPT Codex** subscription login is unchanged and remains the only supported subscription sign-in.

### Added

- **Streaming reasoning display.** Reasoning ("thinking") models now show their chain-of-thought live as a compact `✻ Thinking… (elapsed · ~tokens)` line that settles into a persistent `✻ Thought for Xs` when the answer arrives — press **Ctrl+T** to expand or collapse it. This works with any OpenAI-compatible endpoint — local servers like LM Studio, Ollama, vLLM, or mlx_lm, as well as OpenAI-compatible APIs — that streams reasoning in a separate field (`reasoning_content`, `reasoning`, or `thinking`). Models or servers that don't send a separate reasoning field are unaffected; their output renders as normal.

### Changed

- The installer's **Claude** option now configures an Anthropic API-key endpoint instead of subscription OAuth, matching the auth change above.

### Fixed

- **Compatibility with strict OpenAI-compatible servers.** Forge injects some system-role messages mid-conversation (continuation nudges, plan-mode notes, etc.). Servers that require the system message to come first — notably `mlx_lm` — rejected those turns with `System message must be at the beginning`. Forge now keeps the leading system prompt and relocates later ones, so these servers work.
- Ctrl-key shortcuts (e.g. **Ctrl+F** copy mode, **Ctrl+T** expand reasoning) no longer leak their letter into the message input.

## [0.1.0] — 2026-06-19

Initial public release.

### Added

- Headless Rust agent (`forge-agent`) speaking a JSON-newline protocol on stdin/stdout
- Bun/Ink terminal UI (`forge`) that drives the agent
- Twelve built-in tools: read/write/edit files, apply unified diffs, list directory, search code, glob files, todo write, shell exec, web search, web fetch, delegate task
- Built-in agent definitions: bash, explore, general, plan
- Custom shell-backed tools loaded from `~/.config/forge/tools/` and `.agent/tools/`
- Custom Markdown agent definitions loaded from `~/.config/forge/agents/` and `.agent/agents/`
- Endpoint backends: OpenAI-compatible, Anthropic `/v1/messages`, ChatGPT Codex Responses API
- OAuth login for Claude (`forge --login`) and ChatGPT Codex (`forge --login-chatgpt`) subscriptions
- Direct API key support for Anthropic, OpenAI, OpenRouter, and custom OpenAI-compatible endpoints
- Paste-the-code OAuth fallback for environments where the localhost callback can't land (remote SSH without port forwarding, firewall restrictions, etc.)
- Live ChatGPT Codex model catalog discovery — no dependency on the official `codex` CLI
- Plan mode with explicit approval before edits
- Session persistence and `--resume-session`
- Git-backed per-turn snapshots and `/revert`
- LLM-backed context compaction and rolling-window context strategies
- Approval-based command gating with `--dangerously-allow-all` for trusted sessions
- Native installers for macOS, Linux, and Windows
- One-command bootstrap installers (`bootstrap.sh` / `bootstrap.ps1`)
- Five-way setup wizard: local LLM / Claude subscription / ChatGPT Codex subscription / direct API key / skip
- Cross-platform browser launching for OAuth flows (`open` on macOS, `xdg-open` on Linux/BSD, `cmd /c start` on Windows)

[Unreleased]: https://github.com/Vulkgryph/Forge/compare/v0.4.2...HEAD
[0.4.2]: https://github.com/Vulkgryph/Forge/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/Vulkgryph/Forge/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/Vulkgryph/Forge/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/Vulkgryph/Forge/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Vulkgryph/Forge/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/Vulkgryph/Forge/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Vulkgryph/Forge/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Vulkgryph/Forge/releases/tag/v0.1.0
