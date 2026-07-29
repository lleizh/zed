use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    StatefulInteractiveElement, Styled, Task, WeakEntity, Window, actions, div, px,
};
use settings::Settings;
use workforme_panel::WorkformePanelSettings;
use ui::{
    ActiveTheme, ButtonCommon, Clickable, Disableable, FluentBuilder, IconButton, IconName,
    IconSize, Label, LabelCommon, Tooltip, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const MY_TASKS_PANEL_KEY: &str = "MyTasksPanel";

actions!(my_tasks_panel, [ToggleFocus]);

#[derive(Debug, Clone, Copy, PartialEq)]
enum PanelTab {
    Mine,
    Team,
}

#[derive(Debug, Clone, PartialEq)]
enum ItemKind {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, PartialEq)]
struct HumanReview {
    reviewer: String,
    state: String,
    summary: String,
}

#[derive(Debug, Clone, PartialEq)]
struct InboxItem {
    repo: String,
    number: u64,
    title: String,
    url: String,
    kind: ItemKind,
    role: String,
    state: String,
    summary: String,
    greptile_score: Option<u8>,
    review_state: Option<String>,
    human_reviews: Vec<HumanReview>,
}

pub struct MyTasksPanel {
    focus_handle: FocusHandle,
    active_tab: PanelTab,
    items: Vec<InboxItem>,
    running: std::collections::HashSet<String>,
    existing_urls: std::collections::HashSet<String>,
    syncing: bool,
    updated_at: Option<String>,
    _workspace: WeakEntity<Workspace>,
    _poll_task: Task<()>,
}

impl EventEmitter<PanelEvent> for MyTasksPanel {}

