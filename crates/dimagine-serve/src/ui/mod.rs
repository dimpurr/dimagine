//! UI rendering helpers and components.

pub mod components;
pub mod shell;

use ammonia::Builder;
use pulldown_cmark::{html, Event, Options, Parser};

use crate::assets::css_url;

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

pub fn markdown_html(markdown: &str) -> String {
    let mut out = String::new();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let safe_events = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    html::push_html(&mut out, safe_events);
    let mut sanitizer = Builder::default();
    sanitizer.url_schemes(["http", "https", "mailto"].into_iter().collect());
    sanitizer.clean(&out).to_string()
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
