#![cfg_attr(target_family = "wasm", no_main)]

use gpui::{
    App, Bounds, Context, FocusHandle, KeyBinding, Orientation, Role, SharedString, Toggled,
    Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};
use gpui_platform::application;

actions!(a11y_example, [Tab, TabPrev, ToggleDarkMode]);

// --- Data tables demo ---

struct FileEntry {
    name: &'static str,
    kind: &'static str,
    size: &'static str,
}

const FILES: &[FileEntry] = &[
    FileEntry {
        name: "README.md",
        kind: "Markdown",
        size: "4 KB",
    },
    FileEntry {
        name: "main.rs",
        kind: "Rust",
        size: "12 KB",
    },
    FileEntry {
        name: "Cargo.toml",
        kind: "TOML",
        size: "1 KB",
    },
    FileEntry {
        name: "lib.rs",
        kind: "Rust",
        size: "8 KB",
    },
];

// --- Tree data ---

struct TreeNode {
    label: &'static str,
    depth: usize,
    children: &'static [TreeNode],
}

const FILE_TREE: &[TreeNode] = &[
    TreeNode {
        label: "src",
        depth: 1,
        children: &[
            TreeNode {
                label: "main.rs",
                depth: 2,
                children: &[],
            },
            TreeNode {
                label: "lib.rs",
                depth: 2,
                children: &[],
            },
        ],
    },
    TreeNode {
        label: "tests",
        depth: 1,
        children: &[TreeNode {
            label: "integration.rs",
            depth: 2,
            children: &[],
        }],
    },
    TreeNode {
        label: "README.md",
        depth: 1,
        children: &[],
    },
];

// --- App state ---

struct A11yExample {
    focus_handle: FocusHandle,
    dark_mode: bool,
    notifications_enabled: bool,
    auto_save: bool,
    selected_tab: usize,
    progress: f64,
    expanded_tree_nodes: Vec<bool>,
    selected_tree_node: Option<usize>,
    selected_file_row: Option<usize>,
    status_message: SharedString,
}

