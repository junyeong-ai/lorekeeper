//! Normalize source-specific rich text into standard Markdown so downstream LLM steps and
//! the Obsidian vault receive clean, AI-friendly input instead of ADF JSON, raw HTML, or
//! Slack's `<…>` token soup. Conversions are loss-averse: any construct without a Markdown
//! equivalent degrades to its text content rather than being dropped.

use std::collections::HashMap;

use lk_core::text::collapse_blank_lines;
use serde_json::Value;

/// Convert an HTML fragment to Markdown via `htmd`. `htmd::convert` reads from an
/// in-memory string, whose only fallible step is a `Read` that cannot fail, and HTML5
/// parsing always recovers from malformed markup — so conversion is infallible here; the
/// `Result` collapses to an empty string for the unreachable error case.
pub fn html_to_markdown(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    // Dash bullets to match the ADF converter's list style (one consistent Markdown
    // dialect across all sources).
    let converter = htmd::HtmlToMarkdown::builder()
        .options(htmd::options::Options {
            bullet_list_marker: htmd::options::BulletListMarker::Dash,
            ..Default::default()
        })
        .add_handler(vec!["img"], img_without_data_uris)
        .add_handler(MACHINE_STATE_ELEMENTS.to_vec(), drop_element)
        .add_handler(vec!["ac:task-status"], task_checkbox)
        .add_handler(
            ATTRIBUTE_BORNE_TEXT.iter().map(|(tag, _)| *tag).collect(),
            attribute_borne_text,
        )
        .add_handler(
            vec!["ac:link-body", "ac:plain-text-link-body"],
            spaced_link_body,
        )
        .add_handler(vec!["ac:link"], trimmed_link)
        .add_handler(vec!["ac:adf-extension"], one_adf_representation)
        .build();
    converter
        .convert(&normalize_storage_format(html))
        .map(|md| md.trim().to_string())
        .unwrap_or_default()
}

/// Elements whose TEXT is machine state rather than prose, so the loss-averse rule above —
/// degrade an unmapped construct to its text — is wrong for them. Most come from Confluence,
/// but the rule is about what the text IS, not where it came from, so a `<style>` block out of
/// an email or a feed belongs here on the same grounds.
///
/// Confluence storage format is XHTML. A macro's `<ac:parameter>` children hold its
/// settings (a status macro's colour, a roadmap's base64 state blob) and a task's
/// `<ac:task-id>`/`<ac:task-uuid>` hold its identity. Degraded, they are emitted INLINE and
/// unseparated: a page reads `검증JTdCJTIybmFtZSUyMi…` or `170e6f1a-9cincompleteShip the
/// thing`. One real page came out 30% encoded settings.
///
/// A task's `ac:task-status` is NOT in that company — a reader sees it, as a ticked or
/// unticked box, and whether the thing was done is most of what a checklist says. It is the
/// text `complete`/`incomplete` that is machine state, so it is translated rather than
/// dropped ([`task_checkbox`]).
///
/// This is a deliberate trade, not a free win. A macro WITH a body loses nothing — the body
/// is `ac:rich-text-body`/`ac:plain-text-body` and is untouched. A macro WITHOUT one loses
/// its visible value: a `status` lozenge's label and a `jira` macro's issue key are
/// parameters, so `State: APPROVED` becomes `State:`. Keeping those would take a whitelist
/// of parameter names per macro type — real machinery, permanently incomplete — and a
/// kilobyte of base64 mid-sentence is the worse of the two costs.
const MACHINE_STATE_ELEMENTS: &[&str] = &[
    "ac:parameter",
    "ac:task-id",
    "ac:task-uuid",
    // Cloud smart-links carry their settings the same way, welding a URL onto the fallback
    // text they sit beside (`https://x.example/1fallback text`). Which of the extension's
    // representations is the human-readable one is `one_adf_representation`'s question, not
    // this list's — for a bodyless card it is the fallback, for a panel either would do.
    "ac:adf-attribute",
    // A stylesheet and a script are the same thing arriving from ordinary HTML rather than
    // from Confluence. Degraded to text they land in the page as themselves: one real vault
    // page carries `.abbel-fig { display: block; text-align: center; … }` mid-article,
    // lifted verbatim out of an RSS feed's `<style>` block.
    "style",
    "script",
];

/// A task's state as the reader saw it: a ticked or unticked box, which is most of what a
/// checklist says. The storage words `complete`/`incomplete` are the machine's spelling of it,
/// so anything else — a status this converter has no translation for — degrades to nothing
/// rather than leaking that vocabulary into the prose.
fn task_checkbox(
    handlers: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    let state = handlers.walk_children(element.node).content;
    Some(match state.trim() {
        "complete" => "[x] ".to_string().into(),
        "incomplete" => "[ ] ".to_string().into(),
        _ => String::new().into(),
    })
}

fn drop_element(
    _: &dyn htmd::element_handler::Handlers,
    _: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    Some(String::new().into())
}

/// The mirror of the rule above: these carry what the reader saw in an ATTRIBUTE, so
/// degrading them to their (empty) text drops the thing entirely — `See <ac:link><ri:page
/// ri:content-title="Design Notes"/></ac:link>` becomes a dangling `See`. Every
/// Confluence→Confluence cross-reference is lost that way, which is precisely the material a
/// knowledge vault wants, and an external reference, an inline date and an emoji go the same
/// way for the same reason.
///
/// Only attributes that ARE what the reader saw belong here. `ri:user` carries an opaque
/// account id, so a mention stays dropped rather than becoming `557058:abc` — emitting that
/// would commit the machine-state defect from the opposite direction.
const ATTRIBUTE_BORNE_TEXT: &[(&str, &str)] = &[
    ("ri:page", "ri:content-title"),
    ("ri:attachment", "ri:filename"),
    ("ri:url", "ri:value"),
    ("ac:emoticon", "ac:emoji-fallback"),
    ("time", "datetime"),
];

/// Render an element from [`ATTRIBUTE_BORNE_TEXT`], preferring the text it actually has.
///
/// Only `time` can carry both — `<time datetime="2020-12-25">Christmas</time>` — and there
/// the word is what the page reads. The rest are empty elements, for which this degenerates
/// to the attribute, so one rule covers both without a special case.
fn attribute_borne_text(
    handlers: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    let text = handlers.walk_children(element.node).content;
    if !text.trim().is_empty() {
        return Some(text.into());
    }
    // The parser keeps the `ri:`/`ac:` prefix in the attribute's local name, since storage
    // format declares no namespace an HTML parser would resolve.
    let carrier = ATTRIBUTE_BORNE_TEXT
        .iter()
        .find(|(tag, _)| *tag == element.tag)
        .map(|(_, attr)| *attr);
    let value = carrier
        .and_then(|attr| element.attrs.iter().find(|a| &*a.name.local == attr))
        .map(|a| a.value.to_string())
        .unwrap_or_default();
    Some(value.into())
}

/// A link may carry BOTH halves of a reference: the label of the page it points at and the
/// display text its author typed — `<ac:link><ri:page ri:content-title="Design Notes"/>
/// <ac:plain-text-link-body><![CDATA[the notes]]></ac:plain-text-link-body></ac:link>`.
/// Rendered as plain siblings they weld into `Design Notesthe notes`, a word that is on
/// neither the page nor in any vocabulary a reader or an extractor would recognise — the same
/// defect the machine-state rule exists to prevent, arriving from the opposite direction. Both
/// halves are text the reader saw, so both are kept, separated.
///
/// The space is emitted by the BODY and trimmed off again by the LINK, which is what confines
/// it to the gap BETWEEN the two: a link with no resource label — an anchor link, the form
/// where the body stands alone — renders its body and trims the leading space away, so it
/// never gains a stray one before the sentence's next character.
fn spaced_link_body(
    handlers: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    let body = handlers.walk_children(element.node).content;
    let body = body.trim();
    Some(if body.is_empty() {
        String::new().into()
    } else {
        format!(" {body}").into()
    })
}

/// An ADF extension's children are ALTERNATIVE renderings of one thing, not a sequence: the
/// ADF node itself, and a fallback authored for consumers that do not read ADF. Rendering them
/// as siblings printed a Cloud panel's prose twice, once from each — and Cloud emits both for
/// every extension that has a body.
///
/// So exactly one is emitted: the NODE's own rendering, and the fallback only where the node
/// renders to nothing. A fallback is a stand-in, so it may never displace what it stands in for
/// — a Cloud extension whose fallback reads `This macro is not available.` must still say what
/// its body says. An `inlineCard` is the case from the other side: its node is nothing but
/// attributes, so it renders empty and the fallback carries the only link there is.
///
/// Asking WHICH CHILD rather than which position is the point. Position is a premise — that the
/// fallback comes second — and a premise that fails does so silently, inverting the rule into
/// exactly the stand-in-wins outcome it was chosen to prevent. Naming the children has no
/// premise to fail.
fn one_adf_representation(
    handlers: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    let children = element.node.children.borrow();
    // `handle`, not `walk_children`: a child's OWN rendering, so one that is not an element
    // wrapper — bare text — is not silently skipped over.
    let render = |rendered: &std::rc::Rc<markup5ever_rcdom::Node>| {
        handlers
            .handle(rendered)
            .map(|result| result.content)
            .filter(|content| !content.trim().is_empty())
    };
    let named = |name: &'static str| {
        children
            .iter()
            .filter(|child| element_name(child) == Some(name))
            .find_map(render)
    };
    let chosen = named("ac:adf-node")
        .or_else(|| named("ac:adf-fallback"))
        // An extension holding neither still says whatever it holds.
        .or_else(|| children.iter().find_map(render))
        .unwrap_or_default();
    Some(chosen.into())
}

/// The tag name of `node`, or `None` when it is not an element.
fn element_name(node: &std::rc::Rc<markup5ever_rcdom::Node>) -> Option<&str> {
    match &node.data {
        markup5ever_rcdom::NodeData::Element { name, .. } => Some(&name.local),
        _ => None,
    }
}

/// See [`spaced_link_body`] — this is the half that keeps the separator internal.
fn trimmed_link(
    handlers: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    Some(handlers.walk_children(element.node).content.trim().into())
}

