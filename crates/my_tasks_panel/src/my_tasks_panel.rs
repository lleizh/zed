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
    ActiveTheme, ButtonCommon, Clickable, FluentBuilder, IconButton, IconName, IconSize, Label,
    LabelCommon, Tooltip, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const MY_TASKS_PANEL_KEY: &str = "MyTasksPanel";

actions!(my_tasks_panel, [ToggleFocus]);

#[derive(Debug, Clone, PartialEq)]
enum GhItemKind {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, PartialEq)]
struct GhItem {
    repo: String,
    number: u64,
    title: String,
    url: String,
    kind: GhItemKind,
}

pub struct MyTasksPanel {
    focus_handle: FocusHandle,
    items: Vec<GhItem>,
    running: std::collections::HashSet<String>,
    existing_urls: std::collections::HashSet<String>,
    _workspace: WeakEntity<Workspace>,
    _poll_task: Task<()>,
}

impl EventEmitter<PanelEvent> for MyTasksPanel {}

impl MyTasksPanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn({
            async move |this: WeakEntity<Self>, cx| {
                loop {
                    let (owner, search_days, workforme_root, poll_secs) = {
                        let Ok(vals) = this.update(cx, |_panel, cx| {
                            let s = WorkformePanelSettings::get_global(cx);
                            let root = shellexpand::tilde(&s.workforme_root).to_string();
                            (
                                s.github_owner.clone(),
                                s.search_days,
                                PathBuf::from(root),
                                s.my_tasks_poll_interval_secs,
                            )
                        }) else {
                            break;
                        };
                        vals
                    };
                    let (items, existing) = cx
                        .background_executor()
                        .spawn(async move {
                            (
                                fetch_assigned_items(&owner, search_days),
                                load_existing_task_urls(&workforme_root),
                            )
                        })
                        .await;
                    let result = this.update(cx, |panel, cx| {
                        let changed = panel.items != items || panel.existing_urls != existing;
                        if changed {
                            panel.items = items;
                            panel.existing_urls = existing;
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
            items: Vec::new(),
            running: std::collections::HashSet::new(),
            existing_urls: std::collections::HashSet::new(),
            _workspace: workspace,
            _poll_task: poll_task,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let s = WorkformePanelSettings::get_global(cx);
        let root = PathBuf::from(shellexpand::tilde(&s.workforme_root).to_string());
        self.items = fetch_assigned_items(&s.github_owner, s.search_days);
        self.existing_urls = load_existing_task_urls(&root);
        cx.notify();
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
        // Group items by repo
        let mut repos: Vec<String> = self.items.iter().map(|i| i.repo.clone()).collect();
        repos.sort();
        repos.dedup();

        let mut sections: Vec<gpui::AnyElement> = Vec::new();
        let entity = cx.entity().downgrade();

        for repo in &repos {
            // Repo header
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

            // Items under this repo
            for (idx, item) in self.items.iter().filter(|i| &i.repo == repo).enumerate() {
                let item_clone = item.clone();
                let entity_clone = entity.clone();
                let item_key = format!("{}/{}", item.repo, item.number);
                let is_running = self.running.contains(&item_key);
                let already_exists = self.existing_urls.contains(&item.url);
                let kind_label = match item.kind {
                    GhItemKind::Issue => "issue",
                    GhItemKind::PullRequest => "review",
                };
                let kind_color = match item.kind {
                    GhItemKind::Issue => gpui::rgb(0x238636),
                    GhItemKind::PullRequest => gpui::rgb(0x8957e5),
                };

                let url_for_click = item.url.clone();
                sections.push(
                    h_flex()
                        .id(gpui::ElementId::NamedInteger(
                            format!("mytask-{}", repo).into(),
                            idx as u64,
                        ))
                        .px_3()
                        .py_1()
                        .gap_1()
                        .items_center()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
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
                                .child(
                                    Label::new(item.title.clone())
                                        .size(ui::LabelSize::Small),
                                ),
                        )
                        .child(
                            div()
                                .id(gpui::ElementId::NamedInteger(
                                    format!("mytask-run-{}", repo).into(),
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
                                    let item_clone = item_clone.clone();
                                    let entity_clone = entity_clone.clone();
                                    let item_key = item_key.clone();
                                    el.cursor_pointer()
                                        .bg(cx.theme().colors().element_background)
                                        .hover(|s| s.bg(cx.theme().colors().ghost_element_selected))
                                        .child(
                                            Label::new("▶")
                                                .size(ui::LabelSize::XSmall)
                                                .color(ui::Color::Accent),
                                        )
                                        .on_click(move |_event, _window, cx| {
                                            let item = item_clone.clone();
                                            let entity = entity_clone.clone();
                                            let key = item_key.clone();
                                            if let Some(panel) = entity.upgrade() {
                                                panel.update(cx, |panel, cx| {
                                                    panel.running.insert(key);
                                                    cx.notify();
                                                    let s = WorkformePanelSettings::get_global(cx);
                                                    let root = PathBuf::from(
                                                        shellexpand::tilde(&s.workforme_root)
                                                            .to_string(),
                                                    );
                                                    create_workforme_task(&item, &root);
                                                });
                                            }
                                        })
                                }),
                        )
                        .on_click(move |_event, _window, cx| {
                            cx.open_url(&url_for_click);
                        })
                        .into_any_element(),
                );
            }
        }

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
                        Label::new("My Tasks")
                            .size(ui::LabelSize::Small)
                            .color(ui::Color::Muted),
                    )
                    .child(
                        IconButton::new("mytasks-refresh", IconName::ArrowCircle)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
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
                                    Label::new("No assigned items")
                                        .size(ui::LabelSize::Small)
                                        .color(ui::Color::Muted),
                                ),
                        )
                    })
                    .children(sections),
            )
    }
}

