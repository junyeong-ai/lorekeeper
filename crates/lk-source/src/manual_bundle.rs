//! Bounded, read-once local dependency capture. No publication occurs in this adapter.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use html5ever::tendril::TendrilSink;
use lk_core::captured_file::CapturedFile;
use lk_core::manual_original::{
    MAX_ORIGINAL_MEMBERS, ManualBundleCapture, ManualBundleManifest, ManualBundleMember,
    ORIGINAL_SCHEME, OriginalCaptureConfig, UnsupportedOriginalConstruct,
};
use markup5ever_rcdom::{NodeData, RcDom};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};

use crate::SourceError;
#[cfg(test)]
const MAX_TEXT_BYTES: usize = lk_core::manual_original::MAX_ORIGINAL_ENTRY_BYTES;
const MAX_DEPTH: usize = 32;
const SERIALIZER_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_FOOTNOTES);
const PARSER_OPTIONS: Options = SERIALIZER_OPTIONS.union(Options::ENABLE_WIKILINKS);

fn error(message: impl std::fmt::Display) -> SourceError {
    SourceError::Parse(format!("manual original: {message}"))
}

/// Refuse symlinks at every component, even where their destination stays inside the inbox.
pub(crate) fn direct_member(root: &Path, relative: &Path) -> Result<PathBuf, SourceError> {
    let mut full = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(error("member must remain inside bundle root"));
        };
        full.push(name);
        let metadata = full
            .symlink_metadata()
            .map_err(|e| error(format!("{}: {e}", full.display())))?;
        if metadata.file_type().is_symlink() {
            return Err(error(format!("symlink member: {}", full.display())));
        }
    }
    if !full.metadata().map_err(error)?.is_file() {
        return Err(error(format!("not a regular member: {}", full.display())));
    }
    Ok(full)
}
#[cfg(test)]
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, SourceError> {
    let mut file = std::fs::File::open(path).map_err(error)?;
    if !file.metadata().map_err(error)?.is_file() {
        return Err(error("not a regular original"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    if bytes.len() > limit {
        return Err(error(format!(
            "original member or remaining dependency-closure budget exceeds {limit} bytes; provide a smaller self-contained export: {}",
            path.display()
        )));
    }
    Ok(bytes)
}
fn text_kind(path: &Path) -> Option<bool> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "html" | "htm" => Some(true),
        "md" | "markdown" | "txt" => Some(false),
        _ => None,
    }
}
/// None is a nonlocal URI or same-document fragment, never a guessed local filename.
fn local_path(
    root: &Path,
    member: &Path,
    dest: &str,
    derived: bool,
) -> Result<Option<PathBuf>, SourceError> {
    if dest.is_empty() || dest.starts_with('#') {
        return Ok(None);
    }
    let bytes = dest.as_bytes();
    if dest.contains('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        return Err(error(
            "Windows drive/UNC local destinations are unsupported",
        ));
    }
    if let Ok(url) = url::Url::parse(dest) {
        if url.scheme() != "file" {
            return Ok(None);
        }
        if !derived {
            return Err(error(
                "absolute file URLs are outside the local bundle contract",
            ));
        }
    } else if dest.starts_with('/') || dest.contains('\\') {
        return Err(error("absolute/Windows local destination is unsupported"));
    }
    let base = url::Url::from_file_path(root.join(member))
        .map_err(|()| error("invalid entry file URL"))?;
    let resolved = base.join(dest).map_err(error)?;
    if resolved.scheme() != "file" {
        return Ok(None);
    }
    if resolved.query().is_some() {
        return Err(error("local file query is unsupported"));
    }
    let full = resolved
        .to_file_path()
        .map_err(|()| error("invalid local file URL"))?;
    let relative = full
        .strip_prefix(root)
        .map_err(|_| error(format!("local reference escapes bundle: {dest}")))?;
    lk_core::manual_original::validate_member_path(
        relative
            .to_str()
            .ok_or_else(|| error("non-UTF-8 original member paths are unsupported"))?,
    )
    .map_err(error)?;
    Ok(Some(relative.to_path_buf()))
}