impl MyTasksPanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn({
            async move |this: WeakEntity<Self>, cx| {
                let mut ticks_since_sync: u64 = 3600; // 初回は即 sync
                loop {
                    let (workforme_root, poll_secs) = {
                        let Ok(vals) = this.update(cx, |_panel, cx| {
                            let s = WorkformePanelSettings::get_global(cx);
                            let root = shellexpand::tilde(&s.workforme_root).to_string();
                            (PathBuf::from(root), s.my_tasks_poll_interval_secs)
                        }) else {
                            break;
                        };
                        vals
                    };

                    // 1 時間（3600 秒）ごとに sync-inbox.sh を実行（平日 9-20 時のみ）
                    ticks_since_sync += poll_secs;
                    if ticks_since_sync >= 3600 {
                        ticks_since_sync = 0;
                        let root = workforme_root.clone();
                        cx.background_executor()
                            .spawn(async move {
                                if is_working_hours() {
                                    std::process::Command::new(root.join("scripts/sync-inbox.sh"))
                                        .current_dir(&root)
                                        .output()
                                        .ok();
                                }
                            })
                            .await;
                    }

                    let root = workforme_root.clone();
                    let (items, updated_at, existing) = cx
                        .background_executor()
                        .spawn(async move {
                            let (items, updated_at) = load_inbox(&root);
                            let existing = load_existing_task_urls(&root);
                            (items, updated_at, existing)
                        })
                        .await;
                    let result = this.update(cx, |panel, cx| {
                        let changed = panel.items != items
                            || panel.existing_urls != existing
                            || panel.updated_at != updated_at;
                        if changed {
                            panel.items = items;
                            panel.existing_urls = existing;
                            panel.updated_at = updated_at;
                            cx.notify();
                        }
                    });
                    if result.is_err() {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_secs(poll_secs))
                        .await;
                }
            }
        });

        Self {
            focus_handle: cx.focus_handle(),
            active_tab: PanelTab::Mine,
            items: Vec::new(),
            running: std::collections::HashSet::new(),
            existing_urls: std::collections::HashSet::new(),
            syncing: false,
            updated_at: None,
            _workspace: workspace,
            _poll_task: poll_task,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let s = WorkformePanelSettings::get_global(cx);
        let root = PathBuf::from(shellexpand::tilde(&s.workforme_root).to_string());

        self.syncing = true;
        cx.notify();

        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            // sync-inbox.sh を実行
            cx.background_executor()
                .spawn({
                    let root = root.clone();
                    async move {
                        std::process::Command::new(root.join("scripts/sync-inbox.sh"))
                            .current_dir(&root)
                            .output()
                            .ok();
                    }
                })
                .await;

            // スクリプトは bg agent を起動するだけなので即読み込み
            let (items, updated_at, existing) = cx
                .background_executor()
                .spawn(async move {
                    let (items, updated_at) = load_inbox(&root);
                    let existing = load_existing_task_urls(&root);
                    (items, updated_at, existing)
                })
                .await;
            this.update(cx, |panel, cx| {
                panel.items = items;
                panel.existing_urls = existing;
                panel.updated_at = updated_at;
                panel.syncing = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn filtered_items(&self, tab: PanelTab) -> Vec<&InboxItem> {
        self.items
            .iter()
            .filter(|item| match tab {
                PanelTab::Mine => item.role == "assignee" || item.role == "reviewer",
                PanelTab::Team => item.role == "team",
            })
            .collect()
    }

    fn render_item_list(
        &self,
        items: Vec<&InboxItem>,
        id_prefix: &str,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut repos: Vec<String> = items.iter().map(|i| i.repo.clone()).collect();
        repos.sort();
        repos.dedup();

        let mut sections: Vec<gpui::AnyElement> = Vec::new();
        let entity = cx.entity().downgrade();

        for repo in &repos {
            sections.push(
                h_flex()
                    .px_2()
                    .py_0p5()
                    .child(
                        Label::new(repo.clone())
                            .size(ui::LabelSize::Small)
                            .color(ui::Color::Muted),
                    )
                    .into_any_element(),
            );

            for (idx, item) in items.iter().filter(|i| &i.repo == repo).enumerate() {
                let item_key = format!("{}/{}", item.repo, item.number);
                let is_running = self.running.contains(&item_key);
                let already_exists = self.existing_urls.contains(&item.url);
                let kind_label = match item.kind {
                    ItemKind::Issue => "issue",
                    ItemKind::PullRequest => "PR",
                };
                let kind_color = match item.kind {
                    ItemKind::Issue => gpui::rgb(0x238636),
                    ItemKind::PullRequest => gpui::rgb(0x8957e5),
                };

                let url_for_click = item.url.clone();
                let item_for_task = (*item).clone();
                let entity_clone = entity.clone();
                let item_key_clone = item_key.clone();

                let review_state_badge = match item.review_state.as_deref() {
                    Some("approved") => Some(("Approved", gpui::rgb(0x238636))),
                    Some("changes_requested") => Some(("Changes", gpui::rgb(0xd29922))),
                    _ => None,
                };

                let state_badge = match item.state.as_str() {
                    "merged" => Some(("Merged", gpui::rgb(0x8957e5))),
                    "closed" => Some(("Closed", gpui::rgb(0xda3633))),
                    _ => None,
                };

                let badge = review_state_badge.or(state_badge);

                sections.push(
                    v_flex()
                        .id(gpui::ElementId::NamedInteger(
                            format!("{}-{}", id_prefix, repo).into(),
                            idx as u64,
                        ))
                        .px_3()
                        .py_1()
                        .gap_0p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                        .on_click(move |_event, _window, cx| {
                            cx.open_url(&url_for_click);
                        })
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    div()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(kind_color)
                                        .child(
                                            Label::new(kind_label)
                                                .size(ui::LabelSize::XSmall)
                                                .color(ui::Color::Default),
                                        ),
                                )
                                .child(
                                    Label::new(format!("#{}", item.number))
                                        .size(ui::LabelSize::XSmall)
                                        .color(ui::Color::Muted),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(
                                            Label::new(item.title.clone())
                                                .size(ui::LabelSize::Small)
                                                .single_line(),
                                        ),
                                )
                                .when(item.greptile_score.is_some(), |el| {
                                    let score = item.greptile_score.unwrap();
                                    let color = if score >= 4 {
                                        gpui::rgb(0x238636)
                                    } else if score >= 3 {
                                        gpui::rgb(0xd29922)
                                    } else {
                                        gpui::rgb(0xda3633)
                                    };
                                    el.child(
                                        div()
                                            .px_1()
                                            .rounded_sm()
                                            .child(
                                                Label::new(format!("g:{}/5", score))
                                                    .size(ui::LabelSize::XSmall)
                                                    .color(ui::Color::Custom(color.into())),
                                            ),
                                    )
                                })
                                .when(badge.is_some(), |el| {
                                    let (label, color) = badge.unwrap();
                                    el.child(
                                        div()
                                            .px_1()
                                            .rounded_sm()
                                            .bg(color)
                                            .child(
                                                Label::new(label)
                                                    .size(ui::LabelSize::XSmall)
                                                    .color(ui::Color::Default),
                                            ),
                                    )
                                })
                                .when(
                                    badge.map(|b| b.0) != Some("Approved")
                                        && badge.map(|b| b.0) != Some("Merged"),
                                    |el| {
                                        el.child(
                                            div()
                                                .id(gpui::ElementId::NamedInteger(
                                                    format!("{}-run-{}", id_prefix, repo).into(),
                                                    idx as u64,
                                                ))
                                                .px_1()
                                                .rounded_sm()
                                                .when(already_exists, |el| {
                                                    el.child(
                                                        Label::new("✓")
                                                            .size(ui::LabelSize::XSmall)
                                                            .color(ui::Color::Success),
                                                    )
                                                })
                                                .when(!already_exists && is_running, |el| {
                                                    el.bg(cx.theme().colors().ghost_element_selected)
                                                        .child(
                                                            Label::new("...")
                                                                .size(ui::LabelSize::XSmall)
                                                                .color(ui::Color::Muted),
                                                        )
                                                })
                                                .when(!already_exists && !is_running, |el| {
                                                    let item_clone = item_for_task.clone();
                                                    let entity_clone = entity_clone.clone();
                                                    let key = item_key_clone.clone();
                                                    el.cursor_pointer()
                                                        .bg(cx.theme().colors().element_background)
                                                        .hover(|s| {
                                                            s.bg(cx
                                                                .theme()
                                                                .colors()
                                                                .ghost_element_selected)
                                                        })
                                                        .child(
                                                            Label::new("▶")
                                                                .size(ui::LabelSize::XSmall)
                                                                .color(ui::Color::Accent),
                                                        )
                                                        .on_click(move |_event, _window, cx| {
                                                            cx.stop_propagation();
                                                            let item = item_clone.clone();
                                                            let entity = entity_clone.clone();
                                                            let key = key.clone();
                                                            if let Some(panel) = entity.upgrade() {
                                                                panel.update(cx, |panel, cx| {
                                                                    panel.running.insert(key);
                                                                    cx.notify();
                                                                    let s = WorkformePanelSettings::get_global(cx);
                                                                    let root = PathBuf::from(
                                                                        shellexpand::tilde(
                                                                            &s.workforme_root,
                                                                        )
                                                                        .to_string(),
                                                                    );
                                                                    create_workforme_task(
                                                                        &item, &root,
                                                                    );
                                                                });
                                                            }
                                                        })
                                                }),
                                        )
                                    },
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new(item.summary.clone())
                                        .size(ui::LabelSize::XSmall)
                                        .color(ui::Color::Muted)
                                        .single_line(),
                                ),
                        )
                        .into_any_element(),
                );
            }
        }
        sections
    }
}

