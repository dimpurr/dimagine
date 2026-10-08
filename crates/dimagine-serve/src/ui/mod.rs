//! UI rendering helpers and components.

pub mod components;
pub mod filters;
pub mod shell;

use ammonia::Builder;
use pulldown_cmark::{html, CowStr, Event, Options, Parser};

use crate::assets::css_url;
use crate::view_query::query_value;

/// One HTML document for a page outside the library frame: the
/// sign-in and setup pages (spec §4b). The same stylesheet, type
/// and light/dark themes as the library, in one centred card —
/// no sidebar and no tab bar, because nobody is signed in yet.
pub fn layout(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{} · dimagine</title><link rel=\"stylesheet\" href=\"{}\"></head>\
         <body><div class=\"auth-page\"><main class=\"auth-card\">{}</main></div></body></html>",
        escape_html(title),
        escape_html(&css_url()),
        body
    )
}

/// What a wikilink or embed in a note resolved to (FORMAT §5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkOutcome {
    /// A library image, by library-relative path.
    Image(String),
    /// A library note, by library-relative path.
    Note(String),
    /// The image the note belongs to (FORMAT §3.2), by
    /// library-relative path. The image page already shows
    /// it, so an embed of it is not repeated.
    SelfImage(String),
    /// Nothing resolved, or the match was ambiguous (FORMAT §5.1
    /// rule 3): the link stays plain text.
    Unresolved,
}

/// Resolution of the links of one note body (FORMAT §5.1).
///
/// The renderer asks about one target at a time, as written in the
/// note; the implementation knows which note is being rendered.
pub trait LinkResolver {
    /// Resolve a link target as written in the note being rendered.
    fn resolve_link(&self, target: &str) -> LinkOutcome;
}

pub fn markdown_html(markdown: &str, links: Option<&dyn LinkResolver>) -> String {
    let mut out = String::new();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(markdown, options);
    // Raw HTML in the source is shown as text, never executed. The
    // only HTML that reaches the output is what this function writes
    // for resolved links, and ammonia sanitises that too.
    let safe_events: Vec<Event> = parser
        .map(|event| match event {
            Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
            other => other,
        })
        .collect();
    let linked_events = merge_text_events(safe_events, links);
    html::push_html(&mut out, linked_events.into_iter());
    let mut sanitizer = Builder::default();
    sanitizer.url_schemes(["http", "https", "mailto"].into_iter().collect());
    // The one class this renderer writes: a link that
    // resolved to nothing (FORMAT §5.1 rule 3).
    sanitizer.add_allowed_classes("span", &["missing-link"]);
    sanitizer.clean(&out).to_string()
}

/// Merge runs of consecutive text events, then split the
/// wikilinks and embeds out of each merged run.
///
/// The parser fragments text at every `[` and `]` while it
/// looks for links, so the characters of one wikilink arrive
/// as a run of small text events. Merging consecutive text
/// events is invisible in the HTML output and puts the
/// wikilink back together; code spans and fenced blocks stay
/// separate events, so their contents are never merged.
fn merge_text_events<'a>(
    events: Vec<Event<'a>>,
    links: Option<&dyn LinkResolver>,
) -> Vec<Event<'a>> {
    let mut out = Vec::new();
    let mut text = String::new();
    for event in events {
        match event {
            Event::Text(fragment) => {
                text.push_str(fragment.as_ref());
            }
            other => {
                if !text.is_empty() {
                    out.extend(wikilink_events(&text, links));
                    text.clear();
                }
                out.push(other);
            }
        }
    }
    if !text.is_empty() {
        out.extend(wikilink_events(&text, links));
    }
    out
}