fn unsupported_url_attribute(tag: &str, key: &str, namespace: &str) -> bool {
    if namespace != "http://www.w3.org/1999/xhtml" && matches!(key, "href" | "src") {
        return true;
    }
    matches!(
        (tag, key),
        ("base" | "area", "href")
            | ("track" | "input" | "embed" | "iframe" | "script", "src")
            | ("object", "data")
            | ("form", "action")
            | ("input" | "button", "formaction")
            | ("blockquote" | "q" | "ins" | "del", "cite")
            | ("img" | "source", "srcset")
            | ("link", "imagesrcset")
            | ("a" | "area", "ping")
            | ("iframe", "srcdoc")
            | ("img" | "iframe", "longdesc")
            | ("img" | "input" | "object", "usemap")
            | ("body" | "table" | "td" | "th", "background")
            | ("html", "manifest")
            | ("head", "profile")
            | ("object" | "applet", "codebase" | "archive" | "classid")
            | ("applet", "code")
            | ("command", "icon")
    )
}

fn html_references(text: &str) -> (Vec<String>, BTreeSet<String>) {
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(text);

    // Keep the DOM owner alive: RcDom Node::drop drains descendants even
    // when a traversal holds another handle to a child.
    let mut stack = vec![dom.document.clone()];
    let mut refs = Vec::new();
    let mut unsupported = BTreeSet::new();
    while let Some(node) = stack.pop() {
        if let NodeData::Element {
            name,
            attrs,
            template_contents,
            ..
        } = &node.data
        {
            if let Some(contents) = template_contents.borrow().as_ref() {
                stack.push(contents.clone());
            }
            let tag = name.local.as_ref();
            if matches!(
                tag,
                "style" | "script" | "base" | "iframe" | "object" | "svg"
            ) {
                unsupported.insert(format!("html-{tag}"));
            }
            for attr in attrs.borrow().iter() {
                let key = attr.name.local.as_ref();
                // A closed table of URL-bearing HTML/foreign-content surfaces. Unsupported
                // syntax is recorded even when it points to a remote URI: preserving HTML
                // bytes is not a claim to have captured those resource semantics.
                if key == "style" || unsupported_url_attribute(tag, key, name.ns.as_ref()) {
                    let construct = if key == "style" {
                        "inline-css".into()
                    } else if name.ns.as_ref() != "http://www.w3.org/1999/xhtml" {
                        "foreign-url".into()
                    } else {
                        format!("html-{tag}-{key}")
                    };
                    unsupported.insert(construct);
                }
                if matches!(
                    (tag, key),
                    ("a", "href")
                        | ("img", "src")
                        | ("link", "href")
                        | ("audio", "src")
                        | ("video", "src")
                        | ("source", "src")
                        | ("video", "poster")
                ) {
                    refs.push(attr.value.to_string());
                }
                if tag == "link"
                    && key == "rel"
                    && attr
                        .value
                        .split_ascii_whitespace()
                        .any(|s| s.eq_ignore_ascii_case("stylesheet"))
                {
                    unsupported.insert("css-dependencies".into());
                }
            }
        }
        stack.extend(node.children.borrow().iter().cloned());
    }
    (refs, unsupported)
}
/// Obsidian's vault-wide basename lookup/implicit .md and embed options are not
/// filesystem identity. Only an explicit relative filename (or same-page anchor) is supported.
fn validate_wikilink(dest: &str, piped: bool, image: bool) -> Result<(), SourceError> {
    if (image && piped)
        || (!dest.starts_with('#')
            && (url::Url::parse(dest).is_ok()
                || Path::new(dest.split('#').next().unwrap_or(dest))
                    .extension()
                    .is_none()))
    {
        return Err(error(
            "Obsidian basename/implicit-extension lookup or embed options are unsupported; use an explicit relative filename/standard Markdown link",
        ));
    }
    Ok(())
}

