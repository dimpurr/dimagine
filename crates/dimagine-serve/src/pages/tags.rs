//! Indexed tag hierarchy and batch management controls.

use std::collections::BTreeMap;

use crate::index_sync::SidebarData;
use crate::ui::{
    escape_html,
    shell::{Destination, Frame},
};
use crate::view_query::query_value;
use crate::{acquire_admission, admission_denied, index_failure, AppState};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{Html, IntoResponse, Response},
};

#[derive(Default)]
struct Node {
    count: Option<u64>,
    children: BTreeMap<String, Node>,
}

#[derive(Default, serde::Deserialize)]
pub(crate) struct TagsQuery {
    q: Option<String>,
    review: Option<String>,
    select: Option<String>,
}

pub(crate) async fn tags_page(
    State(state): State<AppState>,
    Query(query): Query<TagsQuery>,
    headers: HeaderMap,
) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(1),
    };
    let root = state.index.root().to_path_buf();
    let blockers = tokio::task::spawn_blocking(move || {
        let op = dimagine_core::tag_batch::TagOperation::Remove {
            source: "dimagine-blocker-probe".into(),
        };
        dimagine_core::tag_batch::plan_tag_batch(&root, &[op], None)
            .map(|plan| render_blockers(&root, &plan.unreadable))
    })
    .await;
    let blockers = match blockers {
        Ok(Ok(html)) => html,
        Ok(Err(error)) => {
            eprintln!("Could not inspect batch blockers: {error}");
            "<p>Could not inspect batch blockers.</p>".to_owned()
        }
        Err(_) => "<p>Could not inspect batch blockers.</p>".to_owned(),
    };
    let sidebar = match state.index.sidebar_data() {
        Ok(sidebar) => sidebar,
        Err(_) if !blockers.is_empty() => {
            let content = format!("<h1>Tags</h1><p>The tag index is unavailable.</p>{blockers}");
            let frame = Frame {
                banner: state.banner(),
                ..Frame::new("Tags", Destination::Search, content)
            };
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Html(frame.render()),
            )
                .into_response();
        }
        Err(_) => return index_failure(&state, false),
    };
    let filter = query.q.unwrap_or_default();
    let csrf = crate::write_api::write_controls_for(&state, &headers);
    let progress = state.tag_progress.lock().unwrap().clone();
    let progress_html = crate::write_api::tag_progress_html(&progress, csrf.as_deref());
    let frame = Frame {
        banner: state.banner(),
        sidebar: Some(&sidebar),
        ..Frame::new(
            "Tags",
            Destination::Search,
            format!(
                "<div id=\"tag-server-status\">{}</div>{}",
                progress_html,
                body(
                    &sidebar,
                    &filter,
                    query.review.as_deref(),
                    csrf.as_deref(),
                    query.select.as_deref(),
                    &blockers,
                )
            ),
        )
    };
    let mut response = Html(frame.render()).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn body(
    sidebar: &SidebarData,
    filter: &str,
    review: Option<&str>,
    csrf: Option<&str>,
    selected: Option<&str>,
    blockers: &str,
) -> String {
    let review = review == Some("1");
    let mut root = Node::default();
    for tag in &sidebar.tags {
        if !tag.tag.to_lowercase().contains(&filter.to_lowercase())
            || (review && !needs_review(tag, &sidebar.tags))
        {
            continue;
        }
        let mut node = &mut root;
        for segment in tag.tag.split('/') {
            node = node.children.entry(segment.to_owned()).or_default();
        }
        node.count = Some(tag.count);
    }
    let mut out = format!("<section class=\"tags-overview\"><h1>Tags <span class=\"sidebar-row-count\">{}</span></h1><form method=\"get\" action=\"/tags\" class=\"search-pill-container\"><label class=\"sr-only\" for=\"tags-query\">Search tags</label><input id=\"tags-query\" type=\"search\" name=\"q\" value=\"{}\" placeholder=\"Search tags\"><button type=\"submit\">Search</button></form>", sidebar.tags.len(), escape_html(filter));
    out.push_str(blockers);
    out.push_str(&format!("<nav aria-label=\"Tag views\"><a href=\"/tags?q={}\"{}>All</a> · <a href=\"/tags?review=1&amp;q={}\"{}>Needs review</a></nav><p class=\"tags-review-note\">Needs review includes invalid tag names, tags with one or two images, and matching leaf names in different paths.</p>", escape_html(&query_value(filter)), if !review { " aria-current=\"page\"" } else { "" }, escape_html(&query_value(filter)), if review { " aria-current=\"page\"" } else { "" }));
    if root.children.is_empty() {
        out.push_str("<p>No tags match.</p>");
    }
    if let Some(csrf) = csrf {
        out.push_str(&format!(r#"<div id="tag-management" data-csrf="{}"><div class="tag-actions" aria-live="polite"><span id="tag-selection-count">0 selected</span><button type="button" data-tag-action="merge">Merge</button><button type="button" data-tag-action="move">Move</button><button type="button" class="tag-desktop-action" data-tag-action="rename">Rename</button><button type="button" class="tag-desktop-action" data-tag-action="remove">Remove</button><details class="tag-more"><summary>More</summary><div><button type="button" data-tag-action="rename">Rename</button><button type="button" data-tag-action="remove">Remove</button></div></details></div><div id="tag-result" role="status"></div><details class="tag-proposals"><summary>Review a proposal list</summary><p>Paste a version 1 tag list JSON to review its grouped actions.</p><label for="tag-proposal-input">Proposal JSON</label><textarea id="tag-proposal-input" rows="5"></textarea><button type="button" id="tag-proposal-load">Review list</button><div id="tag-proposal-rows"></div><button type="button" id="tag-proposal-preview" hidden>Preview selected proposals</button></details><dialog id="tag-dialog" aria-labelledby="tag-dialog-title"><form id="tag-operation"><h2 id="tag-dialog-title">Manage tags</h2><p id="tag-sources"></p><label id="tag-target-label">Target <input id="tag-target" autocomplete="off"></label><label id="tag-alias-label"><input type="checkbox" id="tag-alias"> Remember old names as aliases</label><div id="tag-preview" aria-live="polite"></div><div class="tag-dialog-actions"><button type="button" id="tag-cancel">Cancel</button><button type="submit" id="tag-plan">Preview</button><button type="button" id="tag-apply" hidden>Apply changes</button></div></form></dialog></div><noscript><form method="post" action="/api/write/tags/form"><h2>Manage a tag</h2><input type="hidden" name="csrf" value="{}"><label>Action <select name="action"><option value="rename">Rename</option><option value="merge">Merge into existing tag</option><option value="move">Move tree</option><option value="remove">Remove</option></select></label><label>Source <input name="source" required></label><label>Target <input name="target"></label><label><input type="checkbox" name="remember_aliases" value="true"> Remember old names as aliases</label><button name="step" value="preview">Preview</button></form></noscript>"#, escape_html(csrf), escape_html(csrf)));
    }
    render_children(&root, "", &mut out, csrf.is_some(), selected);
    out.push_str("</section>");
    out
}

fn render_blockers(
    root: &std::path::Path,
    issues: &[dimagine_core::tag_batch::PlanIssue],
) -> String {
    if issues.is_empty() {
        return String::new();
    }
    let mut out = String::from("<section><h2>Notes blocking batch operations</h2><ul>");
    for issue in issues {
        let image = issue
            .path
            .strip_suffix(".md")
            .filter(|path| root.join(path).is_file());
        let (route, path) = match image {
            Some(path) => ("image", path),
            None => ("raw", issue.path.as_str()),
        };
        out.push_str(&format!(
            "<li><a href=\"/{route}/{}\">{}</a>: {}</li>",
            escape_html(&query_value(path)),
            escape_html(&issue.path),
            escape_html(&issue.message)
        ));
    }
    out.push_str("</ul></section>");
    out
}

fn needs_review(
    tag: &crate::index_sync::SidebarTag,
    tags: &[crate::index_sync::SidebarTag],
) -> bool {
    if !dimagine_core::tag_aliases::valid_target(&tag.tag) || tag.count <= 2 {
        return true;
    }
    let leaf = tag.tag.rsplit('/').next().unwrap_or(&tag.tag);
    tags.iter().any(|other| {
        other.tag != tag.tag
            && other
                .tag
                .rsplit('/')
                .next()
                .unwrap_or(&other.tag)
                .eq_ignore_ascii_case(leaf)
    })
}

fn render_children(
    node: &Node,
    parent: &str,
    out: &mut String,
    editable: bool,
    selected: Option<&str>,
) {
    let max = node
        .children
        .values()
        .filter_map(|child| child.count)
        .max()
        .unwrap_or(1)
        .max(1);
    out.push_str("<ul class=\"tags-tree\">");
    for (name, child) in &node.children {
        let path = if parent.is_empty() {
            name.clone()
        } else {
            format!("{parent}/{name}")
        };
        out.push_str("<li>");
        if !child.children.is_empty() {
            out.push_str("<details open><summary>");
        } else {
            out.push_str("<div class=\"tags-leaf\">");
        }
        if let Some(count) = child.count {
            if editable {
                out.push_str(&format!("<input type=\"checkbox\" class=\"tag-select\" value=\"{}\" aria-label=\"Select {}\"{}>", escape_html(&path), escape_html(&path), if selected == Some(path.as_str()) { " checked" } else { "" }));
            }
            out.push_str(&format!("<a href=\"/?tag={}\" title=\"{}\">{}</a><span class=\"tags-meter\" aria-hidden=\"true\"><span style=\"width:{}%\"></span></span><span class=\"sidebar-row-count\">{count}</span>", escape_html(&query_value(&path)), escape_html(&path), escape_html(name), count.saturating_mul(100) / max));
        } else {
            out.push_str(&format!(
                "<a class=\"tags-virtual\" href=\"/?tag={}\">{}</a>",
                escape_html(&query_value(&path)),
                escape_html(name)
            ));
        }
        if editable && child.count.is_some() {
            out.push_str("<button type=\"button\" class=\"tag-row-menu\" aria-label=\"Actions for tag\" aria-haspopup=\"menu\">⋯</button><div class=\"tag-row-popup\" role=\"menu\" hidden><button type=\"button\" role=\"menuitem\" data-row-action=\"rename\">Rename</button><button type=\"button\" role=\"menuitem\" data-row-action=\"move\">Move</button><button type=\"button\" role=\"menuitem\" data-row-action=\"remove\">Remove</button></div>");
        }
        if !child.children.is_empty() {
            out.push_str("</summary>");
            render_children(child, &path, out, editable, selected);
            out.push_str("</details>");
        } else {
            out.push_str("</div>");
        }
        out.push_str("</li>");
    }
    out.push_str("</ul>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_sync::SidebarTag;
    #[test]
    fn review_flags_invalid_rare_and_matching_leaf_names() {
        let tags = vec![
            SidebarTag {
                tag: "bad name".into(),
                count: 10,
            },
            SidebarTag {
                tag: "rare".into(),
                count: 2,
            },
            SidebarTag {
                tag: "screen/mobile".into(),
                count: 20,
            },
            SidebarTag {
                tag: "device/mobile".into(),
                count: 30,
            },
            SidebarTag {
                tag: "ordinary".into(),
                count: 10,
            },
        ];
        assert!(tags[..4].iter().all(|tag| needs_review(tag, &tags)));
        assert!(!needs_review(&tags[4], &tags));
    }

    #[test]
    fn management_controls_are_permission_gated_and_escape_token() {
        let data = SidebarData {
            folders: vec![],
            collections: vec![],
            tags: vec![SidebarTag {
                tag: "a&b".into(),
                count: 3,
            }],
            total: 0,
            untagged: 0,
            recent: 0,
        };
        let readonly = body(&data, "", None, None, None, "");
        assert!(!readonly.contains("tag-management"));
        assert!(!readonly.contains("tag-select"));
        let editable = body(&data, "", None, Some("<&"), None, "");
        assert!(editable.contains("data-csrf=\"&lt;&amp;\""));
        assert!(editable.contains("value=\"a&amp;b\""));
    }

    #[test]
    fn tree_marks_only_nodes_with_children_and_escapes_names() {
        let data = SidebarData {
            folders: vec![],
            collections: vec![],
            tags: vec![
                SidebarTag {
                    tag: "a/<b>".into(),
                    count: 2,
                },
                SidebarTag {
                    tag: "z".into(),
                    count: 1,
                },
            ],
            total: 0,
            untagged: 0,
            recent: 0,
        };
        let html = body(&data, "", None, None, None, "");
        assert_eq!(html.matches("<details open>").count(), 1);
        assert!(html.contains("&lt;b&gt;"));
        assert!(!html.contains("<b>"));
        assert!(body(&data, "missing", None, None, None, "").contains("No tags match."));
    }
}