impl A11yExample {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        Self {
            focus_handle,
            dark_mode: false,
            notifications_enabled: true,
            auto_save: false,
            selected_tab: 0,
            progress: 0.65,
            expanded_tree_nodes: vec![true, true, false],
            selected_tree_node: None,
            selected_file_row: None,
            status_message: "Welcome! This demo showcases GPUI accessibility features.".into(),
        }
    }

    fn on_tab(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn on_tab_prev(&mut self, _: &TabPrev, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev(cx);
    }

    fn bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x1e1e2e).into()
        } else {
            rgb(0xf5f5f5).into()
        }
    }

    fn fg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0xcdd6f4).into()
        } else {
            rgb(0x1e1e2e).into()
        }
    }

    fn subtle(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x45475a).into()
        } else {
            rgb(0xd0d0d0).into()
        }
    }

    fn surface(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x313244).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    fn accent(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x89b4fa).into()
        } else {
            rgb(0x1a73e8).into()
        }
    }

    fn accent_fg(&self) -> gpui::Hsla {
        rgb(0xffffff).into()
    }

    fn success(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0xa6e3a1).into()
        } else {
            rgb(0x2e7d32).into()
        }
    }

    // --- Section builders ---

    fn render_heading(&self, text: &str) -> impl IntoElement {
        div()
            .text_lg()
            .font_weight(gpui::FontWeight::BOLD)
            .text_color(self.fg())
            .mb_1()
            .child(text.to_string())
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = ["Overview", "Settings", "Data"];
        let selected = self.selected_tab;

        div()
            .id("tab-bar")
            .role(Role::TabList)
            .aria_label("Main sections")
            .aria_orientation(Orientation::Horizontal)
            .flex()
            .flex_row()
            .gap_1()
            .mb_4()
            .children(tabs.iter().enumerate().map(|(index, label)| {
                let is_selected = index == selected;
                div()
                    .id(("tab", index))
                    .role(Role::Tab)
                    .aria_label(SharedString::from(*label))
                    .aria_selected(is_selected)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(tabs.len())
                    .px_4()
                    .py_1()
                    .cursor_pointer()
                    .rounded_t_md()
                    .font_weight(if is_selected {
                        gpui::FontWeight::BOLD
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(if is_selected {
                        self.accent()
                    } else {
                        self.fg()
                    })
                    .border_b_2()
                    .border_color(if is_selected {
                        self.accent()
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|s| s.bg(self.subtle().opacity(0.3)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_tab = index;
                        this.status_message =
                            SharedString::from(format!("Switched to {} tab.", tabs[index]));
                        cx.notify();
                    }))
                    .child(label.to_string())
            }))
    }

    fn render_overview_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("overview-panel")
            .role(Role::TabPanel)
            .aria_label("Overview")
            .flex()
            .flex_col()
            .gap_4()
            .child(self.render_heading("Buttons"))
            .child(self.render_buttons(cx))
            .child(self.render_heading("Progress"))
            .child(self.render_progress_bar(cx))
            .child(self.render_heading("File Tree"))
            .child(self.render_tree(cx))
    }

    fn render_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("button-group")
            .role(Role::Group)
            .aria_label("Actions")
            .flex()
            .flex_row()
            .gap_2()
            .child(
                div()
                    .id("btn-primary")
                    .role(Role::Button)
                    .aria_label("Run build")
                    .px_4()
                    .py_1()
                    .rounded_md()
                    .bg(self.accent())
                    .text_color(self.accent_fg())
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.85))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.status_message = "Build started!".into();
                        this.progress = 0.0;
                        cx.notify();
                    }))
                    .child("Run Build"),
            )
            .child(
                div()
                    .id("btn-increment")
                    .role(Role::Button)
                    .aria_label("Increment progress by 10%")
                    .px_4()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(self.accent())
                    .text_color(self.accent())
                    .cursor_pointer()
                    .hover(|s| s.bg(self.accent().opacity(0.1)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.progress = (this.progress + 0.1).min(1.0);
                        let pct = (this.progress * 100.0) as u32;
                        this.status_message =
                            SharedString::from(format!("Progress: {}%", pct));
                        cx.notify();
                    }))
                    .child("+10%"),
            )
            .child(
                div()
                    .id("btn-reset")
                    .role(Role::Button)
                    .aria_label("Reset progress")
                    .px_4()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(self.subtle())
                    .text_color(self.fg())
                    .cursor_pointer()
                    .hover(|s| s.bg(self.subtle().opacity(0.3)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.progress = 0.0;
                        this.status_message = "Progress reset.".into();
                        cx.notify();
                    }))
                    .child("Reset"),
            )
    }

    fn render_progress_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pct = (self.progress * 100.0) as u32;
        let bar_color = if self.progress >= 1.0 {
            self.success()
        } else {
            self.accent()
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .id("progress-bar")
                    .role(Role::ProgressIndicator)
                    .aria_label("Build progress")
                    .aria_numeric_value(self.progress * 100.0)
                    .aria_min_numeric_value(0.0)
                    .aria_max_numeric_value(100.0)
                    .h(px(12.0))
                    .w_full()
                    .rounded_full()
                    .bg(self.subtle().opacity(0.5))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(self.progress as f32))
                            .rounded_full()
                            .bg(bar_color),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(self.fg().opacity(0.7))
                    .child(format!("{}% complete", pct)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .mt_1()
                    .children((0..5).map(|index| {
                        let step_progress = (index as f64 + 1.0) * 0.2;
                        let is_done = self.progress >= step_progress;
                        div()
                            .id(("progress-step", index))
                            .role(Role::ListItem)
                            .aria_label(SharedString::from(format!("Step {}", index + 1)))
                            .aria_position_in_set(index + 1)
                            .aria_size_of_set(5)
                            .size_6()
                            .rounded_full()
                            .flex()
                            .justify_center()
                            .items_center()
                            .text_xs()
                            .bg(if is_done {
                                bar_color
                            } else {
                                self.subtle().opacity(0.5)
                            })
                            .text_color(if is_done {
                                self.accent_fg()
                            } else {
                                self.fg().opacity(0.5)
                            })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.progress = step_progress;
                                let pct = (step_progress * 100.0) as u32;
                                this.status_message =
                                    SharedString::from(format!("Progress set to {}%.", pct));
                                cx.notify();
                            }))
                            .child(format!("{}", index + 1))
                    })),
            )
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut flat_index = 0usize;

        div()
            .id("file-tree")
            .role(Role::Tree)
            .aria_label("Project files")
            .flex()
            .flex_col()
            .border_1()
            .border_color(self.subtle())
            .rounded_md()
            .p_2()
            .children(FILE_TREE.iter().enumerate().flat_map(
                |(root_index, node)| {
                    let mut items = Vec::new();
                    let current_index = flat_index;
                    let is_expanded = self
                        .expanded_tree_nodes
                        .get(root_index)
                        .copied()
                        .unwrap_or(false);
                    let is_selected = self.selected_tree_node == Some(current_index);
                    let has_children = !node.children.is_empty();

                    items.push(
                        div()
                            .id(("tree-node", current_index))
                            .role(Role::TreeItem)
                            .aria_label(SharedString::from(node.label))
                            .aria_level(node.depth)
                            .aria_selected(is_selected)
                            .aria_position_in_set(root_index + 1)
                            .aria_size_of_set(FILE_TREE.len())
                            .when(has_children, |this| this.aria_expanded(is_expanded))
                            .pl(px(node.depth as f32 * 16.0))
                            .py(px(2.0))
                            .px_2()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(self.fg())
                            .when(is_selected, |this| {
                                this.bg(self.accent().opacity(0.15))
                            })
                            .hover(|s| s.bg(self.subtle().opacity(0.3)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_tree_node = Some(current_index);
                                if has_children {
                                    if let Some(val) =
                                        this.expanded_tree_nodes.get_mut(root_index)
                                    {
                                        *val = !*val;
                                    }
                                }
                                this.status_message = SharedString::from(format!(
                                    "Selected: {}",
                                    node.label
                                ));
                                cx.notify();
                            }))
                            .child(format!(
                                "{} {}",
                                if has_children {
                                    if is_expanded {
                                        "▾"
                                    } else {
                                        "▸"
                                    }
                                } else {
                                    " "
                                },
                                node.label
                            )),
                    );
                    flat_index += 1;

                    if has_children && is_expanded {
                        for (child_index, child) in node.children.iter().enumerate() {
                            let child_flat_index = flat_index;
                            let child_is_selected =
                                self.selected_tree_node == Some(child_flat_index);

                            items.push(
                                div()
                                    .id(("tree-node", child_flat_index))
                                    .role(Role::TreeItem)
                                    .aria_label(SharedString::from(child.label))
                                    .aria_level(child.depth)
                                    .aria_selected(child_is_selected)
                                    .aria_position_in_set(child_index + 1)
                                    .aria_size_of_set(node.children.len())
                                    .pl(px(child.depth as f32 * 16.0))
                                    .py(px(2.0))
                                    .px_2()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_color(self.fg())
                                    .when(child_is_selected, |this| {
                                        this.bg(self.accent().opacity(0.15))
                                    })
                                    .hover(|s| s.bg(self.subtle().opacity(0.3)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.selected_tree_node = Some(child_flat_index);
                                        this.status_message = SharedString::from(format!(
                                            "Selected: {}",
                                            child.label
                                        ));
                                        cx.notify();
                                    }))
                                    .child(format!("  {}", child.label)),
                            );
                            flat_index += 1;
                        }
                    }

                    items
                },
            ))
    }

    fn render_settings_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-panel")
            .role(Role::TabPanel)
            .aria_label("Settings")
            .flex()
            .flex_col()
            .gap_4()
            .child(self.render_heading("Preferences"))
            .child(
                div()
                    .id("settings-group")
                    .role(Role::Group)
                    .aria_label("Application preferences")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(self.render_toggle(
                        "dark-mode",
                        "Dark mode",
                        self.dark_mode,
                        Role::Switch,
                        cx,
                        |this, _, _, cx| {
                            this.dark_mode = !this.dark_mode;
                            this.status_message = if this.dark_mode {
                                "Dark mode enabled.".into()
                            } else {
                                "Dark mode disabled.".into()
                            };
                            cx.notify();
                        },
                    ))
                    .child(self.render_toggle(
                        "notifications",
                        "Enable notifications",
                        self.notifications_enabled,
                        Role::Switch,
                        cx,
                        |this, _, _, cx| {
                            this.notifications_enabled = !this.notifications_enabled;
                            this.status_message = if this.notifications_enabled {
                                "Notifications enabled.".into()
                            } else {
                                "Notifications disabled.".into()
                            };
                            cx.notify();
                        },
                    ))
                    .child(self.render_toggle(
                        "auto-save",
                        "Auto-save files",
                        self.auto_save,
                        Role::CheckBox,
                        cx,
                        |this, _, _, cx| {
                            this.auto_save = !this.auto_save;
                            this.status_message = if this.auto_save {
                                "Auto-save enabled.".into()
                            } else {
                                "Auto-save disabled.".into()
                            };
                            cx.notify();
                        },
                    )),
            )
    }

    fn render_toggle(
        &self,
        id: &'static str,
        label: &'static str,
        value: bool,
        role: Role,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &gpui::ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let toggled = if value {
            Toggled::True
        } else {
            Toggled::False
        };

        let is_switch = role == Role::Switch;

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .id(id)
                    .role(role)
                    .aria_label(SharedString::from(label))
                    .aria_toggled(toggled)
                    .cursor_pointer()
                    .on_click(cx.listener(on_click))
                    .when(is_switch, |this| {
                        this.w(px(40.0))
                            .h(px(22.0))
                            .rounded_full()
                            .bg(if value {
                                self.accent()
                            } else {
                                self.subtle()
                            })
                            .p(px(2.0))
                            .child(
                                div()
                                    .size(px(18.0))
                                    .rounded_full()
                                    .bg(gpui::white())
                                    .when(value, |this| this.ml(px(18.0))),
                            )
                    })
                    .when(!is_switch, |this| {
                        this.size(px(18.0))
                            .rounded_sm()
                            .border_2()
                            .border_color(if value {
                                self.accent()
                            } else {
                                self.subtle()
                            })
                            .bg(if value {
                                self.accent()
                            } else {
                                gpui::transparent_black()
                            })
                            .flex()
                            .justify_center()
                            .items_center()
                            .text_xs()
                            .text_color(self.accent_fg())
                            .when(value, |this| this.child("✓"))
                    }),
            )
            .child(
                div()
                    .text_color(self.fg())
                    .child(label.to_string()),
            )
    }

    fn render_data_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let column_count = 3;
        let row_count = FILES.len();

        div()
            .id("data-panel")
            .role(Role::TabPanel)
            .aria_label("Data")
            .flex()
            .flex_col()
            .gap_4()
            .child(self.render_heading("File Table"))
            .child(
                div()
                    .id("file-table")
                    .role(Role::Table)
                    .aria_label("Project files")
                    .aria_row_count(row_count + 1)
                    .aria_column_count(column_count)
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(self.subtle())
                    .rounded_md()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("table-header")
                            .role(Role::Row)
                            .aria_row_index(1)
                            .flex()
                            .flex_row()
                            .bg(self.subtle().opacity(0.3))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(self.fg())
                            .child(self.render_cell("header-name", "Name", 1, column_count, true))
                            .child(self.render_cell("header-type", "Type", 2, column_count, true))
                            .child(self.render_cell("header-size", "Size", 3, column_count, true)),
                    )
                    .children(FILES.iter().enumerate().map(|(row_index, file)| {
                        let is_selected = self.selected_file_row == Some(row_index);

                        div()
                            .id(("table-row", row_index))
                            .role(Role::Row)
                            .aria_row_index(row_index + 2)
                            .aria_selected(is_selected)
                            .flex()
                            .flex_row()
                            .cursor_pointer()
                            .text_color(self.fg())
                            .when(is_selected, |this| {
                                this.bg(self.accent().opacity(0.15))
                            })
                            .when(row_index % 2 == 1, |this| {
                                this.bg(self.subtle().opacity(0.1))
                            })
                            .hover(|s| s.bg(self.accent().opacity(0.1)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_file_row = Some(row_index);
                                this.status_message = SharedString::from(format!(
                                    "Selected file: {}",
                                    FILES[row_index].name
                                ));
                                cx.notify();
                            }))
                            .child(self.render_cell(
                                ("cell-name", row_index),
                                file.name,
                                1,
                                column_count,
                                false,
                            ))
                            .child(self.render_cell(
                                ("cell-type", row_index),
                                file.kind,
                                2,
                                column_count,
                                false,
                            ))
                            .child(self.render_cell(
                                ("cell-size", row_index),
                                file.size,
                                3,
                                column_count,
                                false,
                            ))
                    })),
            )
            .child(self.render_heading("Item List"))
            .child(self.render_list())
    }

    fn render_cell(
        &self,
        id: impl Into<gpui::ElementId>,
        text: &str,
        column: usize,
        total_columns: usize,
        is_header: bool,
    ) -> impl IntoElement {
        div()
            .id(id.into())
            .role(if is_header {
                Role::ColumnHeader
            } else {
                Role::Cell
            })
            .aria_label(SharedString::from(text.to_string()))
            .aria_column_index(column)
            .aria_column_count(total_columns)
            .flex_1()
            .px_3()
            .py_2()
            .child(text.to_string())
    }

    fn render_list(&self) -> impl IntoElement {
        let items = ["Alpha", "Beta", "Gamma", "Delta", "Epsilon"];

        div()
            .id("demo-list")
            .role(Role::List)
            .aria_label("Greek letters")
            .flex()
            .flex_col()
            .border_1()
            .border_color(self.subtle())
            .rounded_md()
            .children(items.iter().enumerate().map(|(index, label)| {
                div()
                    .id(("list-item", index))
                    .role(Role::ListItem)
                    .aria_label(SharedString::from(*label))
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(items.len())
                    .px_3()
                    .py_1()
                    .text_color(self.fg())
                    .when(index % 2 == 1, |this| {
                        this.bg(self.subtle().opacity(0.1))
                    })
                    .child(format!("{}. {}", index + 1, label))
            }))
    }

    fn render_status_bar(&self) -> impl IntoElement {
        div()
            .id("status-bar")
            .role(Role::Status)
            .aria_label(self.status_message.clone())
            .w_full()
            .px_4()
            .py_2()
            .bg(self.subtle().opacity(0.3))
            .rounded_md()
            .text_sm()
            .text_color(self.fg().opacity(0.8))
            .child(self.status_message.clone())
    }
}