fn markdown_references(text: &str) -> Result<(Vec<String>, BTreeSet<String>), SourceError> {
    let mut refs = Vec::new();
    let mut issues = BTreeSet::new();
    for event in Parser::new_ext(text, PARSER_OPTIONS) {
        match event {
            Event::Start(tag @ (Tag::Link { .. } | Tag::Image { .. })) => {
                let (link_type, dest_url) = match &tag {
                    Tag::Link {
                        link_type,
                        dest_url,
                        ..
                    }
                    | Tag::Image {
                        link_type,
                        dest_url,
                        ..
                    } => (link_type, dest_url),
                    _ => unreachable!(),
                };
                if let LinkType::WikiLink { has_pothole } = link_type {
                    validate_wikilink(dest_url, *has_pothole, matches!(tag, Tag::Image { .. }))?;
                }
                refs.push(dest_url.to_string())
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let (r, i) = html_references(&html);
                refs.extend(r);
                issues.extend(i);
            }
            _ => {}
        }
    }
    Ok((refs, issues))
}

#[cfg(test)]
pub(crate) fn capture(root: &Path, entry: &Path) -> Result<ManualBundleCapture, SourceError> {
    capture_limited(
        root,
        entry,
        lk_core::manual_original::MAX_MANUAL_BATCH_BYTES,
    )
}
#[cfg(test)]
pub(crate) fn capture_limited(
    root: &Path,
    entry: &Path,
    budget: usize,
) -> Result<ManualBundleCapture, SourceError> {
    capture_with_limits(
        root,
        entry,
        budget,
        &OriginalCaptureConfig::default(),
        MAX_TEXT_BYTES,
    )
}

pub(crate) fn capture_with_limits(
    root: &Path,
    entry: &Path,
    budget: usize,
    limits: &OriginalCaptureConfig,
    text_limit: usize,
) -> Result<ManualBundleCapture, SourceError> {
    let entry = entry.strip_prefix(root).map_err(error)?.to_path_buf();
    lk_core::manual_original::validate_member_path(
        entry
            .to_str()
            .ok_or_else(|| error("non-UTF-8 original entry paths are unsupported"))?,
    )
    .map_err(error)?;
    let mut pending = VecDeque::from([(entry.clone(), 0usize)]);
    let mut scheduled = BTreeSet::from([entry.clone()]);
    let mut members = BTreeMap::new();
    let mut issues = BTreeSet::new();
    let mut total = 0usize;
    while let Some((relative, depth)) = pending.pop_front() {
        if members.contains_key(&relative) {
            continue;
        }
        if depth > MAX_DEPTH || members.len() >= MAX_ORIGINAL_MEMBERS {
            return Err(error("bundle dependency depth/member limit exceeded"));
        }
        let path = direct_member(root, &relative)?;
        let kind = text_kind(&path);
        let input_limit = (if relative == entry {
            limits.max_entry_bytes.get()
        } else {
            limits.max_member_bytes.get()
        })
        .min(budget.saturating_sub(total));
        let bytes = CapturedFile::capture(std::fs::File::open(&path).map_err(error)?, input_limit)
            .map_err(error)?;
        total = total
            .checked_add(bytes.byte_len())
            .ok_or_else(|| error("bundle size overflow"))?;
        if total > budget {
            return Err(error(format!(
                "local dependency closure exceeds the remaining {budget}-byte source budget; splitting entries does not shrink a linked closure"
            )));
        }
        if let Some(html) = kind {
            let raw = bytes.read_bytes(text_limit).map_err(error)?;
            let text = std::str::from_utf8(&raw).map_err(error)?;
            let (refs, unsupported) = if html {
                html_references(text)
            } else {
                let (_, body) = crate::manual::split_frontmatter(text, &path);
                markdown_references(&body)?
            };
            if unsupported.contains("html-base") {
                return Err(error("HTML base is unsupported; retaining this entry"));
            }
            for construct in unsupported {
                issues.insert(UnsupportedOriginalConstruct {
                    member: relative.to_string_lossy().into(),
                    construct,
                });
            }
            for dest in refs {
                if let Some(next) = local_path(root, &relative, &dest, false)?
                    && scheduled.insert(next.clone())
                {
                    if scheduled.len() > MAX_ORIGINAL_MEMBERS {
                        return Err(error("bundle dependency member limit exceeded"));
                    }
                    pending.push_back((next, depth + 1));
                }
            }
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("css") || e.eq_ignore_ascii_case("svg"))
        {
            issues.insert(UnsupportedOriginalConstruct {
                member: relative.to_string_lossy().into(),
                construct: "opaque-css-svg-dependencies".into(),
            });
        }
        members.insert(relative, bytes);
    }
    let manifest = ManualBundleManifest {
        schema: ORIGINAL_SCHEME.into(),
        entry: entry.to_string_lossy().into(),
        members: members
            .iter()
            .map(|(p, b)| ManualBundleMember {
                path: p.to_string_lossy().into(),
                bytes: b.byte_len(),
                revision: b.revision().clone(),
            })
            .collect(),
        unsupported: issues.into_iter().collect(),
    };
    let capture = ManualBundleCapture {
        source_file: root.join(entry).display().to_string(),
        manifest,
        contents: members.into_values().collect(),
    };
    capture.validate().map_err(error)?;
    Ok(capture)
}