/// Rewrite the parts of Confluence storage format that an HTML parser reads wrongly, so the
/// shared converter sees ordinary HTML. Three rewrites, all literal-token exact rather than
/// guesses, and an input carrying none is returned untouched.
///
/// **CDATA.** Storage format is XHTML and puts a code macro's body — and a link's display
/// text — in `<![CDATA[…]]>`. An HTML5 parser has no CDATA outside foreign content: it reads
/// the whole section as a bogus COMMENT and drops the contents, so every Confluence code
/// block arrived empty, silently, which on an engineering wiki is the most valuable text on
/// the page. CDATA cannot nest and ends at the first `]]>`.
///
/// **An empty element is empty, and a container is its HTML counterpart.** See
/// [`rewrite_tags`] — the rewrite that decides
/// whether the content after it survives at all. It runs AFTER the CDATA unwrap, which
/// escapes `<`/`>`, so tag-shaped text recovered from a code sample is never rescanned as
/// markup.
///
/// **A code body is a code block.** `<ac:plain-text-body>` is where a `code`/`noformat`
/// macro keeps its body; left as an unknown element it degrades to a paragraph, and the
/// converter then MARKDOWN-ESCAPES it — `[1, 2]` arrives as `\[1, 2\]`, backslashes
/// injected into JSON. Mapping it to `<pre><code>` is what makes the converter emit a fenced
/// block and stop escaping, which is the only form that reproduces the source text.
///
/// **A known gap, left open deliberately.** `unwrap_cdata` does not share [`rewrite_tags`]'s
/// comment and raw-text skipping, so inside a RAWTEXT element it escapes text a parser would
/// have read as text — a CDATA section that closes, and equally the bare opening token of one
/// that does not, which needs no escaping there because RAWTEXT has no bogus-comment state to
/// protect against. The exposure is the raw-text elements that are KEPT — `xmp`, `iframe`,
/// `noembed`, `noframes` and `noscript` — since `textarea`/`title` are RCDATA and decode the
/// entities back, and only `script`/`style` are
/// dropped outright. The cost is a literal `&lt;` in the text. Nothing is deleted,
/// nothing injected, no element left open, which is a different class from every other defect
/// this file records. Closing it means either a second scanner tracking the same spans — the
/// duplication that lets two scanners diverge, which is what put most of those defects here — or
/// merging the passes, which would trade a call ORDER anyone can check by reading two lines for
/// an invariant about a cursor that never rewinds. Neither is worth paying for a cosmetic gap
/// whose trigger no source is known to reach.
///
/// The ELEMENTS are reachable and it would be wrong to say otherwise — RSS full-text fetching
/// pulls arbitrary web pages, where `<iframe>` and `<noscript>` are both ordinary. What has
/// never been observed is the CONJUNCTION the gap needs: one of the five whose content ALSO
/// spells CDATA syntax. An iframe's fallback is a sentence about browsers when it is anything at
/// all, and what makes `noscript` common is the lazy-loaded `<img>` inside it, which carries no
/// CDATA. The live vault holds none of either.
fn normalize_storage_format(html: &str) -> std::borrow::Cow<'_, str> {
    let unwrapped = unwrap_cdata(html);
    match rewrite_tags(&unwrapped) {
        std::borrow::Cow::Borrowed(_) => unwrapped,
        std::borrow::Cow::Owned(owned) => std::borrow::Cow::Owned(owned),
    }
}

/// Storage-format containers with an exact HTML counterpart, paired with the tags to write in
/// their place. Rewriting them here rather than handling them in the converter is what gets
/// the STRUCTURE: a list has to reach the converter as a list to come out as one.
///
/// `ac:plain-text-body` is where a `code`/`noformat` macro keeps its body. Left as an unknown
/// element it degrades to a paragraph, which the converter then MARKDOWN-ESCAPES — `[1, 2]`
/// arrives as `\[1, 2\]`, backslashes injected into JSON — so it becomes the one form that
/// both fences and stops escaping.
///
/// `ac:task-list`/`ac:task` are a CHECKLIST, the construct a working wiki puts its decisions
/// and follow-ups in. Unmapped, each task degraded to bare text and three of them ran together
/// into `Rotate the signing keyUpdate the runbookNotify the on-call rota` — the list, and with
/// it which items were done, gone.
const REWRITTEN_ELEMENTS: &[(&str, &str, &str)] = &[
    ("ac:plain-text-body", "<pre><code>", "</code></pre>"),
    ("ac:task-list", "<ul>", "</ul>"),
    ("ac:task", "<li>", "</li>"),
];

/// Give every XHTML empty element an explicit end tag, because HTML has no such syntax.
///
/// `<ac:parameter ac:name="icon"/>` is an EMPTY element in XHTML. An HTML parser has no
/// self-closing form for a non-void element: it reads that as an OPEN tag and hands it every
/// following sibling as a CHILD. The handlers above answer from the element alone — one drops
/// it, the other replaces it with an attribute — so those adopted siblings are discarded
/// along with it. An empty parameter ahead of a macro's body therefore deletes the body, and
/// a self-closed `<ac:task-id/>` deletes the whole task list. Confluence writes empty elements
/// this way as a matter of course, so this is the ordinary shape of the input rather than a
/// corner case, and the loss is silent and total.
///
/// Fixing it in the parse rather than in each handler is what makes it hold for constructs
/// nobody has enumerated yet: afterwards an element the source wrote as empty IS empty, so
/// ignoring a handled element's children is correct by construction rather than by luck.
///
/// Void elements keep their form: `<br></br>` is TWO line breaks to an HTML parser, so
/// expanding those would invent content instead of preserving it. Comments and raw-text
/// elements are skipped whole, since what is inside them is text and rewriting text is how a
/// pre-parse pass corrupts a document.
fn rewrite_tags(html: &str) -> std::borrow::Cow<'_, str> {
    let rewritten = |name: &str| {
        REWRITTEN_ELEMENTS
            .iter()
            .find(|(tag, _, _)| *tag == name)
            .map(|(_, open, close)| (*open, *close))
    };
    if !html.contains("/>") && !REWRITTEN_ELEMENTS.iter().any(|(t, _, _)| html.contains(t)) {
        return std::borrow::Cow::Borrowed(html);
    }
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0;
    let mut at = 0;
    while let Some(offset) = html[at..].find('<') {
        let lt = at + offset;
        if html[lt..].starts_with("<!--") {
            // A comment's contents are not markup, so a `/>` inside one is not a tag.
            at = skip_past(html, lt, "-->");
            continue;
        }
        if let Some(tag) = scan_end_tag(html, lt) {
            at = tag.end + 1;
            if let Some((_, close)) = rewritten(&html[tag.name.clone()]) {
                out.push_str(&html[cursor..lt]);
                out.push_str(close);
                cursor = tag.end + 1;
            }
            continue;
        }
        let Some(tag) = scan_start_tag(html, lt) else {
            at = lt + 1;
            continue;
        };
        at = tag.end + 1;
        let name = &html[tag.name.clone()];
        if is_raw_text_element(name) {
            at = skip_raw_text(html, at, name);
            continue;
        }
        if let Some((open, close)) = rewritten(name) {
            out.push_str(&html[cursor..lt]);
            out.push_str(open);
            // Written as empty, it still needs both halves, or the replacement opens an
            // element nothing closes.
            if tag.self_closing {
                out.push_str(close);
            }
            cursor = tag.end + 1;
            continue;
        }
        if !tag.self_closing || is_void_element(name) {
            continue;
        }
        out.push_str(&html[cursor..=tag.end]);
        out.push_str("</");
        out.push_str(name);
        out.push('>');
        cursor = tag.end + 1;
    }
    if cursor == 0 {
        return std::borrow::Cow::Borrowed(html);
    }
    out.push_str(&html[cursor..]);
    std::borrow::Cow::Owned(out)
}

/// Read the end tag opening at `lt`, or `None` when that is not what is there.
///
/// A rewritten element is matched by NAME on both halves. Testing for the literal
/// `<ac:plain-text-body>` while replacing every `</ac:plain-text-body>` is what let one
/// carrying an attribute go unmatched on open and matched on close, leaving an orphan
/// `</code></pre>` that closed an enclosing block early and spilled its tail into the page.
fn scan_end_tag(html: &str, lt: usize) -> Option<Tag> {
    let rest = html.get(lt + 1..)?.strip_prefix('/')?;
    if !rest.chars().next()?.is_ascii_alphabetic() {
        return None;
    }
    let name_end = rest
        .char_indices()
        .find(|(_, c)| c.is_ascii_whitespace() || *c == '>')
        .map(|(i, _)| i)?;
    let gt = name_end + rest[name_end..].find('>')?;
    Some(Tag {
        name: lt + 2..lt + 2 + name_end,
        end: lt + 2 + gt,
        self_closing: false,
    })
}

/// The index just past a LOWERCASE `needle`, or the end of the input when it never appears —
/// an unterminated comment runs to EOF, which is what a parser concludes too. Matching is
/// ASCII-case-insensitive, which leaves every byte index intact because ASCII case folding
/// cannot change a character's width.
fn skip_past(html: &str, from: usize, needle: &str) -> usize {
    debug_assert_eq!(needle, needle.to_ascii_lowercase());
    html[from..]
        .to_ascii_lowercase()
        .find(needle)
        .map_or(html.len(), |end| from + end + needle.len())
}

/// The index just past the name of the end tag that closes raw-text element `name`.
///
/// What ends a raw-text element is an end tag whose name MATCHES, and HTML decides that on the
/// character after the name — ASCII whitespace, `/` or `>`. A plain substring search does not:
/// a person typing `</textareas>` inside a text box ended the protected span at their own words,
/// so everything after them was rescanned as markup and rewritten. Text boxes are the case that
/// matters, since they hold prose someone typed and, like every raw-text element but `script` and
/// `style`, their content is kept rather than dropped.
///
/// ASCII whitespace exactly, not `char::is_whitespace`: HTML's terminator set is the five ASCII
/// ones, so a parser reads `</textarea\u{3000}>` as an unclosed span and a Unicode test reads it
/// as a closed one — the same early ending by a different route, and an ideographic space is
/// ordinary text in a CJK vault.
fn skip_raw_text(html: &str, from: usize, name: &str) -> usize {
    let close = format!("</{}", name.to_ascii_lowercase());
    let hay = html[from..].to_ascii_lowercase();
    let mut at = 0;
    while let Some(found) = hay[at..].find(&close) {
        let past_name = at + found + close.len();
        match hay[past_name..].chars().next() {
            Some(c) if c.is_ascii_whitespace() || c == '/' || c == '>' => return from + past_name,
            // A longer name (`</textareas>`) is somebody's text, not this element's end tag.
            Some(_) => at = past_name,
            None => break,
        }
    }
    html.len()
}

/// Elements whose content a parser reads as TEXT, not markup, so a `/>` inside one belongs to
/// a script, a stylesheet or a text box rather than to a tag. Rewriting there would inject
/// `</div>` into a JavaScript string — the pre-parse pass corrupting exactly the document it
/// was meant to leave alone.
fn is_raw_text_element(name: &str) -> bool {
    const RAW_TEXT: &[&str] = &[
        "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes",
        // Scripting is a parser's default, and with it on this is raw text like the rest.
        // It is ordinary in email and in fetched articles — a lazy-loaded image's fallback,
        // a tracking pixel — so leaving it off meant rewriting the document's own text there.
        "noscript",
    ];
    RAW_TEXT.iter().any(|raw| name.eq_ignore_ascii_case(raw))
}

