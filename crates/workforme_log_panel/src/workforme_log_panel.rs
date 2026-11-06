use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    StatefulInteractiveElement, Styled, Task, WeakEntity, Window, actions, div, px,
};
use settings::Settings;
use ui::{
    ActiveTheme, ButtonCommon, Clickable, FluentBuilder, IconButton, IconName, IconSize, Label,
    LabelCommon, Tooltip, h_flex,
};
use workforme_panel::WorkformePanelSettings;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const LOG_PANEL_KEY: &str = "WorkformeLogPanel";

actions!(workforme_log_panel, [ToggleFocus]);

#[derive(Debug, Clone, PartialEq)]
struct LogEntry {
    timestamp: String,
    action: String,
    target: String,
    title: String,
}

pub struct WorkformeLogPanel {
    focus_handle: FocusHandle,
    entries: Vec<LogEntry>,
    _workspace: WeakEntity<Workspace>,
    _poll_task: Task<()>,
}

impl EventEmitter<PanelEvent> for WorkformeLogPanel {}

impl WorkformeLogPanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn({
            async move |this: WeakEntity<Self>, cx| {
                loop {
                    let workforme_root = {
                        let Ok(root) = this.update(cx, |_panel, cx| {
                            let settings = WorkformePanelSettings::get_global(cx);
                            PathBuf::from(shellexpand::tilde(&settings.workforme_root).to_string())
                        }) else {
                            break;
                        };
                        root
                    };

                    let entries = cx
                        .background_executor()
                        .spawn(async move { load_log_entries(&workforme_root) })
                        .await;

                    let result = this.update(cx, |panel, cx| {
                        if panel.entries != entries {
                            panel.entries = entries;
                            cx.notify();
                        }
                    });
                    if result.is_err() {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_secs(10))
                        .await;
                }
            }
        });

        let settings = WorkformePanelSettings::get_global(cx);
        let root = PathBuf::from(shellexpand::tilde(&settings.workforme_root).to_string());
        let entries = load_log_entries(&root);

        Self {
            focus_handle: cx.focus_handle(),
            entries,
            _workspace: workspace,
            _poll_task: poll_task,
        }
    }
}

impl Focusable for WorkformeLogPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for WorkformeLogPanel {
    fn persistent_name() -> &'static str {
        "WorkformeLogPanel"
    }

    fn panel_key() -> &'static str {
        LOG_PANEL_KEY
    }

    fn position(&self, _: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Bottom)
    }

    fn set_position(&mut self, _position: DockPosition, _: &mut Window, _cx: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _cx: &App) -> Pixels {
        px(200.)
    }

    fn icon(&self, _: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _: &Window, _cx: &App) -> Option<&'static str> {
        Some("Workforme Log")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        13
    }
}

impl Render for WorkformeLogPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = &self.entries;

        div()
            .id("workforme-log-panel")
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
                        Label::new("Workforme Log")
                            .size(ui::LabelSize::Small)
                            .color(ui::Color::Default),
                    )
                    .child(
                        IconButton::new("clear-log", IconName::Trash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Clear Log"))
                            .on_click(cx.listener(|panel, _event, _window, cx| {
                                let settings = WorkformePanelSettings::get_global(cx);
                                let root = PathBuf::from(
                                    shellexpand::tilde(&settings.workforme_root).to_string(),
                                );
                                let log_path = root.join(".work/auto-review.log");
                                let _ = std::fs::write(&log_path, "");
                                panel.entries.clear();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("workforme-log-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .py_1()
                    .when(entries.is_empty(), |el| {
                        el.child(
                            Label::new("No log entries")
                                .size(ui::LabelSize::Small)
                                .color(ui::Color::Muted),
                        )
                    })
                    .children(entries.iter().rev().enumerate().map(|(i, entry)| {
                        let action_color = match entry.action.as_str() {
                            "done" => ui::Color::Success,
                            "failed" => ui::Color::Error,
                            "skip" => ui::Color::Muted,
                            _ => ui::Color::Default,
                        };
                        let icon = match entry.action.as_str() {
                            "done" => "✓",
                            "failed" => "✗",
                            "skip" => "—",
                            _ => "●",
                        };

                        h_flex()
                            .id(gpui::ElementId::NamedInteger("log-entry".into(), i as u64))
                            .gap_2()
                            .py_0p5()
                            .border_b_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Label::new(format_timestamp(&entry.timestamp))
                                    .size(ui::LabelSize::XSmall)
                                    .color(ui::Color::Muted),
                            )
                            .child(
                                Label::new(icon)
                                    .size(ui::LabelSize::XSmall)
                                    .color(action_color),
                            )
                            .child(
                                Label::new(entry.target.clone())
                                    .size(ui::LabelSize::XSmall)
                                    .color(ui::Color::Default),
                            )
                            .when(!entry.title.is_empty(), |el| {
                                el.child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(
                                            Label::new(entry.title.clone())
                                                .size(ui::LabelSize::XSmall)
                                                .color(ui::Color::Muted)
                                                .single_line(),
                                        ),
                                )
                            })
                            .into_any_element()
                    })),
            )
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorkformeLogPanel>(window, cx);
        });
    })
    .detach();
}

pub async fn load(
    workspace: WeakEntity<Workspace>,
    mut cx: AsyncWindowContext,
) -> anyhow::Result<Entity<WorkformeLogPanel>> {
    let weak = workspace.clone();
    workspace
        .update_in(&mut cx, |_workspace, _window, cx| {
            cx.new(|cx| WorkformeLogPanel::new(weak, cx))
        })
        .context("failed to create WorkformeLogPanel")
}

fn load_log_entries(workforme_root: &std::path::Path) -> Vec<LogEntry> {
    let log_path = workforme_root.join(".work/auto-review.log");
    let content = match std::fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            // format: "2026-11-05T07:30:15Z done ssp-adserver#11719 feat: ..."
            let mut parts = line.splitn(4, ' ');
            let timestamp = parts.next()?.to_string();
            let action = parts.next()?.to_string();
            let rest = parts.next().unwrap_or("").to_string();
            let title = parts.next().unwrap_or("").to_string();

            let target = if action == "skip" {
                rest.clone() // "candidates=0"
            } else {
                rest
            };

            Some(LogEntry {
                timestamp,
                action,
                target,
                title,
            })
        })
        .collect()
}

fn format_timestamp(ts: &str) -> String {
    // "2026-11-05T07:30:15Z" → "11-05 07:30"
    if ts.len() >= 16 {
        format!("{} {}", &ts[5..10], &ts[11..16])
    } else {
        ts.to_string()
    }
}