/// The proven event serializer handles CommonMark escapes/references; no destination grammar
/// is reimplemented. Only a derived view is normalized, and only when a link changes.
pub(crate) fn project_links(
    text: &str,
    root: &Path,
    entry: &Path,
    capture: &ManualBundleCapture,
    wiki_documents: &Path,
    max_output_bytes: usize,
) -> Result<String, SourceError> {
    let reference = capture.manifest.reference().map_err(error)?;
    let entry = entry.strip_prefix(root).map_err(error)?;
    let known: BTreeSet<_> = capture
        .manifest
        .members
        .iter()
        .map(|m| m.path.as_str())
        .collect();
    let mut changed = false;
    let mut events = Vec::new();
    for event in Parser::new_ext(text, PARSER_OPTIONS) {
        let event = match event {
            Event::Html(ref html) | Event::InlineHtml(ref html) => {
                // Raw HTML in Markdown has no faithful local-attribute rewrite through
                // Markdown Link/Image events. Refuse it rather than render a wrong wiki base.
                let (refs, _) = html_references(html);
                for dest in refs {
                    if local_path(root, entry, &dest, false)?.is_some() {
                        return Err(error(
                            "local-resource raw HTML inside Markdown is unsupported; retaining entry",
                        ));
                    }
                }
                event
            }
            Event::Start(mut tag @ (Tag::Link { .. } | Tag::Image { .. })) => {
                let image = matches!(tag, Tag::Image { .. });
                match &mut tag {
                    Tag::Link {
                        link_type,
                        dest_url,
                        ..
                    }
                    | Tag::Image {
                        link_type,
                        dest_url,
                        ..
                    } => {
                        if let LinkType::WikiLink { has_pothole } = *link_type {
                            validate_wikilink(dest_url, has_pothole, image)?;
                            *link_type = LinkType::Inline;
                            changed = true;
                        }
                    }
                    _ => unreachable!(),
                }
                let dest = match &mut tag {
                    Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => dest_url,
                    _ => unreachable!(),
                };
                if let Some(member) = local_path(root, entry, dest, true)? {
                    let member = member.to_string_lossy();
                    if !known.contains(member.as_ref()) {
                        return Err(error(format!(
                            "derived local link is not captured: {member}"
                        )));
                    }
                    let target = PathBuf::from(
                        reference
                            .manifest_path
                            .rsplit_once('/')
                            .ok_or_else(|| error("manifest path"))?
                            .0,
                    )
                    .join("files")
                    .join(member.as_ref());
                    let source = wiki_documents.join("document.md");
                    let mut rewritten = lk_core::link::relative_dest(&source, &target);
                    if let Some(fragment) = dest.split_once('#').map(|(_, s)| s) {
                        rewritten.push('#');
                        rewritten.push_str(fragment);
                    }
                    *dest = rewritten.into();
                    changed = true;
                }
                Event::Start(tag)
            }
            other => other,
        };
        events.push(event);
    }
    if !changed {
        return Ok(text.into());
    }
    let mut out = String::new();
    pulldown_cmark_to_cmark::cmark(events, &mut out, SERIALIZER_OPTIONS).map_err(error)?;
    if out.len() > max_output_bytes {
        return Err(error(
            "derived Markdown exceeds the configured projection limit",
        ));
    }
    Ok(out)
}