fn date_n_days_ago(days: u64) -> String {
    let output = std::process::Command::new("date")
        .args([&format!("-v-{}d", days), "+%Y-%m-%d"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    output
}

fn fetch_assigned_items(owner: &str, search_days: u64) -> Vec<GhItem> {
    let mut items = Vec::new();
    let since = date_n_days_ago(search_days);
    let created_filter = format!("--created=>={}", since);

    // Fetch assigned issues
    if let Ok(output) = std::process::Command::new("gh")
        .args([
            "search", "issues",
            "--assignee=@me",
            "--state=open",
            &format!("--owner={}", owner),
            &created_filter,
            "--json", "repository,number,title,url",
            "--limit", "50",
        ])
        .output()
    {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
            if let Some(arr) = parsed.as_array() {
                for item in arr {
                    let repo = item
                        .get("repository")
                        .and_then(|v| v.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
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
                    if number > 0 {
                        items.push(GhItem {
                            repo,
                            number,
                            title,
                            url,
                            kind: GhItemKind::Issue,
                        });
                    }
                }
            }
        }
    }

    // Fetch review-requested PRs
    if let Ok(output) = std::process::Command::new("gh")
        .args([
            "search", "prs",
            "--review-requested=@me",
            "--state=open",
            &format!("--owner={}", owner),
            &created_filter,
            "--json", "repository,number,title,url",
            "--limit", "50",
        ])
        .output()
    {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
            if let Some(arr) = parsed.as_array() {
                for item in arr {
                    let repo = item
                        .get("repository")
                        .and_then(|v| v.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
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
                    if number > 0 {
                        items.push(GhItem {
                            repo,
                            number,
                            title,
                            url,
                            kind: GhItemKind::PullRequest,
                        });
                    }
                }
            }
        }
    }

    items
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

fn create_workforme_task(item: &GhItem, workforme_root: &PathBuf) {
    let script = match item.kind {
        GhItemKind::Issue => "scripts/create-task.sh",
        GhItemKind::PullRequest => "scripts/review-pr.sh",
    };

    std::process::Command::new(workforme_root.join(script))
        .arg(&item.url)
        .current_dir(workforme_root)
        .spawn()
        .ok();
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