/// Elements an HTML parser closes on its own, for which `<x/>` is already the whole element.
fn is_void_element(name: &str) -> bool {
    const VOID: &[&str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
        "source", "track", "wbr",
    ];
    VOID.iter().any(|void| name.eq_ignore_ascii_case(void))
}

struct Tag {
    name: std::ops::Range<usize>,
    /// Index of the `>` closing the tag.
    end: usize,
    self_closing: bool,
}

/// Read the start tag opening at `lt`, following the HTML tokenizer's tag states.
///
/// Those states are exactly what decides the question this is asked, so a `/` counts as
/// self-closing only where the tokenizer would see one: `<ri:url ri:value="https://x/>y"/>`
/// closes at the LAST `>` with the quoted `/>` left alone, and an unquoted `href=/a/` keeps
/// its slash as value text instead of turning the element empty.
fn scan_start_tag(html: &str, lt: usize) -> Option<Tag> {
    enum State {
        BeforeAttrName,
        AttrName,
        BeforeAttrValue,
        Quoted(char),
        Unquoted,
        SelfClosing,
    }
    let rest = &html[lt + 1..];
    if !rest.chars().next()?.is_ascii_alphabetic() {
        return None;
    }
    let name_end = rest
        .char_indices()
        .find(|(_, c)| c.is_ascii_whitespace() || *c == '/' || *c == '>')
        .map(|(i, _)| i)?;
    let name = lt + 1..lt + 1 + name_end;

    let mut state = State::BeforeAttrName;
    for (i, c) in rest[name_end..].char_indices() {
        let end = lt + 1 + name_end + i;
        let close = |self_closing| {
            Some(Tag {
                name: name.clone(),
                end,
                self_closing,
            })
        };
        if let State::SelfClosing = state {
            if c == '>' {
                return close(true);
            }
            state = State::BeforeAttrName;
        }
        match state {
            State::Quoted(quote) => {
                if c == quote {
                    state = State::BeforeAttrName;
                }
            }
            State::Unquoted => match c {
                '>' => return close(false),
                c if c.is_ascii_whitespace() => state = State::BeforeAttrName,
                _ => {}
            },
            State::BeforeAttrValue => match c {
                '"' | '\'' => state = State::Quoted(c),
                '>' => return close(false),
                c if c.is_ascii_whitespace() => {}
                _ => state = State::Unquoted,
            },
            State::BeforeAttrName | State::AttrName => match c {
                '>' => return close(false),
                '/' => state = State::SelfClosing,
                '=' if matches!(state, State::AttrName) => state = State::BeforeAttrValue,
                c if c.is_ascii_whitespace() => state = State::BeforeAttrName,
                _ => state = State::AttrName,
            },
            State::SelfClosing => unreachable!("resolved above"),
        }
    }
    None
}

/// Replace every CDATA section with its text, escaped for HTML — see
/// [`normalize_storage_format`] for why.
fn unwrap_cdata(html: &str) -> std::borrow::Cow<'_, str> {
    const OPEN: &str = "<![CDATA[";
    const CLOSE: &str = "]]>";
    const ESCAPED_OPEN: &str = "&lt;![CDATA[";
    if !html.contains(OPEN) {
        return std::borrow::Cow::Borrowed(html);
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        // Only a section that CLOSES is one. Every HTML source shares this converter, and an
        // unterminated `<![CDATA[` is far likelier to be prose about XML than a truncated
        // Confluence body — reading the document's remainder as its content turned the rest
        // of a newsletter into escaped literal markup (`raw token\</p>\<p>TAIL`).
        //
        // Having judged it text, ESCAPE it as text — the same thing this does to a section it
        // unwraps, for the same reason. Left as live bytes it is still markup to the parser,
        // which reads `<!…` as a bogus comment running to the next `>`: inside the `<pre><code>`
        // a code macro becomes, that `>` belongs to `</code>`, so the body vanished and the
        // element never closed, rendering the prose after it as inline code. Only the opening
        // tokens are escaped; nothing else in the remainder is touched, which is the point.
        //
        // Every LATER opener gets the same treatment, and that is a deduction rather than a
        // guess: reaching this branch means no `]]>` exists anywhere in the remainder, so each
        // one of them fails the very same test. A document that mentions CDATA once mentions it
        // twice, and escaping only the first left every other one eating to the next `>`.
        let Some(end) = after.find(CLOSE) else {
            out.push_str(ESCAPED_OPEN);
            out.push_str(&after.replace(OPEN, ESCAPED_OPEN));
            rest = "";
            break;
        };
        let (text, tail) = (&after[..end], &after[end + CLOSE.len()..]);
        for c in text.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                _ => out.push(c),
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// Whether a URL is a `data:` URI, tested on the prefix of a borrowed `&str` so a
/// multi-kilobyte base64 payload is never copied just to classify it (the case this
/// exists for). Case-insensitive and tolerant of leading whitespace, matching the
/// leniency browsers apply to the scheme.
fn is_data_uri(url: &str) -> bool {
    let url = url.trim_start();
    url.len() >= 5 && url.as_bytes()[..5].eq_ignore_ascii_case(b"data:")
}

/// `img` handler that drops `data:` URIs — upholding the `lk_core::markdown`
/// cleanliness contract (`scan_defects` finds no `InlineDataUri`) at the conversion
/// boundary. An inlined base64 image (HTML email trackers, embedded logos) converts
/// to a multi-kilobyte single line that bloats vault pages and LLM task inputs while
/// carrying zero retrievable knowledge — so it degrades to its alt text (the
/// loss-averse rule: keep the text content). A fetchable `http(s)` image keeps the
/// standard `![alt](src "title")` form.
fn img_without_data_uris(
    _: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    let attr = |name: &str| {
        element
            .attrs
            .iter()
            .find(|a| &a.name.local == name)
            .map(|a| a.value.as_ref())
    };
    let src = attr("src")?;
    let alt = attr("alt").unwrap_or_default();

    // A data: URI degrades to its alt text as PLAIN body content — not inside
    // `![…]` — so it is emitted verbatim. Markdown escaping here would surface as
    // literal backslashes in the rendered text.
    if is_data_uri(src) {
        return Some(alt.to_string().into());
    }

    // A fetchable image: emit `![alt](src "title")`. Escape only what the syntax
    // demands — parens in the URL, a space-bearing URL wrapped in `<…>`, and the
    // `"`-delimited title's own quotes.
    let src = src.replace('(', "\\(").replace(')', "\\)");
    let (open, close) = if src.contains(' ') {
        ("<", ">")
    } else {
        ("", "")
    };
    let title = attr("title").map_or(String::new(), |t| {
        format!(" \"{}\"", t.replace('"', "\\\""))
    });
    Some(format!("![{alt}]({open}{src}{title}{close})").into())
}

/// Extract the readable article core from a full HTML page and convert it to
/// Markdown, stripping boilerplate (nav, ads, footers) via `dom_smoothie`.
///
/// Returns `None` when extraction fails or yields empty content — it is heuristic
/// and mis-extracts on non-article pages (a sidebar, a near-empty node). The caller
/// owns the fallback because the right one differs: a feed reader keeps its
/// known-clean summary, an importer of a user-chosen file converts the whole page.
/// Folding a full-page fallback in here would let boilerplate longer than a clean
/// summary silently replace it.
pub fn readable_html_to_markdown(html: &str, base_url: &url::Url) -> Option<String> {
    let mut readability = match dom_smoothie::Readability::new(html, Some(base_url.as_str()), None)
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url = %base_url, error = %e, "readability extraction failed");
            return None;
        }
    };
    match readability.parse() {
        Ok(article) => {
            let extracted = html_to_markdown(&article.content);
            if extracted.trim().is_empty() {
                tracing::warn!(url = %base_url, "readability extracted empty content");
                None
            } else {
                Some(extracted)
            }
        }
        Err(e) => {
            tracing::warn!(url = %base_url, error = %e, "readability extraction failed");
            None
        }
    }
}

/// Convert an Atlassian Document Format node tree (Jira rich text) to Markdown.
pub fn adf_to_markdown(node: &Value) -> String {
    let mut out = String::new();
    render_adf(node, &mut out);
    collapse_blank_lines(out.trim())
}

fn render_adf(node: &Value, out: &mut String) {
    match node.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => out.push_str(&apply_marks(node)),
        "hardBreak" => out.push('\n'),
        "rule" => out.push_str("\n---\n\n"),
        "mention" => out.push_str(adf_attr(node, "text").unwrap_or("")),
        "emoji" => out.push_str(
            adf_attr(node, "text")
                .or_else(|| adf_attr(node, "shortName"))
                .unwrap_or(""),
        ),
        "heading" => {
            let level = node
                .get("attrs")
                .and_then(|a| a.get("level"))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 6) as usize;
            out.push_str(&"#".repeat(level));
            out.push(' ');
            render_adf_children(node, out);
            out.push_str("\n\n");
        }
        "paragraph" => {
            render_adf_children(node, out);
            out.push_str("\n\n");
        }
        "bulletList" => render_adf_list(node, out, None),
        "orderedList" => {
            // ADF carries the list's first number in `attrs.order` (Confluence/Jira split a
            // long list by starting the next block at N). Honor it so the number isn't lost;
            // default to 1 when absent.
            let start = node
                .get("attrs")
                .and_then(|a| a.get("order"))
                .and_then(Value::as_u64)
                .map_or(1, |n| n as usize);
            render_adf_list(node, out, Some(start))
        }
        "taskList" => {
            render_adf_children(node, out);
            out.push('\n');
        }
        "taskItem" => {
            let checked = adf_attr(node, "state") == Some("DONE");
            out.push_str(if checked { "- [x] " } else { "- [ ] " });
            render_adf_children(node, out);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        "codeBlock" => {
            let lang = adf_attr(node, "language").unwrap_or("");
            out.push_str("```");
            out.push_str(lang);
            out.push('\n');
            render_adf_children(node, out);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
        "blockquote" => {
            let mut inner = String::new();
            render_adf_children(node, &mut inner);
            for line in inner.trim_end().lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        }
        "table" => render_adf_table(node, out),
        // doc, panel, and anything unrecognized: recurse so text is preserved.
        // For unknown leaf nodes (no content array), rescue common attrs.
        _ => {
            if node.get("content").and_then(Value::as_array).is_some() {
                render_adf_children(node, out);
            } else if let Some(attrs) = node.get("attrs").and_then(Value::as_object) {
                let rescued = attrs
                    .get("url")
                    .or_else(|| attrs.get("href"))
                    .or_else(|| attrs.get("text"))
                    .or_else(|| attrs.get("title"))
                    .and_then(Value::as_str);
                if let Some(value) = rescued {
                    out.push_str(value);
                }
            }
        }
    }
}