impl Focusable for MyTasksPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for MyTasksPanel {
    fn persistent_name() -> &'static str {
        "MyTasksPanel"
    }

    fn panel_key() -> &'static str {
        MY_TASKS_PANEL_KEY
    }

    fn position(&self, _: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Right | DockPosition::Left)
    }

    fn set_position(&mut self, _position: DockPosition, _: &mut Window, _cx: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::AtSign)
    }

    fn icon_tooltip(&self, _: &Window, _cx: &App) -> Option<&'static str> {
        Some("My Tasks")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        12
    }
}

impl Render for MyTasksPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_tab = self.active_tab;
        let filtered = self.filtered_items(active_tab);
        let sections = self.render_item_list(filtered, match active_tab {
            PanelTab::Mine => "mytask",
            PanelTab::Team => "team",
        }, cx);

        let entity = cx.entity().downgrade();
        let entity_for_team = entity.clone();

        let empty_label = match active_tab {
            PanelTab::Mine => "No assigned items",
            PanelTab::Team => "No review requests",
        };

        div()
            .id("my-tasks-panel")
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("tab-mine")
                                    .cursor_pointer()
                                    .px_1()
                                    .when(active_tab == PanelTab::Mine, |el| {
                                        el.border_b_2()
                                            .border_color(cx.theme().colors().text)
                                    })
                                    .child(
                                        Label::new("Mine")
                                            .size(ui::LabelSize::Small)
                                            .color(if active_tab == PanelTab::Mine {
                                                ui::Color::Default
                                            } else {
                                                ui::Color::Muted
                                            }),
                                    )
                                    .on_click({
                                        let entity = entity.clone();
                                        move |_event, _window, cx| {
                                            if let Some(panel) = entity.upgrade() {
                                                panel.update(cx, |panel, cx| {
                                                    panel.active_tab = PanelTab::Mine;
                                                    cx.notify();
                                                });
                                            }
                                        }
                                    }),
                            )
                            .child(
                                div()
                                    .id("tab-team")
                                    .cursor_pointer()
                                    .px_1()
                                    .when(active_tab == PanelTab::Team, |el| {
                                        el.border_b_2()
                                            .border_color(cx.theme().colors().text)
                                    })
                                    .child(
                                        Label::new("Team")
                                            .size(ui::LabelSize::Small)
                                            .color(if active_tab == PanelTab::Team {
                                                ui::Color::Default
                                            } else {
                                                ui::Color::Muted
                                            }),
                                    )
                                    .on_click(move |_event, _window, cx| {
                                        if let Some(panel) = entity_for_team.upgrade() {
                                            panel.update(cx, |panel, cx| {
                                                panel.active_tab = PanelTab::Team;
                                                cx.notify();
                                            });
                                        }
                                    }),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Label::new(if self.syncing {
                                    "Syncing...".to_string()
                                } else {
                                    self.updated_at
                                        .as_ref()
                                        .map(|s| format_updated_at(s))
                                        .unwrap_or_default()
                                })
                                    .size(ui::LabelSize::XSmall)
                                    .color(ui::Color::Muted),
                            )
                            .child(
                                IconButton::new("mytasks-refresh", IconName::ArrowCircle)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Sync inbox"))
                                    .disabled(self.syncing)
                                    .on_click({
                                        let entity = cx.entity().downgrade();
                                        move |_event, _window, cx| {
                                            if let Some(panel) = entity.upgrade() {
                                                panel.update(cx, |panel, cx| {
                                                    panel.reload(cx);
                                                });
                                            }
                                        }
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("mytasks-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(sections.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .p_4()
                                .gap_2()
                                .items_center()
                                .child(
                                    Label::new(empty_label)
                                        .size(ui::LabelSize::Small)
                                        .color(ui::Color::Muted),
                                ),
                        )
                    })
                    .children(sections),
            )
    }
}