pub(crate) fn unchanged(root: &Path, capture: &ManualBundleCapture) -> bool {
    capture.manifest.members.iter().all(|member| {
        direct_member(root, Path::new(&member.path))
            .and_then(|path| {
                let file = std::fs::File::open(path).map_err(error)?;
                if file.metadata().map_err(error)?.len() != member.bytes as u64 {
                    return Err(error("original size changed"));
                }
                lk_core::captured_file::hash_reader(file.take(member.bytes as u64 + 1))
                    .map_err(error)
            })
            .is_ok_and(|(bytes, revision)| bytes == member.bytes && revision == member.revision)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn larger_binary_capture_is_explicit_file_backed_and_retirement_checks_original_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.pdf");
        let bytes = MAX_TEXT_BYTES + 1;
        std::fs::File::create(&path)
            .unwrap()
            .set_len(bytes as u64)
            .unwrap();
        assert!(capture(root.path(), &path).is_err());
        assert!(path.exists());
        let limits = OriginalCaptureConfig {
            max_entry_bytes: std::num::NonZeroUsize::new(bytes).unwrap(),
            ..Default::default()
        };
        limits.validate().unwrap();
        let captured = capture_with_limits(
            root.path(),
            &path,
            limits.max_source_bytes.get(),
            &limits,
            MAX_TEXT_BYTES,
        )
        .unwrap();
        assert_eq!(captured.byte_len(), bytes);
        assert_eq!(captured.manifest.schema, ORIGINAL_SCHEME);
        let scratch = captured.contents[0].path().to_owned();
        assert!(scratch.exists());
        assert!(unchanged(root.path(), &captured));
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"changed").unwrap();
        assert!(!unchanged(root.path(), &captured));
        captured.validate().unwrap();
        drop(captured);
        assert!(!scratch.exists());
        assert!(path.exists());
        assert!(!root.path().join(".lorekeeper").exists());
    }
    fn fixture() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        (tmp, root)
    }
    #[test]
    fn captured_assets_change_revision_and_reference_images_rewrite_without_a_new_grammar() {
        let (_tmp, root) = fixture();
        std::fs::create_dir(root.join("images")).unwrap();
        std::fs::write(root.join("images/a (1).png"), b"old").unwrap();
        let source = "# Title\n\n![alt][chart]\n\n[chart]: <images/a (1).png> \"caption\"\n\n`[fake](missing.png)`\n\n```md\n![fake](missing.png)\n```\n";
        std::fs::write(root.join("note.md"), source).unwrap();
        let a = capture(&root, &root.join("note.md")).unwrap();
        assert_eq!(a.manifest.members.len(), 2);
        let projected = project_links(
            source,
            &root,
            &root.join("note.md"),
            &a,
            Path::new("knowledge/docs"),
            MAX_TEXT_BYTES,
        )
        .unwrap();
        let destinations: Vec<_> = Parser::new(&projected)
            .filter_map(|e| match e {
                Event::Start(Tag::Image { dest_url, .. }) => Some(dest_url.into_string()),
                _ => None,
            })
            .collect();
        assert_eq!(destinations.len(), 1);
        assert!(destinations[0].starts_with("../../.lorekeeper/evidence/manual/"));
        assert!(destinations[0].ends_with("/files/images/a%20%281%29.png"));
        assert!(projected.contains("missing.png"));
        assert_eq!(
            a.contents[a
                .manifest
                .members
                .iter()
                .position(|m| m.path == "note.md")
                .unwrap()]
            .read_bytes(MAX_TEXT_BYTES)
            .unwrap(),
            source.as_bytes()
        );
        std::fs::write(root.join("images/a (1).png"), b"new").unwrap();
        let b = capture(&root, &root.join("note.md")).unwrap();
        assert_ne!(
            a.manifest.revision().unwrap(),
            b.manifest.revision().unwrap()
        );
        assert!(!unchanged(&root, &a));
    }
    #[test]
    fn capture_is_read_once_projection_uses_the_captured_image_inventory() {
        let (_tmp, root) = fixture();
        std::fs::write(root.join("a.png"), b"old").unwrap();
        std::fs::write(root.join("n.md"), "![x](a.png)").unwrap();
        let a = capture(&root, &root.join("n.md")).unwrap();
        std::fs::remove_file(root.join("a.png")).unwrap();
        assert!(
            project_links(
                "![x](a.png)",
                &root,
                &root.join("n.md"),
                &a,
                Path::new("wiki/documents"),
                MAX_TEXT_BYTES
            )
            .is_ok()
        );
        assert!(!unchanged(&root, &a));
    }
    #[test]
    fn missing_escape_and_base_refuse_instead_of_publishing_a_false_closure() {
        let (_tmp, root) = fixture();
        for text in [
            "![x](missing.png)",
            "[x](../escape.md)",
            "[x](%2e%2e/escape.md)",
            "<base href='https://example.org/'><p><img src='x.png'></p>",
        ] {
            let ext = if text.starts_with("<base") {
                "html"
            } else {
                "md"
            };
            let path = root.join(format!("note.{ext}"));
            std::fs::write(&path, text).unwrap();
            assert!(capture(&root, &path).is_err(), "{text}");
        }
    }
    #[test]
    fn css_svg_and_raw_html_are_honest_partial_coverage_not_retirement_proofs() {
        let (_tmp, root) = fixture();
        std::fs::write(root.join("style.css"), "@import 'uncaptured.css'").unwrap();
        std::fs::write(
            root.join("shape.svg"),
            "<svg><image href='uncaptured.png'/></svg>",
        )
        .unwrap();
        std::fs::write(
            root.join("n.html"),
            "<link rel='stylesheet' href='style.css'><img src='shape.svg'><p>body</p>",
        )
        .unwrap();
        let a = capture(&root, &root.join("n.html")).unwrap();
        eprintln!(
            "refs={:?} manifest={:?}",
            html_references(
                "<link rel='stylesheet' href='style.css'><img src='shape.svg'><p>body</p>"
            ),
            a.manifest
        );
        assert_eq!(a.manifest.members.len(), 3);
        assert!(
            a.manifest
                .unsupported
                .iter()
                .any(|i| i.member == "shape.svg")
        );
        assert!(
            a.manifest
                .unsupported
                .iter()
                .any(|i| i.member == "style.css")
        );
    }
    #[cfg(unix)]
    #[test]
    fn directory_and_leaf_symlinks_are_refused_even_when_they_point_inside() {
        use std::os::unix::fs::symlink;
        let (_tmp, root) = fixture();
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/a.png"), b"a").unwrap();
        symlink(root.join("assets"), root.join("alias")).unwrap();
        symlink(root.join("assets/a.png"), root.join("leaf.png")).unwrap();
        for link in ["alias/a.png", "leaf.png"] {
            std::fs::write(root.join("n.md"), format!("![x]({link})")).unwrap();
            assert!(capture(&root, &root.join("n.md")).is_err());
        }
    }
    #[test]
    fn windows_drive_unc_and_raw_html_local_urls_are_not_external_uri_fallbacks() {
        let (_tmp, root) = fixture();
        for dest in [
            "C:/outside.png",
            "C:\\outside.png",
            "C:outside.png",
            "\\\\host\\share\\a.png",
        ] {
            assert!(
                local_path(&root, Path::new("n.md"), dest, false).is_err(),
                "{dest}"
            );
        }
        std::fs::write(root.join("a.png"), b"image").unwrap();
        std::fs::write(root.join("n.md"), "<img src='a.png'>").unwrap();
        let captured = capture(&root, &root.join("n.md")).unwrap();
        assert!(
            project_links(
                "<img src='a.png'>",
                &root,
                &root.join("n.md"),
                &captured,
                Path::new("wiki/documents"),
                MAX_TEXT_BYTES
            )
            .is_err()
        );
        for html in [
            "<track src='cue.vtt'>",
            "<input src='image.png'>",
            "<embed src='x.pdf'>",
            "<svg><image href='x.png'/></svg>",
        ] {
            let (_, issues) = html_references(html);
            assert!(!issues.is_empty(), "{html}");
        }
    }
    #[test]
    fn bounded_reader_refuses_instead_of_truncating() {
        let (_tmp, root) = fixture();
        let path = root.join("small");
        std::fs::write(&path, b"12345").unwrap();
        assert!(read_bounded(&path, 4).is_err());
        assert_eq!(read_bounded(&path, 5).unwrap(), b"12345");
    }
}