impl Render for A11yExample {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_content: gpui::AnyElement = match self.selected_tab {
            0 => self.render_overview_panel(cx).into_any_element(),
            1 => self.render_settings_panel(cx).into_any_element(),
            2 => self.render_data_panel(cx).into_any_element(),
            _ => div().child("Unknown tab").into_any_element(),
        };

        div()
            .id("app-root")
            .role(Role::Application)
            .aria_label("Accessibility Demo")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_tab_prev))
            .size_full()
            .flex()
            .flex_col()
            .bg(self.bg())
            .font_family("sans-serif")
            .child(
                div()
                    .id("header")
                    .role(Role::Banner)
                    .aria_label("Application header")
                    .w_full()
                    .px_6()
                    .py_3()
                    .bg(self.surface())
                    .border_b_1()
                    .border_color(self.subtle())
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(self.accent())
                                    .child("♿"),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(self.fg())
                                    .child("GPUI Accessibility Demo"),
                            ),
                    )
                    .child(
                        div()
                            .id("theme-toggle")
                            .role(Role::Button)
                            .aria_label(if self.dark_mode {
                                "Switch to light mode"
                            } else {
                                "Switch to dark mode"
                            })
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .border_1()
                            .border_color(self.subtle())
                            .text_color(self.fg())
                            .hover(|s| s.bg(self.subtle().opacity(0.3)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dark_mode = !this.dark_mode;
                                this.status_message = if this.dark_mode {
                                    "Dark mode enabled.".into()
                                } else {
                                    "Dark mode disabled.".into()
                                };
                                cx.notify();
                            }))
                            .child(if self.dark_mode { "☀ Light" } else { "🌙 Dark" }),
                    ),
            )
            .child(
                div()
                    .id("main-content")
                    .role(Role::Main)
                    .aria_label("Main content")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_6()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(self.render_tab_bar(cx))
                    .child(tab_content),
            )
            .child(
                div()
                    .id("footer")
                    .role(Role::ContentInfo)
                    .aria_label("Status")
                    .px_6()
                    .py_2()
                    .border_t_1()
                    .border_color(self.subtle())
                    .child(self.render_status_bar()),
            )
    }
}

fn run_example() {
    application().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("tab", Tab, None),
            KeyBinding::new("shift-tab", TabPrev, None),
        ]);

        let bounds = Bounds::centered(None, size(px(800.), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| A11yExample::new(window, cx)),
        )
        .unwrap();

        cx.activate(true);
    });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    run_example();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run_example();
}