fn is_working_hours() -> bool {
    // 平日 9:00-20:00 (JST) のみ true
    let output = std::process::Command::new("date")
        .args(["+%u %H"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let parts: Vec<&str> = output.split_whitespace().collect();
    if parts.len() != 2 {
        return true;
    }
    let dow: u32 = parts[0].parse().unwrap_or(1);
    let hour: u32 = parts[1].parse().unwrap_or(12);
    dow <= 5 && hour >= 10 && hour < 19
}

fn format_updated_at(iso: &str) -> String {
    // ISO8601 UTC → JST (UTC+9) の HH:MM 表示
    let time_part = match iso.split('T').nth(1) {
        Some(t) => t,
        None => return iso.to_string(),
    };
    let hm: String = time_part.chars().take(5).collect();
    let parts: Vec<&str> = hm.split(':').collect();
    if parts.len() == 2 {
        if let (Ok(h), Ok(m)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
            let jst_h = (h + 9) % 24;
            return format!("{:02}:{:02}", jst_h, m);
        }
    }
    hm
}

fn load_inbox(workforme_root: &PathBuf) -> (Vec<InboxItem>, Option<String>) {
    let inbox_path = workforme_root.join(".work/inbox.json");
    let Ok(content) = std::fs::read_to_string(&inbox_path) else {
        return (Vec::new(), None);
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
        return (Vec::new(), None);
    };
    let updated_at = val
        .get("updated_at")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(items_arr) = val.get("items").and_then(|v| v.as_array()) else {
        return (Vec::new(), updated_at);
    };

    let mut items = Vec::new();
    for item in items_arr {
        let repo = item
            .get("repo")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        if number == 0 {
            continue;
        }
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let url = item
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let kind = match item.get("kind").and_then(|v| v.as_str()).unwrap_or("issue") {
            "pull_request" => ItemKind::PullRequest,
            _ => ItemKind::Issue,
        };
        let role = item
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("assignee")
            .to_string();
        let state = item
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("open")
            .to_string();
        let summary = item
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let greptile_score = item
            .get("greptile_score")
            .and_then(|v| v.as_u64())
            .map(|v| v as u8);
        let review_state = item
            .get("review_state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let human_reviews = item
            .get("human_reviews")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        Some(HumanReview {
                            reviewer: r.get("reviewer")?.as_str()?.to_string(),
                            state: r.get("state")?.as_str()?.to_string(),
                            summary: r
                                .get("summary")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        items.push(InboxItem {
            repo,
            number,
            title,
            url,
            kind,
            role,
            state,
            summary,
            greptile_score,
            review_state,
            human_reviews,
        });
    }
    (items, updated_at)
}

fn load_existing_task_urls(workforme_root: &PathBuf) -> std::collections::HashSet<String> {
    let mut urls = std::collections::HashSet::new();
    let work_dir = workforme_root.join(".work");
    let Ok(entries) = std::fs::read_dir(&work_dir) else {
        return urls;
    };
    for entry in entries.flatten() {
        let state_path = entry.path().join("state.json");
        let Ok(content) = std::fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if let Some(url) = val.get("url").and_then(|v| v.as_str()) {
            urls.insert(url.to_string());
        }
        if let Some(pr_url) = val.get("pr_url").and_then(|v| v.as_str()) {
            urls.insert(pr_url.to_string());
        }
    }
    urls
}

fn create_workforme_task(item: &InboxItem, workforme_root: &PathBuf) {
    let (workflow_type, first_stage) = match item.kind {
        ItemKind::Issue => ("issue-dev", "plan"),
        ItemKind::PullRequest => ("pr-review", "review-pr"),
    };

    let root = workforme_root.clone();
    let url = item.url.clone();
    std::thread::spawn(move || {
        let output = std::process::Command::new(root.join("scripts/create-task.sh"))
            .arg(workflow_type)
            .arg(&url)
            .current_dir(&root)
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                let task_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !task_dir.is_empty() {
                    std::process::Command::new(root.join("scripts/run-stage.sh"))
                        .arg(&task_dir)
                        .arg(first_stage)
                        .current_dir(&root)
                        .spawn()
                        .ok();
                }
            }
        }
    });
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<MyTasksPanel>(window, cx);
        });
    })
    .detach();
}

pub async fn load(
    workspace: WeakEntity<Workspace>,
    mut cx: AsyncWindowContext,
) -> anyhow::Result<Entity<MyTasksPanel>> {
    let weak = workspace.clone();
    workspace
        .update_in(&mut cx, |_workspace, _window, cx| {
            cx.new(|cx| MyTasksPanel::new(weak, cx))
        })
        .context("failed to create MyTasksPanel")
}