#[cfg(test)]
mod wikilink_tests {
    use super::*;
    #[test]
    fn parser_observes_explicit_obsidian_embeds_and_rewrites_them_as_standard_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("figure.png"), [0, 255]).unwrap();
        std::fs::write(root.join("other.md"), "Other").unwrap();
        let text = "![[figure.png]] and [[other.md#section|label]]\n\n`![[missing.png]]`";
        std::fs::write(root.join("note.md"), text).unwrap();
        let captured = capture(&root, &root.join("note.md")).unwrap();
        assert_eq!(captured.manifest.members.len(), 3);
        let output = project_links(
            text,
            &root,
            &root.join("note.md"),
            &captured,
            Path::new("wiki/documents"),
            MAX_TEXT_BYTES,
        )
        .unwrap();
        let mut found = 0;
        for event in Parser::new(&output) {
            if let Event::Start(
                Tag::Link {
                    link_type,
                    dest_url,
                    ..
                }
                | Tag::Image {
                    link_type,
                    dest_url,
                    ..
                },
            ) = event
            {
                assert!(!matches!(link_type, LinkType::WikiLink { .. }));
                assert!(dest_url.contains(".lorekeeper/evidence/manual/"));
                found += 1;
            }
        }
        assert_eq!(found, 2);
        assert!(output.contains("#section"));
        assert!(output.contains("`![[missing.png]]`"));
    }
    #[test]
    fn implicit_obsidian_lookup_and_embed_size_options_refuse_instead_of_retiring() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("note.md"), "body").unwrap();
        std::fs::write(root.join("figure.png"), b"image").unwrap();
        for text in ["[[note]]", "![[figure.png|300]]", "[[missing.md]]"] {
            std::fs::write(root.join("entry.md"), text).unwrap();
            assert!(capture(&root, &root.join("entry.md")).is_err(), "{text}");
        }
    }
    #[test]
    fn harmless_html_comments_do_not_invent_unresolved_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("n.md"), "<!-- note -->\n<div>prose only</div>").unwrap();
        let original = capture(&root, &root.join("n.md")).unwrap();
        assert!(original.manifest.unsupported.is_empty());
        assert_eq!(original.manifest.members.len(), 1);
    }
    #[test]
    fn remaining_batch_budget_is_applied_before_reading_the_next_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("n.md"), "12345").unwrap();
        assert!(capture_limited(&root, &root.join("n.md"), 4).is_err());
        assert_eq!(
            capture_limited(&root, &root.join("n.md"), 5)
                .unwrap()
                .byte_len(),
            5
        );
    }
}