fn render_adf_children(node: &Value, out: &mut String) {
    if let Some(content) = node.get("content").and_then(Value::as_array) {
        for child in content {
            render_adf(child, out);
        }
    }
}

fn render_adf_list(node: &Value, out: &mut String, ordered_start: Option<usize>) {
    let Some(items) = node.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut idx = ordered_start.unwrap_or(0);
    for item in items {
        let mut inner = String::new();
        render_adf_children(item, &mut inner);
        let marker = match ordered_start {
            Some(_) => {
                let m = format!("{idx}. ");
                idx += 1;
                m
            }
            None => "- ".to_string(),
        };
        let mut lines = inner.trim().lines();
        if let Some(first) = lines.next() {
            out.push_str(&marker);
            out.push_str(first);
            out.push('\n');
            // Continuation lines (nested lists, multi-paragraph items) align under the text.
            for line in lines {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push('\n');
}

/// Render an ADF table as a GFM pipe table. Each cell is flattened to a single line
/// (cell separators escaped); the first row becomes the GFM header. Ragged rows are
/// padded to the widest row so the column count is consistent.
fn render_adf_table(node: &Value, out: &mut String) {
    let Some(content) = node.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in content {
        if row.get("type").and_then(Value::as_str) != Some("tableRow") {
            continue;
        }
        let Some(cells) = row.get("content").and_then(Value::as_array) else {
            continue;
        };
        let rendered: Vec<String> = cells
            .iter()
            .map(|cell| {
                let mut inner = String::new();
                render_adf_children(cell, &mut inner);
                // A GFM cell is single-line; collapse whitespace and escape the pipe.
                inner
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .replace('|', "\\|")
            })
            .collect();
        if !rendered.is_empty() {
            rows.push(rendered);
        }
    }
    let Some(cols) = rows.iter().map(Vec::len).max() else {
        return;
    };

    for (i, row) in rows.iter().enumerate() {
        out.push('|');
        for c in 0..cols {
            out.push(' ');
            out.push_str(row.get(c).map(String::as_str).unwrap_or(""));
            out.push_str(" |");
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            for _ in 0..cols {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    out.push('\n');
}

fn adf_attr<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.get("attrs")
        .and_then(|a| a.get(key))
        .and_then(Value::as_str)
}

/// Wrap a text leaf's content in the Markdown for each of its ADF marks. Unknown marks
/// (underline, color, …) leave the text untouched so nothing is lost.
fn apply_marks(text_node: &Value) -> String {
    let mut s = text_node
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(marks) = text_node.get("marks").and_then(Value::as_array) {
        for mark in marks {
            s = match mark.get("type").and_then(Value::as_str).unwrap_or("") {
                "strong" => format!("**{s}**"),
                "em" => format!("*{s}*"),
                "code" => format!("`{s}`"),
                "strike" => format!("~~{s}~~"),
                "link" => {
                    let href = mark
                        .get("attrs")
                        .and_then(|a| a.get("href"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    format!("[{s}]({href})")
                }
                _ => s,
            };
        }
    }
    s
}

/// Convert Slack mrkdwn to Markdown: rewrite `<…>` tokens (user/channel mentions, special
/// commands, links), convert emphasis markers (`*bold*` → `**bold**`,
/// `~strike~` → `~~strike~~`), and decode HTML entities. User-id mentions are resolved to
/// display names via `users`.
pub fn slack_to_markdown(text: &str, users: &HashMap<String, String>) -> String {
    let rewritten = rewrite_angle_tokens(text, users);
    let converted = convert_mrkdwn_formatting(&rewritten);
    let decoded = decode_entities(&converted);
    render_emoji_shortcodes(&decoded)
}

fn rewrite_angle_tokens(text: &str, users: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        match after.find('>') {
            Some(gt) => {
                out.push_str(&convert_slack_token(&after[..gt], users));
                rest = &after[gt + 1..];
            }
            // Unbalanced '<' — emit literally and continue past it.
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The label after a `|` in a Slack token (`U123|name` → `name`), if non-empty.
fn label_after_pipe(s: &str) -> Option<&str> {
    s.split('|').nth(1).filter(|l| !l.is_empty())
}

/// `token` is the text between `<` and `>`. Forms: `@U123`/`@U123|name` (user),
/// `#C123`/`#C123|name` (channel), `!here`/`!subteam^S1|@team` (special), or `url`/
/// `url|label` (link).
fn convert_slack_token(token: &str, users: &HashMap<String, String>) -> String {
    if let Some(rest) = token.strip_prefix('@') {
        // Prefer the pipe-label, then the resolved display name, then the raw user id.
        let user_id = rest.split('|').next().unwrap_or(rest);
        let name = label_after_pipe(rest)
            .map(|s| s.to_string())
            .or_else(|| users.get(user_id).cloned())
            .unwrap_or_else(|| user_id.to_string());
        format!("@{name}")
    } else if let Some(rest) = token.strip_prefix('#') {
        format!(
            "#{}",
            label_after_pipe(rest).unwrap_or_else(|| rest.split('|').next().unwrap_or(rest))
        )
    } else if let Some(rest) = token.strip_prefix("!date^") {
        // Date tokens: `!date^timestamp^format|fallback` — emit the fallback text or the
        // raw unix timestamp when no fallback is given.
        match label_after_pipe(rest) {
            Some(fallback) => fallback.to_string(),
            None => rest.split('^').next().unwrap_or(rest).to_string(),
        }
    } else if let Some(rest) = token.strip_prefix('!') {
        match label_after_pipe(rest) {
            Some(label) => format!("@{}", label.trim_start_matches('@')),
            None => format!("@{}", rest.split('^').next().unwrap_or(rest)),
        }
    } else {
        let url = token.split('|').next().unwrap_or("");
        match label_after_pipe(token) {
            Some(label) => format!("[{label}]({url})"),
            None => url.to_string(),
        }
    }
}

/// Convert Slack mrkdwn emphasis markers to standard Markdown:
/// - `*text*` (Slack bold) → `**text**` (Markdown bold)
/// - `~text~` (Slack strikethrough) → `~~text~~` (Markdown strikethrough)
///
/// Markers must be at word boundaries: preceded by whitespace/start-of-string and followed
/// by whitespace/end-of-string (after the closing marker). Content inside backtick code
/// spans and triple-backtick code blocks is left untouched.
fn convert_mrkdwn_formatting(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Skip triple-backtick code blocks.
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            out.push_str("```");
            i += 3;
            // Copy until closing ```.
            while i < len {
                if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
                    out.push_str("```");
                    i += 3;
                    break;
                }
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        // Skip inline code spans.
        if chars[i] == '`' {
            out.push('`');
            i += 1;
            while i < len && chars[i] != '`' {
                out.push(chars[i]);
                i += 1;
            }
            if i < len {
                out.push('`');
                i += 1;
            }
            continue;
        }

        // Try to convert *bold* or ~strike~.
        if (chars[i] == '*' || chars[i] == '~') && is_word_boundary_before(i, &chars) {
            let marker = chars[i];
            if let Some(end) = find_closing_marker(i + 1, marker, &chars) {
                // The closing marker must be followed by a word boundary.
                if is_word_boundary_after(end, &chars) {
                    let inner: String = chars[i + 1..end].iter().collect();
                    if marker == '*' {
                        out.push_str("**");
                        out.push_str(&inner);
                        out.push_str("**");
                    } else {
                        out.push_str("~~");
                        out.push_str(&inner);
                        out.push_str("~~");
                    }
                    i = end + 1;
                    continue;
                }
            }
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

/// CJK characters act as word boundaries for inline formatting — Korean, Chinese, and
/// Japanese text typically has no whitespace between words, but Slack treats CJK characters
/// as natural boundaries for `*bold*` and `~strike~` markers.
fn is_cjk_char(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF    // Hangul Jamo
        | 0x2E80..=0x9FFF  // CJK Radicals through Unified Ideographs (includes Hiragana/Katakana)
        | 0xAC00..=0xD7AF  // Hangul Syllables
        | 0xD7B0..=0xD7FF  // Hangul Jamo Extended-B
        | 0xF900..=0xFAFF  // CJK Compatibility Ideographs
        | 0xFF65..=0xFFDC  // Halfwidth Katakana + Hangul
        | 0x20000..=0x2FA1F // CJK Extension B–F
    )
}

/// The position before `pos` is a word boundary (start of string, whitespace, CJK character,
/// or punctuation that commonly precedes emphasis).
fn is_word_boundary_before(pos: usize, chars: &[char]) -> bool {
    if pos == 0 {
        return true;
    }
    let prev = chars[pos - 1];
    prev.is_whitespace() || is_cjk_char(prev) || matches!(prev, '(' | '[' | '{' | '"' | '\'' | '\n')
}

/// The position after `pos` is a word boundary (end of string, whitespace, CJK character,
/// or punctuation that commonly follows emphasis).
fn is_word_boundary_after(pos: usize, chars: &[char]) -> bool {
    let next_pos = pos + 1;
    if next_pos >= chars.len() {
        return true;
    }
    let next = chars[next_pos];
    next.is_whitespace()
        || is_cjk_char(next)
        || matches!(
            next,
            ')' | ']' | '}' | '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\''
        )
}

/// Find the closing marker character that is not preceded by whitespace (Slack rule:
/// closing markers must be adjacent to the word they wrap). Returns the index of the
/// closing marker, or `None`.
fn find_closing_marker(start: usize, marker: char, chars: &[char]) -> Option<usize> {
    let mut j = start;
    while j < chars.len() {
        if chars[j] == '\n' {
            // Slack inline formatting doesn't span lines.
            return None;
        }
        if chars[j] == marker && j > start && !chars[j - 1].is_whitespace() {
            return Some(j);
        }
        j += 1;
    }
    None
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Render Slack emoji shortcodes (`:tada:`, `:+1:`) as their Unicode glyph — exactly
/// what the author saw — instead of dropping them, since a shortcode like `:100:` or
/// `:rocket:` can carry real meaning. Only shortcodes in the standard Unicode emoji set
/// are converted, and only in prose: a shortcode written as a code literal is content, so
/// code spans are preserved verbatim. Colon-delimited prose (`:default:`, a `key:value:`
/// token) and Slack-specific or workspace-custom names (not in the standard set) are left
/// untouched.
fn render_emoji_shortcodes(text: &str) -> String {
    // Code spans are skipped using the CommonMark rule: a run of N backticks opens a span
    // that closes at the next run of EXACTLY N backticks. This keeps inline (`` `…` ``) and
    // fenced (```` ```…``` ````) code intact even when a fenced body itself contains
    // backticks — a naive split on `` ` `` miscounts parity there and would eat a shortcode
    // inside the fence.
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut prose_start = 0;
    let mut i = 0;
    while i < n {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        // Convert the prose run preceding this backtick run.
        render_prose_shortcodes(&text[prose_start..i], &mut out);
        let open = i;
        while i < n && bytes[i] == b'`' {
            i += 1;
        }
        let run = i - open;
        // Seek the matching closing run of exactly `run` backticks.
        let mut close_end = None;
        let mut j = i;
        while j < n {
            if bytes[j] != b'`' {
                j += 1;
                continue;
            }
            let cstart = j;
            while j < n && bytes[j] == b'`' {
                j += 1;
            }
            if j - cstart == run {
                close_end = Some(j);
                break;
            }
        }
        match close_end {
            // Emit the whole code span (both fences + body) verbatim.
            Some(end) => {
                out.push_str(&text[open..end]);
                i = end;
                prose_start = end;
            }
            // Unclosed run: the backticks are literal text, so the rest is prose again.
            None => {
                out.push_str(&text[open..i]);
                prose_start = i;
            }
        }
    }
    render_prose_shortcodes(&text[prose_start..], &mut out);
    out
}

/// Convert recognized emoji shortcodes in one prose run (no code spans) to their glyph,
/// writing into `out`.
fn render_prose_shortcodes(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        // A shortcode is delimited, not embedded mid-word: the opening colon must not
        // directly follow an alphanumeric, so `key:tada:value` is left intact.
        let boundary = !rest[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric());
        match after.find(':') {
            Some(end) if boundary && end > 0 => match emojis::get_by_shortcode(&after[..end]) {
                Some(emoji) => {
                    out.push_str(emoji.as_str());
                    rest = &after[end + 1..];
                }
                None => {
                    out.push(':');
                    rest = after;
                }
            },
            _ => {
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Machine state is not prose. Degraded to text it welds onto the surrounding words
    /// with no separator — a status macro's base64 state blob and a task's UUID land
    /// mid-sentence, and one real Confluence page came out 30% that. The macro's and the
    /// task's actual content must survive intact.
    #[test]
    fn machine_state_never_becomes_body_text() {
        let macro_html = concat!(
            "<p>Before</p>",
            r#"<ac:structured-macro ac:name="status">"#,
            r#"<ac:parameter ac:name="title">검증</ac:parameter>"#,
            r#"<ac:parameter ac:name="source">JTdCJTIybmFtZSUyMiUzQQ==</ac:parameter>"#,
            "<ac:rich-text-body><p>Real body</p></ac:rich-text-body>",
            "</ac:structured-macro>",
            "<p>After</p>",
        );
        let md = html_to_markdown(macro_html);
        assert!(
            !md.contains("JTdC"),
            "no encoded settings may reach the page:\n{md}"
        );
        assert!(
            !md.contains("검증"),
            "nor a parameter's display value:\n{md}"
        );
        assert!(
            md.contains("Before") && md.contains("After"),
            "prose survives:\n{md}"
        );
        assert!(
            md.contains("Real body"),
            "the macro's own content survives:\n{md}"
        );

        // A task list is far more common than a roadmap macro, and carries three of these.
        let task_html = concat!(
            "<ac:task-list><ac:task>",
            "<ac:task-id>17</ac:task-id>",
            "<ac:task-uuid>0e6f1a-9c</ac:task-uuid>",
            "<ac:task-status>incomplete</ac:task-status>",
            "<ac:task-body><span>Ship the thing</span></ac:task-body>",
            "</ac:task></ac:task-list>",
        );
        let md = html_to_markdown(task_html);
        assert_eq!(
            md, "-   [ ] Ship the thing",
            "a task keeps its prose and its state, and nothing of its identity:\n{md}"
        );
    }

    /// Storage format puts a code macro's body in CDATA, which an HTML5 parser reads as a
    /// bogus COMMENT and drops — so every Confluence code block arrived empty, silently.
    /// Recovering it is only half: left as an unknown element the body degrades to a
    /// paragraph and the converter MARKDOWN-ESCAPES it, injecting backslashes into the
    /// code. It has to arrive as a fenced block, which is the only form that reproduces
    /// the source text.
    #[test]
    fn a_code_body_survives_cdata_and_arrives_as_code_not_escaped_prose() {
        let md = html_to_markdown(concat!(
            r#"<ac:structured-macro ac:name="code">"#,
            r#"<ac:parameter ac:name="language">json</ac:parameter>"#,
            r#"<ac:plain-text-body><![CDATA[{ "a": [1, 2], "b": "x_y", "c": a < b }]]></ac:plain-text-body>"#,
            "</ac:structured-macro>",
        ));
        assert!(md.starts_with("```"), "a code body is a code block:\n{md}");
        assert!(
            md.contains(r#"{ "a": [1, 2], "b": "x_y", "c": a < b }"#),
            "reproduced verbatim — no markdown escaping, no lost `<`:\n{md}"
        );
        assert!(!md.contains("\\["), "no injected backslashes:\n{md}");
    }

    /// A Cloud smart-link carries its settings the same way a macro does, welding a URL
    /// onto the fallback text beside it. The fallback is the readable half and stays.
    #[test]
    fn a_smart_link_keeps_its_fallback_and_drops_its_settings() {
        let md = html_to_markdown(concat!(
            "<p>See <ac:adf-extension><ac:adf-node type=\"inline-card\">",
            r#"<ac:adf-attribute key="url">https://x.example/1</ac:adf-attribute>"#,
            "</ac:adf-node><ac:adf-fallback>the card</ac:adf-fallback></ac:adf-extension></p>",
        ));
        assert_eq!(md, "See the card", "settings out, fallback kept:\n{md}");
    }

    /// An extension's children are ALTERNATIVE renderings of one thing — the ADF node, and a
    /// fallback written for consumers that do not read ADF. Emitted as siblings, a Cloud
    /// panel said everything twice, and Cloud emits both halves for every bodied extension.
    #[test]
    fn an_adf_extension_is_rendered_once() {
        assert_eq!(
            html_to_markdown(concat!(
                r#"<ac:adf-extension><ac:adf-node type="panel">"#,
                r#"<ac:adf-attribute key="panel-type">warning</ac:adf-attribute>"#,
                "<ac:adf-content><p>Do not deploy on Friday.</p></ac:adf-content></ac:adf-node>",
                r#"<ac:adf-fallback><div class="panel"><p>Do not deploy on Friday.</p></div>"#,
                "</ac:adf-fallback></ac:adf-extension>",
            )),
            "Do not deploy on Friday."
        );
        // Offering no fallback, the node's own content is the only alternative there is.
        assert_eq!(
            html_to_markdown(concat!(
                r#"<ac:adf-extension><ac:adf-node type="panel">"#,
                "<ac:adf-content><p>Only copy.</p></ac:adf-content>",
                "</ac:adf-node></ac:adf-extension>",
            )),
            "Only copy."
        );
        // A fallback is what to use when the thing before it cannot be used, so a fallback
        // that is only a placeholder never displaces the body it stands in for.
        assert_eq!(
            html_to_markdown(concat!(
                r#"<ac:adf-extension><ac:adf-node type="extension">"#,
                "<ac:adf-content><p>The real macro body.</p></ac:adf-content></ac:adf-node>",
                "<ac:adf-fallback><p>This macro is not available.</p></ac:adf-fallback>",
                "</ac:adf-extension>",
            )),
            "The real macro body."
        );
        // Each child is asked for its OWN rendering, so one that is not an element wrapper
        // is not skipped over into nothing.
        assert_eq!(
            html_to_markdown("<p>Before</p><ac:adf-extension>Bare text</ac:adf-extension>"),
            "Before\n\nBare text"
        );
        // The children are asked WHICH they are, never where they sit — a rule keyed to
        // position carries the premise that the fallback comes second, and a premise that
        // fails does so silently, handing the stand-in the win it must never have.
        assert_eq!(
            html_to_markdown(concat!(
                "<ac:adf-extension><ac:adf-fallback>STAND-IN</ac:adf-fallback>",
                r#"<ac:adf-node type="panel"><ac:adf-content><p>The body.</p>"#,
                "</ac:adf-content></ac:adf-node></ac:adf-extension>",
            )),
            "The body."
        );
    }

    /// Only a section that CLOSES is one, and a document with no CDATA at all must come back
    /// untouched.
    ///
    /// Every HTML source shares this converter, so an unterminated `<![CDATA[` is far likelier
    /// to be prose about XML than a truncated Confluence body. Reading the remainder of the
    /// document as its content escaped the rest of a newsletter into literal markup.
    ///
    /// Having judged it text, it is escaped AS text. Left as live bytes it was still markup to
    /// the parser — `<!…` opens a bogus comment running to the next `>`, which inside the
    /// `<pre><code>` a code macro becomes is the one in `</code>`, so the body disappeared and
    /// the element never closed, rendering the prose after it as inline code.
    #[test]
    fn cdata_edges_are_exact() {
        assert_eq!(
            html_to_markdown("<p>raw <![CDATA[ token</p><p>Next paragraph</p>"),
            "raw <!\\[CDATA\\[ token\n\nNext paragraph"
        );
        // And every LATER opener, which is a deduction rather than a guess: no `]]>` exists
        // anywhere in the remainder, so each one fails the same test. Escaping only the first
        // left the rest eating to the next `>`, and a document that mentions CDATA once
        // mentions it twice.
        assert_eq!(
            html_to_markdown("<p>a <![CDATA[ x</p><p>b <![CDATA[ y</p><p>TAIL</p>"),
            "a <!\\[CDATA\\[ x\n\nb <!\\[CDATA\\[ y\n\nTAIL"
        );
        // The shape that corrupted: a well-formed code body whose section never closes. The
        // body survives, the fence closes, and the paragraph after it is a paragraph.
        assert_eq!(
            html_to_markdown(concat!(
                "<ac:plain-text-body><![CDATA[fn main() {}</ac:plain-text-body>",
                "<p>Next section</p>",
            )),
            "```\n<![CDATA[fn main() {}\n```\n\nNext section"
        );
        assert_eq!(html_to_markdown("<p>plain</p>"), "plain");
        assert_eq!(html_to_markdown("<p><![CDATA[]]>empty</p>"), "empty");
    }

    /// The one place the CDATA unwrap and the tag rewrite disagree, pinned so it is a decided
    /// property rather than an accident — and so anything that changes it has to say why.
    ///
    /// `unwrap_cdata` does not share the raw-text skipping, so inside a RAWTEXT element it
    /// escapes text a parser would have read as text. The exposure is the raw-text elements
    /// whose content is KEPT — `xmp`/`iframe`/`noembed`/`noframes`/`noscript`, the cases below —
    /// and the cost is a literal entity; see `normalize_storage_format` for why closing it is
    /// not worth what closing it would cost.
    #[test]
    fn cdata_inside_a_raw_text_element_keeps_its_entities() {
        assert_eq!(
            html_to_markdown("<xmp>sample <![CDATA[a < b & c]]> end</xmp>"),
            "sample a &lt; b &amp; c end"
        );
        // The same holds for the bare opening token of a section that never closes. Escaping
        // that one buys nothing here — RAWTEXT has no bogus-comment state to protect against —
        // so this is the second shape the gap takes, not a separate one.
        assert_eq!(
            html_to_markdown("<xmp>text <![CDATA[ unterminated</xmp>"),
            "text &lt;!\\[CDATA\\[ unterminated"
        );
        // `iframe` and `noscript` are the reachable two — RSS full-text fetching pulls arbitrary
        // pages — so both are pinned by name rather than left standing behind `xmp`. `noscript`
        // is raw text that is KEPT, which is what puts it in this set and not in the dropped one.
        assert_eq!(
            html_to_markdown("<iframe>fallback mentions <![CDATA[ never closes</iframe>"),
            "fallback mentions &lt;!\\[CDATA\\[ never closes"
        );
        assert_eq!(
            html_to_markdown("<noscript>alt text <![CDATA[a < b]]> end</noscript>"),
            "alt text a &lt; b end"
        );
        // A text box is RCDATA, so the entities decode again and nothing shows.
        assert_eq!(
            html_to_markdown("<textarea>note <![CDATA[a < b & c]]> end</textarea>"),
            "note a < b & c end"
        );
    }

    /// A resource identifier carries its label in an ATTRIBUTE, so degrading it to its
    /// empty text drops the reference entirely — every Confluence→Confluence
    /// cross-reference, which is exactly the material a knowledge vault wants.
    #[test]
    fn a_resource_identifier_keeps_the_label_its_attribute_carries() {
        assert_eq!(
            html_to_markdown(
                r#"<p>See <ac:link><ri:page ri:content-title="Design Notes"/></ac:link></p>"#
            ),
            "See Design Notes"
        );
        assert_eq!(
            html_to_markdown(
                r#"<p>File <ac:link><ri:attachment ri:filename="spec.pdf"/></ac:link></p>"#
            ),
            "File spec.pdf"
        );
        // An opaque account id is machine state; emitting it would be the very defect the
        // rest of this file prevents, so a user mention stays dropped.
        assert_eq!(
            html_to_markdown(
                r#"<p>Ask <ac:link><ri:user ri:account-id="557058:abc"/></ac:link></p>"#
            ),
            "Ask"
        );
    }

    /// An external reference, an inline date and an emoji carry what the reader saw in an
    /// attribute exactly as a page reference does, so degrading them to their empty text
    /// dropped them silently — a sentence lost its link, its date, or its glyph with nothing
    /// left behind to notice.
    #[test]
    fn every_attribute_borne_value_survives_its_empty_element() {
        assert_eq!(
            html_to_markdown(
                r#"<p>See <ac:link><ri:url ri:value="https://example.com/doc"/></ac:link>.</p>"#
            ),
            "See https://example.com/doc."
        );
        assert_eq!(
            html_to_markdown(r#"<p>Due <time datetime="2020-12-25"/> for review.</p>"#),
            "Due 2020-12-25 for review."
        );
        assert_eq!(
            html_to_markdown(r#"<p>Great <ac:emoticon ac:emoji-fallback="🙂"/> work</p>"#),
            "Great 🙂 work"
        );
        // Where an element carries both, its text is what the page reads.
        assert_eq!(
            html_to_markdown(r#"<p>On <time datetime="2020-12-25">Christmas</time>.</p>"#),
            "On Christmas."
        );
    }

    /// An XHTML empty element must not adopt the content that follows it. HTML has no
    /// self-closing form for a non-void element, so `<ac:parameter …/>` opens one and takes
    /// every following sibling as a child — which the handler that drops it then drops too.
    /// Confluence writes an unset parameter exactly this way and puts parameters BEFORE the
    /// body, so the macro's entire content disappeared: total, silent, and on the ordinary
    /// shape of the input rather than an exotic one.
    #[test]
    fn an_empty_element_does_not_swallow_what_follows_it() {
        assert_eq!(
            html_to_markdown(
                r#"<ac:structured-macro ac:name="info"><ac:parameter ac:name="icon"/><ac:rich-text-body><p>IMPORTANT NOTICE</p></ac:rich-text-body></ac:structured-macro>"#
            ),
            "IMPORTANT NOTICE"
        );
        // The same shape costs a code macro the body the CDATA fix exists to recover.
        assert_eq!(
            html_to_markdown(
                r#"<ac:structured-macro ac:name="code"><ac:parameter ac:name="language"/><ac:plain-text-body><![CDATA[let x = 1;]]></ac:plain-text-body></ac:structured-macro>"#
            ),
            "```\nlet x = 1;\n```"
        );
        // A task list is deleted whole, since its id opens before the body that names it.
        assert_eq!(
            html_to_markdown(
                r#"<ac:task-list><ac:task><ac:task-id/><ac:task-status>incomplete</ac:task-status><ac:task-body>Ship the thing</ac:task-body></ac:task></ac:task-list>"#
            ),
            "-   [ ] Ship the thing"
        );
        // And a resource identifier — handled by attribute, children ignored — takes the
        // rest of the paragraph with it.
        assert_eq!(
            html_to_markdown(
                r#"<p>A <ac:link><ri:page ri:content-title="T"/></ac:link> <span>and B</span></p>"#
            ),
            "A T and B"
        );
    }

    /// Void elements keep their form: an HTML parser reads `<br></br>` as TWO breaks, so
    /// expanding those would invent content instead of preserving it.
    #[test]
    fn a_void_element_is_not_given_a_closing_tag() {
        assert_eq!(html_to_markdown("<p>one<br/>two</p>"), "one  \ntwo");
        assert_eq!(
            html_to_markdown(r#"<p><img src="https://x.example/a.png" alt="ALT"/>after</p>"#),
            "![ALT](https://x.example/a.png)after"
        );
    }

    /// Each element this converter treats specially is claimed by exactly ONE rule.
    ///
    /// Registering a tag twice is silent: the later handler wins and the earlier becomes dead
    /// code that still reads as live. Naming a REWRITTEN element is silent the other way — the
    /// rewrite runs first, so by the time handlers see the document that name is gone and its
    /// handler never fires. Neither surfaces as a failure anywhere, so the overlap is what has
    /// to be impossible rather than the symptom.
    #[test]
    fn no_element_is_claimed_by_two_rules() {
        let mut claimed: Vec<&str> = Vec::new();
        for handled in [
            MACHINE_STATE_ELEMENTS,
            &["ac:task-status"],
            &["ac:link-body", "ac:plain-text-link-body"],
            &["ac:link"],
            &["ac:adf-extension"],
            &["img"],
        ] {
            claimed.extend(handled);
        }
        claimed.extend(ATTRIBUTE_BORNE_TEXT.iter().map(|(tag, _)| *tag));
        let mut seen = std::collections::HashSet::new();
        for tag in &claimed {
            assert!(seen.insert(*tag), "`{tag}` is registered by two handlers");
        }
        for (rewritten, _, _) in REWRITTEN_ELEMENTS {
            assert!(
                !seen.contains(rewritten),
                "`{rewritten}` is rewritten before any handler could see it"
            );
        }
    }

    /// A stylesheet and a script are machine state arriving from ordinary HTML rather than from
    /// Confluence, and the rule is about what the text IS. One real vault page carries
    /// `.abbel-fig { display: block; … }` mid-article, lifted out of an RSS feed's `<style>`.
    #[test]
    fn a_stylesheet_is_not_prose() {
        assert_eq!(
            html_to_markdown("<p>Lead</p><style>.fig { display: block; }</style><p>Body</p>"),
            "Lead\n\nBody"
        );
        assert_eq!(
            html_to_markdown(r#"<p>Lead</p><script>var x = "<div/>";</script><p>Body</p>"#),
            "Lead\n\nBody"
        );
        // A text box holds what a person typed, so it is prose and stays.
        assert_eq!(
            html_to_markdown("<textarea>user typed this</textarea>"),
            "user typed this"
        );
    }

    /// A checklist is where a working page keeps its decisions and follow-ups. Left unmapped,
    /// its items degraded to bare text and ran together into one string, taking the list —
    /// and with it which items were done — with them.
    #[test]
    fn a_task_list_arrives_as_a_checklist() {
        assert_eq!(
            html_to_markdown(
                r#"<ac:task-list><ac:task><ac:task-id>1</ac:task-id><ac:task-status>complete</ac:task-status><ac:task-body>Rotate the signing key</ac:task-body></ac:task><ac:task><ac:task-id>2</ac:task-id><ac:task-status>incomplete</ac:task-status><ac:task-body>Update the runbook</ac:task-body></ac:task></ac:task-list>"#
            ),
            "-   [x] Rotate the signing key\n-   [ ] Update the runbook"
        );
    }

    /// A rewritten element is matched by NAME on both halves. Testing for the bare open token
    /// while replacing every close tag left one carrying an attribute unmatched on open and
    /// matched on close, so the replacement closed an enclosing block early.
    #[test]
    fn a_rewritten_element_is_matched_with_its_attributes() {
        assert_eq!(
            html_to_markdown(
                r#"<ac:plain-text-body id="x"><![CDATA[let x = 1;]]></ac:plain-text-body>"#
            ),
            "```\nlet x = 1;\n```"
        );
    }

    /// A parser reads a script, a stylesheet and a text box as TEXT, so a `/>` inside one is
    /// not a tag. Rewriting there is a pre-parse pass corrupting the document it exists to
    /// leave alone: `var x = "<div/>"` came back carrying an injected `</div>` inside the
    /// JavaScript string.
    #[test]
    fn raw_text_content_is_never_rewritten() {
        for html in [
            r#"<p>A</p><script>var x = "<div/>";</script>"#,
            r#"<p>A</p><style>.x{content:"<i/>"}</style>"#,
            r#"<p>A</p><TEXTAREA><ac:parameter/></TEXTAREA>"#,
            // What ends the span is an end tag whose NAME matches, which HTML decides on the
            // character after it. Matching the name as a mere prefix ended the span at a
            // person's own words and rewrote everything they typed after them — and a text
            // box is the raw-text element whose content is kept rather than dropped, so
            // nothing else would have caught it.
            r#"<textarea>I typed </textareas> then <div/> too</textarea>"#,
            r#"<p>A</p><iframe>x </iframes> <div/> y</iframe>"#,
            r#"<p>A</p><textarea>never closed <div/>"#,
            // HTML ends a tag name on ASCII whitespace only, so a parser reads these as
            // unclosed spans. A Unicode whitespace test closes them instead — the same early
            // ending by another route, and an ideographic space is ordinary CJK text.
            "<textarea>t</textarea\u{3000}> x <div/> y</textarea>",
            "<textarea>t</textarea\u{00A0}> x <div/> y</textarea>",
            // Scripting is a parser's default, which makes this raw text like the rest; it is
            // ordinary in email and fetched articles as a lazy-loaded image's fallback.
            r#"<p>A</p><noscript>var x = "<div/>";</noscript><p>B</p>"#,
        ] {
            let md = html_to_markdown(html);
            assert!(
                !md.contains("</div>") && !md.contains("</i>") && !md.contains("</ac:parameter>"),
                "an end tag was injected into raw text:\n{md}"
            );
        }
        // The span still ends where it really ends, whatever the end tag's case or spacing.
        for html in [
            "<TEXTAREA>typed</TEXTAREA><p>After</p>",
            "<textarea>typed</textarea ><p>After</p>",
        ] {
            assert!(
                html_to_markdown(html).contains("After"),
                "the raw-text span swallowed the document"
            );
        }
    }

    /// The scan follows the tokenizer's tag states, so a `/>` counts only where the parser
    /// would see one — inside a quoted attribute value it is value text, and a URL ending in
    /// a slash does not turn its element empty.
    #[test]
    fn a_slash_inside_an_attribute_value_does_not_close_the_tag() {
        assert_eq!(
            html_to_markdown(r#"<p>A <ac:link><ri:page ri:content-title="a/>b"/></ac:link> B</p>"#),
            "A a/>b B"
        );
        // A comment's contents are not markup either.
        assert_eq!(
            html_to_markdown(r#"<p>A<!-- <ac:parameter ac:name="x"/> -->B</p>"#),
            "AB"
        );
    }

    /// A link can carry the label of its target AND the display text its author typed. Left
    /// as siblings they weld into one word that is on neither the page nor in any vocabulary
    /// downstream recognises — so they are separated, and a link carrying only one of the two
    /// gains no stray space from the rule.
    #[test]
    fn a_link_keeps_its_target_and_its_display_text_apart() {
        assert_eq!(
            html_to_markdown(
                r#"<p>See <ac:link><ri:page ri:content-title="Design Notes"/><ac:plain-text-link-body><![CDATA[the notes]]></ac:plain-text-link-body></ac:link>.</p>"#
            ),
            "See Design Notes the notes."
        );
        assert_eq!(
            html_to_markdown(
                r#"<p>See <ac:link ac:anchor="Sec"><ac:plain-text-link-body><![CDATA[that section]]></ac:plain-text-link-body></ac:link>.</p>"#
            ),
            "See that section."
        );
        assert_eq!(
            html_to_markdown(
                r#"<p>See <ac:link><ri:page ri:content-title="Design Notes"/></ac:link>.</p>"#
            ),
            "See Design Notes."
        );
    }

    use serde_json::json;

    #[test]
    fn adf_paragraph_with_marks_and_link() {
        let adf = json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [
                    {"type": "text", "text": "see "},
                    {"type": "text", "text": "the docs", "marks": [
                        {"type": "strong"},
                        {"type": "link", "attrs": {"href": "https://x.io"}}
                    ]},
                    {"type": "text", "text": " now"}
                ]
            }]
        });
        assert_eq!(
            adf_to_markdown(&adf),
            "see [**the docs**](https://x.io) now"
        );
    }

    #[test]
    fn adf_heading_and_bullets() {
        let adf = json!({
            "type": "doc",
            "content": [
                {"type": "heading", "attrs": {"level": 2},
                 "content": [{"type": "text", "text": "Plan"}]},
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "first"}]}]},
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "second"}]}]}
                ]}
            ]
        });
        assert_eq!(adf_to_markdown(&adf), "## Plan\n\n- first\n- second");
    }

    #[test]
    fn adf_ordered_list_honors_start_number() {
        // `attrs.order` is the list's first number (a split/continued list); honor it.
        let adf = json!({
            "type": "doc",
            "content": [{"type": "orderedList", "attrs": {"order": 3}, "content": [
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "a"}]}]},
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "b"}]}]}
            ]}]
        });
        assert_eq!(adf_to_markdown(&adf), "3. a\n4. b");

        // No `order` attr → default first number 1.
        let adf = json!({
            "type": "doc",
            "content": [{"type": "orderedList", "content": [
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "x"}]}]}
            ]}]
        });
        assert_eq!(adf_to_markdown(&adf), "1. x");
    }

    #[test]
    fn adf_code_block_keeps_content() {
        let adf = json!({
            "type": "doc",
            "content": [{
                "type": "codeBlock", "attrs": {"language": "rust"},
                "content": [{"type": "text", "text": "let x = 1;"}]
            }]
        });
        assert_eq!(adf_to_markdown(&adf), "```rust\nlet x = 1;\n```");
    }

    #[test]
    fn adf_unknown_node_rescues_url_attr() {
        let adf = json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [
                    {"type": "text", "text": "see "},
                    {"type": "inlineCard", "attrs": {"url": "https://example.com/page"}},
                ]
            }]
        });
        assert_eq!(adf_to_markdown(&adf), "see https://example.com/page");
    }

    #[test]
    fn adf_unknown_node_without_attrs_is_silent() {
        let adf = json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "unknownThing"},
                ]
            }]
        });
        assert_eq!(adf_to_markdown(&adf), "ok");
    }

    #[test]
    fn adf_table_renders_as_pipe_table() {
        let adf = json!({
            "type": "table",
            "content": [
                {"type": "tableRow", "content": [
                    {"type": "tableHeader", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Name"}]}
                    ]},
                    {"type": "tableHeader", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Role"}]}
                    ]}
                ]},
                {"type": "tableRow", "content": [
                    {"type": "tableCell", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Ada"}]}
                    ]},
                    {"type": "tableCell", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Eng"}]}
                    ]}
                ]}
            ]
        });
        assert_eq!(
            adf_to_markdown(&adf),
            "| Name | Role |\n| --- | --- |\n| Ada | Eng |"
        );
    }

    fn no_users() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn slack_tokens_rewritten() {
        let u = no_users();
        assert_eq!(slack_to_markdown("hi <@U123>", &u), "hi @U123");
        assert_eq!(slack_to_markdown("<@U1|june> shipped", &u), "@june shipped");
        assert_eq!(slack_to_markdown("see <#C1|general>", &u), "see #general");
        assert_eq!(
            slack_to_markdown("docs <https://x.io|here>", &u),
            "docs [here](https://x.io)"
        );
        assert_eq!(
            slack_to_markdown("raw <https://x.io>", &u),
            "raw https://x.io"
        );
        assert_eq!(slack_to_markdown("<!here> ping", &u), "@here ping");
    }

    #[test]
    fn slack_resolves_user_id_to_display_name() {
        let mut users = HashMap::new();
        users.insert("U123".to_string(), "Alice".to_string());
        assert_eq!(slack_to_markdown("hi <@U123>", &users), "hi @Alice");
        // Pipe-label still takes priority over the resolved name.
        assert_eq!(slack_to_markdown("<@U123|bob> said", &users), "@bob said");
    }

    #[test]
    fn slack_date_token_with_fallback() {
        let u = no_users();
        assert_eq!(
            slack_to_markdown("due <!date^1234567890^{date}|May 24, 2026>", &u),
            "due May 24, 2026"
        );
    }

    #[test]
    fn slack_date_token_without_fallback() {
        let u = no_users();
        assert_eq!(
            slack_to_markdown("at <!date^1234567890^{date}>", &u),
            "at 1234567890"
        );
    }

    #[test]
    fn slack_bold_converted() {
        let u = no_users();
        assert_eq!(
            slack_to_markdown("this is *bold* text", &u),
            "this is **bold** text"
        );
    }

    #[test]
    fn slack_strike_converted() {
        let u = no_users();
        assert_eq!(
            slack_to_markdown("this is ~struck~ out", &u),
            "this is ~~struck~~ out"
        );
    }

    #[test]
    fn slack_bold_not_converted_mid_word() {
        let u = no_users();
        assert_eq!(slack_to_markdown("file*name*here", &u), "file*name*here");
    }

    #[test]
    fn slack_bold_in_code_not_converted() {
        let u = no_users();
        assert_eq!(slack_to_markdown("`*bold*`", &u), "`*bold*`");
    }

    #[test]
    fn slack_bold_with_cjk_boundary() {
        let u = no_users();
        // CJK characters act as word boundaries — Slack renders *bold* adjacent to Korean text.
        assert_eq!(
            slack_to_markdown("한글*bold*텍스트", &u),
            "한글**bold**텍스트"
        );
        assert_eq!(
            slack_to_markdown("결과~삭제~했습니다", &u),
            "결과~~삭제~~했습니다"
        );
    }

    #[test]
    fn slack_decodes_entities() {
        assert_eq!(
            slack_to_markdown("a &lt;b&gt; &amp; c", &no_users()),
            "a <b> & c"
        );
    }

    #[test]
    fn slack_emoji_shortcodes_render_to_glyphs() {
        let u = no_users();
        // Recognized Unicode emoji shortcodes render as their glyph — faithful to what the
        // author saw, never dropped (a `:100:`/`:rocket:` can carry real meaning).
        assert_eq!(slack_to_markdown(":+1: great", &u), "👍 great");
        assert_eq!(slack_to_markdown(":pray::fire:", &u), "🙏🔥");
        // A workspace-custom emoji is NOT in the standard set, so it survives as literal
        // text rather than risk mangling a real word that looks like a shortcode.
        assert_eq!(slack_to_markdown(":custom_parrot:", &u), ":custom_parrot:");
    }

    #[test]
    fn slack_emoji_render_preserves_prose_and_colon_delimited_words() {
        let u = no_users();
        // A colon embedded mid-word is not an emoji shortcode — `key:value:pair`
        // technical text must keep its middle token, not be touched.
        assert_eq!(
            slack_to_markdown("config:value:here", &u),
            "config:value:here"
        );
        assert_eq!(slack_to_markdown("app:icon:large", &u), "app:icon:large");
        // A delimited word that merely LOOKS like a shortcode but isn't a real emoji
        // (a Ruby symbol, colon-emphasis) must be preserved, not converted.
        assert_eq!(
            slack_to_markdown("the :default: value", &u),
            "the :default: value"
        );
        assert_eq!(
            slack_to_markdown("mark :important:", &u),
            "mark :important:"
        );
        // Delimited shortcodes render to glyphs, even adjacent to punctuation.
        assert_eq!(slack_to_markdown("nice (:tada:)", &u), "nice (🎉)");
        assert_eq!(slack_to_markdown("done :+1:", &u), "done 👍");
    }

    #[test]
    fn slack_emoji_render_preserves_shortcodes_in_code_spans() {
        let u = no_users();
        // A shortcode written as a code literal is content, not decoration — a `code`
        // span must survive verbatim even when it holds a real emoji shortcode, while a
        // bare prose shortcode on the same line still renders to its glyph.
        assert_eq!(
            slack_to_markdown("use `:tada:` here :tada:", &u),
            "use `:tada:` here 🎉"
        );
        assert_eq!(
            slack_to_markdown("the `:x:` marker", &u),
            "the `:x:` marker"
        );
        // A fenced block whose body itself contains backticks: the shortcode inside must
        // survive (the opening ``` matches the closing ```; the inner single backticks
        // don't close it). A naive backtick-parity split mis-counts here and converts it.
        assert_eq!(
            slack_to_markdown("```\nlet s = `:tada:`;\n```", &u),
            "```\nlet s = `:tada:`;\n```"
        );
        // A double-backtick inline span is a code span too (run length 2).
        assert_eq!(slack_to_markdown("``:tada:``", &u), "``:tada:``");
    }

    #[test]
    fn html_to_markdown_converts_list_and_link() {
        let md = html_to_markdown("<ul><li>one</li><li>two</li></ul>");
        assert!(md.contains("one") && md.contains("two") && md.contains("-"));
        let link = html_to_markdown(r#"<a href="https://x.io">x</a>"#);
        assert!(link.contains("[x](https://x.io)"));
    }

    #[test]
    fn html_img_data_uri_degrades_to_alt_text() {
        // Exercised on `html_to_markdown` directly — the single seam every HTML-consuming
        // adapter (Gmail, RSS, Calendar, Manual) shares — so the policy holds for all of
        // them, not just the Gmail path where the bloat was first observed.
        // An inlined base64 image (an HTML email's embedded logo/table art) would
        // otherwise become a multi-kilobyte single markdown line. It carries no
        // retrievable knowledge — only its alt text survives.
        let md = html_to_markdown(
            r#"<p>before</p><img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUg" alt="회사 로고"><p>after</p>"#,
        );
        assert!(!md.contains("data:"), "data: URI must be dropped:\n{md}");
        assert!(md.contains("회사 로고"), "alt text must survive:\n{md}");
        assert!(md.contains("before") && md.contains("after"));

        // Without alt text the image vanishes entirely.
        let bare = html_to_markdown(r#"<img src="data:image/gif;base64,R0lGOD">"#);
        assert_eq!(bare, "");
    }

    #[test]
    fn html_img_http_src_keeps_standard_markdown() {
        let md = html_to_markdown(r#"<img src="https://x.io/a.png" alt="chart" title="Q2">"#);
        assert_eq!(md, "![chart](https://x.io/a.png \"Q2\")");
        // Parentheses in the URL are escaped, as in htmd's built-in handler.
        let parens = html_to_markdown(r#"<img src="https://x.io/a(1).png">"#);
        assert!(parens.contains("![](https://x.io/a\\(1\\).png)"));
    }

    #[test]
    fn html_img_title_quote_is_escaped_so_markdown_stays_valid() {
        // A quote inside the title would otherwise close the `"`-delimited title
        // early and emit broken markdown — it must be escaped. (Single-quoted HTML
        // attribute so the inner double quotes are real, not attribute delimiters.)
        let md = html_to_markdown(r#"<img src='https://x.io/a.png' title='he said "hi"'>"#);
        assert_eq!(md, r#"![](https://x.io/a.png "he said \"hi\"")"#);
    }

    #[test]
    fn html_img_data_uri_with_leading_space_and_caps_is_dropped() {
        // The scheme test tolerates leading whitespace and case, so a `  DATA:`
        // payload can't slip through as a giant inline blob.
        let md = html_to_markdown(r#"<img src="  DATA:image/png;base64,AAAA" alt="x">"#);
        assert_eq!(md, "x");
    }

    #[test]
    fn html_img_data_uri_alt_is_plain_text_not_markdown_escaped() {
        // The degraded alt becomes PLAIN body text (not inside `![…]`), so a quote in
        // it must appear verbatim — escaping it here would leak a literal backslash
        // into the rendered prose.
        let md = html_to_markdown(r#"<img src='data:image/png;base64,AAAA' alt='he said "hi"'>"#);
        assert_eq!(md, r#"he said "hi""#);
    }

    #[test]
    fn empty_inputs_are_empty() {
        assert_eq!(html_to_markdown("   "), "");
        assert_eq!(slack_to_markdown("", &no_users()), "");
    }

    #[test]
    fn readable_returns_none_when_no_article_core() {
        // No extractable article body — the helper signals failure rather than
        // falling back to boilerplate, so the caller keeps its known-clean content.
        let base = url::Url::parse("https://example.com/").unwrap();
        assert_eq!(
            readable_html_to_markdown("<html><body></body></html>", &base),
            None
        );
    }

    #[test]
    fn readable_returns_some_for_an_article() {
        let base = url::Url::parse("https://example.com/post").unwrap();
        let html = format!(
            "<html><body><article><h1>Title</h1>{}</article></body></html>",
            "<p>This is a substantial paragraph of article prose worth extracting.</p>".repeat(6)
        );
        let extracted = readable_html_to_markdown(&html, &base).expect("article extracted");
        assert!(extracted.contains("substantial paragraph"));
    }

    #[test]
    fn readable_absolutizes_relative_urls_against_base() {
        // dom_smoothie resolves relative URLs against base_url during extraction. RSS
        // full-text feeds rely on this (links/images must be clickable once detached from
        // the feed). Locked here because the other readable_* tests don't assert on URLs, so
        // a future dom_smoothie upgrade that changes URL resolution would otherwise pass CI.
        let base = url::Url::parse("https://example.com/post").unwrap();
        let para = "<p>This is a substantial paragraph of article prose with a \
                    <a href=\"/rel/page\">relative link</a> worth extracting as content.</p>";
        let html = format!(
            "<html><body><article><h1>Title</h1>{}</article></body></html>",
            para.repeat(6)
        );
        let extracted = readable_html_to_markdown(&html, &base).expect("article extracted");
        assert!(
            extracted.contains("https://example.com/rel/page"),
            "relative link must be absolutized against base_url:\n{extracted}"
        );
    }

    // Adversarial property tests: throw randomized hostile HTML at the converter and
    // assert the vault-text cleanliness contract holds for ANY input — closing the
    // whole "raw source bytes leak into a page" class instead of one example at a
    // time. The invariant is `lk_core::markdown::scan_defects` (the SAME predicate
    // `lore doctor` checks at rest), so code-side and data-side can't drift.
    mod properties {
        use super::*;
        use proptest::prelude::*;

        /// A fragment of attacker-controlled base64-ish payload.
        fn base64_blob() -> impl Strategy<Value = String> {
            "[A-Za-z0-9+/]{0,300}"
        }

        /// Arbitrary alt/surrounding text. Excludes only `<`, `>`, and `]` — the
        /// characters that START a defect signature (`<data:`, `](data:`) — so the
        /// adversarial text can never itself spell a data: URI and produce a FALSE
        /// positive. Everything else (quotes, parens, colons, `data:` as a word) is
        /// fair game, because the property under test is "the CONVERTER never
        /// introduces a data: URI", not "no input ever mentions one".
        fn loose_text() -> impl Strategy<Value = String> {
            r#"[\PC]{0,40}"#.prop_map(|s| s.replace(['<', '>', ']'], ""))
        }

        proptest! {
            #[test]
            fn html_to_markdown_never_emits_a_data_uri(
                blob in base64_blob(),
                alt in loose_text(),
                lead in loose_text(),
            ) {
                // An inlined base64 image embedded anywhere in a fragment must never
                // survive into the output, regardless of alt text or surrounding prose.
                let html = format!(
                    "<p>{lead}</p><img src=\"data:image/png;base64,{blob}\" alt=\"{alt}\">"
                );
                let md = html_to_markdown(&html);
                prop_assert!(
                    lk_core::markdown::scan_defects(&md).is_empty(),
                    "data: URI survived conversion:\n{md}"
                );
            }

            #[test]
            fn html_to_markdown_keeps_fetchable_image_links(
                blob in base64_blob(),
            ) {
                // A real http(s) image is knowledge-bearing and must be preserved as a
                // standard link — proving the filter targets data: URIs specifically,
                // not all images (which would be an over-broad, lossy constraint).
                let html = format!("<img src=\"https://x.io/{blob}.png\" alt=\"chart\">");
                let md = html_to_markdown(&html);
                if !blob.is_empty() {
                    prop_assert!(md.contains("https://x.io/"), "http image dropped:\n{md}");
                }
                prop_assert!(lk_core::markdown::scan_defects(&md).is_empty());
            }

            /// `rewrite_tags` walks bytes itself rather than parsing, so it owns every
            /// slice boundary it takes — and a `&str` sliced off a char boundary is a panic,
            /// not a wrong answer. Adapters feed it whatever a server returned, so arbitrary
            /// text with multibyte characters pressed against tag syntax is the real input
            /// rather than a hypothetical one.
            ///
            /// The other half of the property is that it leaves alone what it has no business
            /// touching: nothing can need expanding where nothing spells an empty element, and
            /// borrowing rather than rewriting is what keeps every non-Confluence source —
            /// Gmail, RSS, Calendar — converting exactly as it did before.
            #[test]
            fn expanding_empty_elements_survives_arbitrary_bytes(
                text in r#"[\PC]{0,60}"#,
                tail in r#"[\PC]{0,20}"#,
            ) {
                for shape in [
                    format!("<p>{text}</p>"),
                    format!("<{text}/>{tail}"),
                    format!("<p a=\"{text}\"/>{tail}"),
                    format!("<p a={text}/>{tail}"),
                    format!("<!--{text}--><x/>{tail}"),
                    format!("<x a='{text}'{tail}"),
                    format!("{text}<"),
                    format!("<![CDATA[{text}]]><y/>{tail}"),
                ] {
                    match rewrite_tags(&shape) {
                        std::borrow::Cow::Borrowed(same) => prop_assert_eq!(same, &shape),
                        std::borrow::Cow::Owned(_) => prop_assert!(
                            shape.contains("/>")
                                || REWRITTEN_ELEMENTS.iter().any(|(t, _, _)| shape.contains(t)),
                            "rewrote an input naming nothing it rewrites:\n{}", shape
                        ),
                    }
                    let _ = html_to_markdown(&shape);
                }
            }
        }
    }
}