/// Split one text event around the wikilinks and embeds it holds
/// (FORMAT §5.1), so each becomes a link, an image or plain text.
///
/// Code spans and fenced blocks never reach this function: the
/// parser reports their contents as `Code` events, not `Text`.
fn wikilink_events<'a>(text: &str, links: Option<&dyn LinkResolver>) -> Vec<Event<'a>> {
    let Some(links) = links else {
        return vec![Event::Text(CowStr::from(text.to_owned()))];
    };
    let mut events = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some((start, embed)) = next_opener(rest) {
        let open_end = start + if embed { 3 } else { 2 };
        match rest[open_end..].find("]]") {
            Some(close) => {
                let close = open_end + close;
                plain.push_str(&rest[..start]);
                if !plain.is_empty() {
                    events.push(Event::Text(CowStr::from(std::mem::take(&mut plain))));
                }
                events.push(link_event(embed, &rest[open_end..close], links));
                rest = &rest[close + 2..];
            }
            None => {
                // An opener with no closing brackets is plain text;
                // a later, closed link in the same text still renders.
                plain.push_str(&rest[..open_end]);
                rest = &rest[open_end..];
            }
        }
    }
    plain.push_str(rest);
    if !plain.is_empty() {
        events.push(Event::Text(CowStr::from(plain)));
    }
    events
}

/// The next `![[` or `[[` in `text`, as `(offset, is_embed)`.
fn next_opener(text: &str) -> Option<(usize, bool)> {
    let embed = text.find("![[");
    let plain = text.find("[[");
    match (embed, plain) {
        (Some(embed), Some(plain)) if embed < plain => Some((embed, true)),
        (Some(_), Some(plain)) => Some((plain, false)),
        (Some(embed), None) => Some((embed, true)),
        (None, Some(plain)) => Some((plain, false)),
        (None, None) => None,
    }
}

/// One wikilink or embed, rendered as an HTML event.
fn link_event<'a>(embed: bool, content: &str, links: &dyn LinkResolver) -> Event<'a> {
    // `[[target|label]]` and `[[target#heading]]`: only the file
    // reference resolves (FORMAT §2.1 keeps `|` and `#` out of names).
    let (target, alias) = content
        .split_once('|')
        .map(|(target, label)| (target, Some(label)))
        .unwrap_or((content, None));
    let file = target.split('#').next().unwrap_or("").trim();
    let label = alias
        .map(str::to_owned)
        .unwrap_or_else(|| file.rsplit('/').next().unwrap_or(file).to_owned());
    match links.resolve_link(file) {
        // FORMAT §3.2: the page already shows this image.
        LinkOutcome::SelfImage(_) if embed => Event::Text(CowStr::from(String::new())),
        LinkOutcome::Image(path) | LinkOutcome::SelfImage(path) => {
            let href = format!("/image/{}", encode_path(&path));
            if embed {
                Event::Html(CowStr::from(format!(
                    "<a href=\"{href}\"><img src=\"/media/{}\" alt=\"{}\"></a>",
                    encode_path(&path),
                    escape_html(&label)
                )))
            } else {
                Event::Html(CowStr::from(format!(
                    "<a href=\"{href}\">{}</a>",
                    escape_html(&label)
                )))
            }
        }
        LinkOutcome::Note(path) => Event::Html(CowStr::from(format!(
            "<a href=\"/?c={}\">{}</a>",
            query_value(&path),
            escape_html(&label)
        ))),
        LinkOutcome::Unresolved => {
            let raw = if embed {
                format!("![[{content}]]")
            } else {
                format!("[[{content}]]")
            };
            Event::Html(CowStr::from(format!(
                "<span class=\"missing-link\">{}</span>",
                escape_html(&raw)
            )))
        }
    }
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out = vec![("Library".to_string(), String::new())];
    let mut acc = String::new();
    for part in path.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        out.push((part.to_string(), acc.clone()));
    }
    out
}

pub fn route_tail(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or("").to_string()
}

pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|part| {
            part.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The fragment that names one image's tile: the `id` every tile carries,
/// and the anchor the image page's "Back to view" returns to, so the grid
/// comes back where it was — without the script, and without the browser
/// having to remember anything (K27 motion 4).
pub fn tile_anchor(path: &str) -> String {
    format!("img-{}", encode_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A resolver with a fixed answer per target, for testing
    /// the renderer in isolation from the library walk.
    struct FixedResolver {
        answers: HashMap<String, LinkOutcome>,
    }

    impl FixedResolver {
        fn new(answers: &[(&str, LinkOutcome)]) -> Self {
            Self {
                answers: answers
                    .iter()
                    .map(|(target, outcome)| ((*target).to_owned(), outcome.clone()))
                    .collect(),
            }
        }
    }

    impl LinkResolver for FixedResolver {
        fn resolve_link(&self, target: &str) -> LinkOutcome {
            self.answers
                .get(target)
                .cloned()
                .unwrap_or(LinkOutcome::Unresolved)
        }
    }

    fn image(path: &str) -> LinkOutcome {
        LinkOutcome::Image(path.to_owned())
    }

    fn note(path: &str) -> LinkOutcome {
        LinkOutcome::Note(path.to_owned())
    }

    #[test]
    fn a_wikilink_to_an_image_links_to_its_page() {
        let resolver = FixedResolver::new(&[("refs/landscape.png", image("refs/landscape.png"))]);
        let html = markdown_html("See [[refs/landscape.png]].", Some(&resolver));
        assert!(
            html.contains(
                "<a href=\"/image/refs/landscape.png\" rel=\"noopener noreferrer\">landscape.png</a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn a_wikilink_to_a_note_links_to_the_note_view() {
        let resolver = FixedResolver::new(&[("browse.md", note("browse.md"))]);
        let html = markdown_html("See [[browse.md]].", Some(&resolver));
        assert!(
            html.contains("<a href=\"/?c=browse.md\" rel=\"noopener noreferrer\">browse.md</a>"),
            "{html}"
        );
    }

    #[test]
    fn an_aliased_wikilink_uses_the_alias_as_the_label() {
        let resolver = FixedResolver::new(&[("refs/landscape.png", image("refs/landscape.png"))]);
        let html = markdown_html("See [[refs/landscape.png|the wide one]].", Some(&resolver));
        assert!(
            html.contains(
                "<a href=\"/image/refs/landscape.png\" rel=\"noopener noreferrer\">the wide one</a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn a_wikilink_without_a_label_shows_the_file_name() {
        let resolver =
            FixedResolver::new(&[("refs/deep-sea/girl.jpg", image("refs/deep-sea/girl.jpg"))]);
        let html = markdown_html("See [[refs/deep-sea/girl.jpg]].", Some(&resolver));
        assert!(
            html.contains(
                "<a href=\"/image/refs/deep-sea/girl.jpg\" rel=\"noopener noreferrer\">girl.jpg</a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn a_wikilink_that_resolves_to_nothing_stays_plain_text() {
        let resolver = FixedResolver::new(&[]);
        let html = markdown_html("See [[missing.png]].", Some(&resolver));
        assert!(
            html.contains("<span class=\"missing-link\">[[missing.png]]</span>"),
            "{html}"
        );
        assert!(!html.contains("<a href"), "{html}");
    }

    #[test]
    fn an_embed_of_another_image_shows_its_view_rendition() {
        let resolver = FixedResolver::new(&[("refs/other.png", image("refs/other.png"))]);
        let html = markdown_html("![[refs/other.png]]", Some(&resolver));
        assert!(
            html.contains(
                "<a href=\"/image/refs/other.png\" rel=\"noopener noreferrer\"><img src=\"/media/refs/other.png\" alt=\"other.png\"></a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn the_self_embed_of_a_note_is_not_repeated() {
        let resolver = FixedResolver::new(&[(
            "landscape.png",
            LinkOutcome::SelfImage("refs/landscape.png".to_owned()),
        )]);
        let html = markdown_html(
            "A wide **landscape**.\n![[landscape.png]]\n",
            Some(&resolver),
        );
        assert!(html.contains("<strong>landscape</strong>"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("landscape.png</a>"), "{html}");
    }

    #[test]
    fn a_wikilink_to_the_own_image_still_links_to_its_page() {
        let resolver = FixedResolver::new(&[(
            "landscape.png",
            LinkOutcome::SelfImage("refs/landscape.png".to_owned()),
        )]);
        let html = markdown_html("See [[landscape.png]].", Some(&resolver));
        assert!(
            html.contains(
                "<a href=\"/image/refs/landscape.png\" rel=\"noopener noreferrer\">landscape.png</a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn paths_with_spaces_and_unicode_are_encoded() {
        let resolver = FixedResolver::new(&[
            ("refs/my photo.png", image("refs/my photo.png")),
            ("refs/猫.png", image("refs/猫.png")),
        ]);
        let html = markdown_html(
            "![[refs/my photo.png]] and [[refs/猫.png]]",
            Some(&resolver),
        );
        assert!(
            html.contains("<img src=\"/media/refs/my%20photo.png\""),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"/image/refs/%E7%8C%AB.png\" rel=\"noopener noreferrer\">"),
            "{html}"
        );
    }

    #[test]
    fn a_target_with_brackets_closes_at_the_first_double_bracket() {
        let resolver = FixedResolver::new(&[("a[b].png", image("a[b].png"))]);
        let html = markdown_html("[[a[b].png]]", Some(&resolver));
        assert!(
            html.contains(
                "<a href=\"/image/a%5Bb%5D.png\" rel=\"noopener noreferrer\">a[b].png</a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn markup_in_link_text_and_targets_is_escaped() {
        // A label never becomes markup, and a target that
        // resolves to nothing keeps its raw spelling, escaped.
        let resolver = FixedResolver::new(&[("evil.png", image("evil.png"))]);
        let html = markdown_html(
            "[[evil.png|<script>alert(1)</script>]] and [[<script>alert(2)</script>.png]]",
            Some(&resolver),
        );
        assert!(!html.contains("<script>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(
            html.contains(
                "<span class=\"missing-link\">[[&lt;script&gt;alert(2)&lt;/script&gt;.png]]</span>"
            ),
            "{html}"
        );
    }

    #[test]
    fn a_scheme_target_is_not_a_link() {
        let resolver = FixedResolver::new(&[]);
        let html = markdown_html("[[javascript:alert(1)]]", Some(&resolver));
        assert!(
            html.contains("<span class=\"missing-link\">[[javascript:alert(1)]]</span>"),
            "{html}"
        );
    }

    #[test]
    fn wikilinks_in_code_spans_are_not_rendered() {
        let resolver = FixedResolver::new(&[("refs/landscape.png", image("refs/landscape.png"))]);
        let html = markdown_html("`[[refs/landscape.png]]`", Some(&resolver));
        assert!(
            html.contains("<code>[[refs/landscape.png]]</code>"),
            "{html}"
        );
        assert!(!html.contains("<a href"), "{html}");
    }

    #[test]
    fn an_opener_closes_at_the_first_double_bracket() {
        // The first `]]` closes the first `[[`, so the
        // whole run is one target that resolves to nothing.
        let resolver = FixedResolver::new(&[("refs/landscape.png", image("refs/landscape.png"))]);
        let html = markdown_html("[[unclosed and [[refs/landscape.png]]", Some(&resolver));
        assert!(
            html.contains(
                "<span class=\"missing-link\">[[unclosed and [[refs/landscape.png]]</span>"
            ),
            "{html}"
        );
        assert!(!html.contains("<a href"), "{html}");
    }

    #[test]
    fn markdown_without_a_resolver_renders_as_before() {
        let html = markdown_html("A **quiet** cat.\n<script>alert(1)</script>\n", None);
        assert!(html.contains("A <strong>quiet</strong> cat"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn a_heading_part_does_not_reach_the_resolver() {
        let resolver = FixedResolver::new(&[("refs/landscape.png", image("refs/landscape.png"))]);
        let html = markdown_html("See [[refs/landscape.png#the-shot]].", Some(&resolver));
        assert!(
            html.contains(
                "<a href=\"/image/refs/landscape.png\" rel=\"noopener noreferrer\">landscape.png</a>"
            ),
            "{html}"
        );
    }
}
