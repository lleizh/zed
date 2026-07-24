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

#[derive(Debug, Clone, Copy, PartialEq)]
enum PanelTab {
    Mine,
    Team,
}

#[derive(Debug, Clone, PartialEq)]
enum GhItemKind {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, PartialEq)]
enum PrState {
    None,
    Open,
    Approved,
    ChangesRequested,
    Merged,
}

#[derive(Debug, Clone, PartialEq)]
struct GhItem {
    repo: String,
    number: u64,
    title: String,
    url: String,
    kind: GhItemKind,
    pr_state: PrState,
}

pub struct MyTasksPanel {
    focus_handle: FocusHandle,
    active_tab: PanelTab,
    items: Vec<GhItem>,
    team_items: Vec<GhItem>,
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
                    let owner_clone = owner.clone();
                    let owner_clone2 = owner_clone.clone();
                    let (items, existing, team_items) = cx
                        .background_executor()
                        .spawn(async move {
                            let mut items = fetch_assigned_items(&owner, search_days);
                            check_pr_states(&mut items, &owner_clone);
                            let existing = load_existing_task_urls(&workforme_root);
                            let mut team = fetch_team_review_items(&owner_clone2);
                            check_pr_states(&mut team, &owner_clone2);
                            (items, existing, team)
                        })
                        .await;
                    let result = this.update(cx, |panel, cx| {
                        let changed = panel.items != items
                            || panel.existing_urls != existing
                            || panel.team_items != team_items;
                        if changed {
                            panel.items = items;
                            panel.existing_urls = existing;
                            panel.team_items = team_items;
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
            team_items: Vec::new(),
            running: std::collections::HashSet::new(),
            existing_urls: std::collections::HashSet::new(),
            _workspace: workspace,
            _poll_task: poll_task,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let s = WorkformePanelSettings::get_global(cx);
        let owner = s.github_owner.clone();
        let search_days = s.search_days;
        let root = PathBuf::from(shellexpand::tilde(&s.workforme_root).to_string());

        let owner_clone = owner.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let (items, existing, team_items) = cx
                .background_executor()
                .spawn(async move {
                    let mut items = fetch_assigned_items(&owner, search_days);
                    check_pr_states(&mut items, &owner);
                    let existing = load_existing_task_urls(&root);
                    let mut team = fetch_team_review_items(&owner_clone);
                    check_pr_states(&mut team, &owner_clone);
                    (items, existing, team)
                })
                .await;
            this.update(cx, |panel, cx| {
                panel.items = items;
                panel.existing_urls = existing;
                panel.team_items = team_items;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_item_list(
        &self,
        items: &[GhItem],
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
                            format!("{}-{}", id_prefix, repo).into(),
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
                                .min_w_0()
                                .overflow_hidden()
                                .child(
                                    Label::new(item.title.clone())
                                        .size(ui::LabelSize::Small)
                                        .single_line(),
                                ),
                        )
                        .when(
                            item.pr_state != PrState::None && item.pr_state != PrState::Open,
                            |el| {
                                let (label, color) = match item.pr_state {
                                    PrState::Merged => ("Merged", gpui::rgb(0x8957e5)),
                                    PrState::Approved => ("Approved", gpui::rgb(0x238636)),
                                    PrState::ChangesRequested => ("Changes", gpui::rgb(0xd29922)),
                                    _ => ("", gpui::rgb(0x000000)),
                                };
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
                            },
                        )
                        .when(
                            item.pr_state != PrState::Approved && item.pr_state != PrState::Merged,
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
                                            cx.stop_propagation();
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
                        )},
                        )
                        .on_click(move |_event, _window, cx| {
                            cx.open_url(&url_for_click);
                        })
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
        let sections = match active_tab {
            PanelTab::Mine => self.render_item_list(&self.items.clone(), "mytask", cx),
            PanelTab::Team => self.render_item_list(&self.team_items.clone(), "team", cx),
        };

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

fn fetch_team_review_items(owner: &str) -> Vec<GhItem> {
    let mut items = Vec::new();

    if let Ok(output) = std::process::Command::new("gh")
        .args([
            "search", "prs",
            "--state=open",
            &format!("--owner={}", owner),
            "--label=review-requested",
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
                            pr_state: PrState::Open,
                        });
                    }
                }
            }
        }
    }

    items
}

fn check_pr_states(items: &mut [GhItem], owner: &str) {
    let pr_indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.kind == GhItemKind::PullRequest)
        .map(|(i, _)| i)
        .collect();
    if pr_indices.is_empty() {
        return;
    }

    let mut query_parts = Vec::new();
    for (idx, &item_idx) in pr_indices.iter().enumerate() {
        let item = &items[item_idx];
        query_parts.push(format!(
            "pr{idx}: repository(owner: \"{owner}\", name: \"{}\") {{ pullRequest(number: {}) {{ state reviewDecision }} }}",
            item.repo, item.number
        ));
    }
    let graphql_query = format!("{{ {} }}", query_parts.join(" "));

    let Ok(output) = std::process::Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", graphql_query)])
        .output()
    else {
        return;
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return;
    };
    let Some(data) = parsed.get("data") else {
        return;
    };

    for (idx, &item_idx) in pr_indices.iter().enumerate() {
        let key = format!("pr{idx}");
        let Some(pr_data) = data.get(&key).and_then(|r| r.get("pullRequest")) else {
            continue;
        };
        let state = pr_data.get("state").and_then(|v| v.as_str()).unwrap_or("");
        let review_decision = pr_data
            .get("reviewDecision")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        items[item_idx].pr_state = match state {
            "MERGED" => PrState::Merged,
            _ => match review_decision {
                "APPROVED" => PrState::Approved,
                "CHANGES_REQUESTED" => PrState::ChangesRequested,
                _ => PrState::Open,
            },
        };
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
                            pr_state: PrState::None,
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
                            pr_state: PrState::Open,
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
