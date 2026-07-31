use std::path::Path;

use gpui::{IntoElement, ParentElement, Styled, div};
use ui::{Label, LabelCommon};

#[derive(Debug, Clone, PartialEq)]
pub enum ItemKind {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HumanReview {
    pub reviewer: String,
    pub state: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InboxItem {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub kind: ItemKind,
    pub role: String,
    pub status: String,
    pub flags: Vec<String>,
    pub summary: String,
    pub greptile_score: Option<u8>,
    pub human_reviews: Vec<HumanReview>,
    pub ci_status: Option<String>,
    pub mergeable: Option<bool>,
}

pub struct Inbox {
    pub items: Vec<InboxItem>,
    pub updated_at: Option<String>,
}

/// status 文字列から badge 色を返す。未知の status はデフォルト色。
pub fn status_color(status: &str) -> u32 {
    match status {
        "approved" => 0x238636,
        "changes_requested" => 0xd29922,
        "re_requested" => 0xf85149,
        "review_commented" => 0x1f6feb,
        "commented" => 0x58a6ff,
        "merged" => 0x8957e5,
        "dismissed" => 0x848d97,
        "closed" => 0xda3633,
        "open" => 0x848d97,
        _ => 0x848d97,
    }
}

/// status + flags を一括で badge 列として描画する。
/// open/空 status は表示しない。flags は status と重複するものを除外。
pub fn render_badges(status: &str, flags: &[String]) -> Vec<gpui::AnyElement> {
    let mut badges = Vec::new();
    if !status.is_empty() && status != "open" {
        badges.push(
            div()
                .flex_shrink_0()
                .px_1()
                .rounded_sm()
                .bg(gpui::rgb(status_color(status)))
                .child(
                    Label::new(status.to_string())
                        .size(ui::LabelSize::XSmall)
                        .color(ui::Color::Default),
                )
                .into_any_element(),
        );
    }
    for f in flags {
        if f == status {
            continue;
        }
        badges.push(
            Label::new(f.clone())
                .size(ui::LabelSize::XSmall)
                .color(ui::Color::Custom(gpui::rgb(status_color(f)).into()))
                .into_any_element(),
        );
    }
    badges
}

pub fn load_inbox(workforme_root: &Path) -> Inbox {
    let inbox_path = workforme_root.join(".work/inbox.json");
    let content = match std::fs::read_to_string(&inbox_path) {
        Ok(c) => c,
        Err(_) => return Inbox { items: Vec::new(), updated_at: None },
    };
    parse_inbox(&content)
}

pub fn parse_inbox(content: &str) -> Inbox {
    let val: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return Inbox { items: Vec::new(), updated_at: None },
    };

    let updated_at = val
        .get("updated_at")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let Some(items_arr) = val.get("items").and_then(|v| v.as_array()) else {
        return Inbox { items: Vec::new(), updated_at };
    };

    let items = items_arr
        .iter()
        .filter_map(|item| parse_item(item))
        .collect();

    Inbox { items, updated_at }
}

fn parse_item(item: &serde_json::Value) -> Option<InboxItem> {
    let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
    if number == 0 {
        return None;
    }

    let repo = item.get("repo").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let title = item.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let url = item.get("url").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let kind = match item.get("kind").and_then(|v| v.as_str()).unwrap_or("issue") {
        "pull_request" => ItemKind::PullRequest,
        _ => ItemKind::Issue,
    };
    let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("assignee").to_string();
    let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("open").to_string();
    let flags = item
        .get("flags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let summary = item.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let greptile_score = item.get("greptile_score").and_then(|v| v.as_u64()).map(|v| v as u8);

    let human_reviews = item
        .get("human_reviews")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(HumanReview {
                        reviewer: r.get("reviewer")?.as_str()?.to_string(),
                        state: r.get("state")?.as_str()?.to_string(),
                        summary: r.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let ci_status = item.get("ci_status").and_then(|v| v.as_str()).map(|s| s.to_string());
    let mergeable = item.get("mergeable").and_then(|v| v.as_bool());

    Some(InboxItem {
        repo,
        number,
        title,
        url,
        kind,
        role,
        status,
        flags,
        summary,
        greptile_score,
        human_reviews,
        ci_status,
        mergeable,
    })
}
