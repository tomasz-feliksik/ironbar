use super::open_state::OpenState;
use crate::channels::AsyncSenderExt;
use crate::clients::compositor::WorkspaceWindow;
use crate::image;
use crate::image::IconButton;
use crate::modules::workspaces::{WorkspaceItemContext, WorkspaceMessage};
use glib::signal::SignalHandlerId;
use gtk::Button as GtkButton;
use gtk::prelude::*;
use gtk::{ContentFit, Orientation, Picture};
use std::collections::HashSet;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct Button {
    button: IconButton,
    workspace_id: i64,
    monitor: String,
    open_state: OpenState,
    conn_id: Option<SignalHandlerId>,
    tx: mpsc::Sender<WorkspaceMessage>,
    /// Container for per-window icons, present only when `show_window_icons`.
    windows_box: Option<gtk::Box>,
    image_provider: image::Provider,
    window_icon_size: i32,
    dedupe_window_icons: bool,
}

impl Button {
    pub fn new(
        id: i64,
        index: i64,
        name: &str,
        monitor: &str,
        open_state: OpenState,
        context: &WorkspaceItemContext,
    ) -> Self {
        let label = context.format_label(name, index);

        let button = IconButton::new(&label, context.icon_size, &context.image_provider);
        button.set_widget_name(name);
        button.add_css_class("item");

        let tx = context.tx.clone();

        let conn_id = button.connect_clicked(move |_item| {
            tx.send_spawn(WorkspaceMessage::FocusWorkspace(id));
        });

        // When window icons are enabled, rebuild the button child as
        // [label][windows] so per-window icons sit beside the workspace label.
        // (Incompatible with `name_map` image labels, which are uncommon
        // alongside window icons.)
        let windows_box = if context.show_window_icons {
            let wb = gtk::Box::new(Orientation::Horizontal, 0);
            wb.add_css_class("workspace-windows");

            let label = button.label().clone();
            button.set_child(None::<&gtk::Widget>);

            let content = gtk::Box::new(Orientation::Horizontal, 4);
            content.add_css_class("workspace-content");
            content.append(&label);
            content.append(&wb);
            button.set_child(Some(&content));

            Some(wb)
        } else {
            None
        };

        let btn = Self {
            button,
            workspace_id: id,
            monitor: monitor.to_string(),
            open_state,
            conn_id: Some(conn_id),
            tx: context.tx.clone(),
            windows_box,
            image_provider: context.image_provider.clone(),
            window_icon_size: context.window_icon_size,
            dedupe_window_icons: context.dedupe_window_icons,
        };

        btn.apply_open_state();
        btn
    }

    /// Replaces the per-window icons shown inside the workspace button.
    ///
    /// A no-op unless `show_window_icons` was enabled (i.e. `windows_box` is
    /// present). Optionally deduplicates by application id.
    pub fn set_windows(&self, windows: &[WorkspaceWindow]) {
        let Some(windows_box) = self.windows_box.as_ref() else {
            return;
        };

        while let Some(child) = windows_box.first_child() {
            windows_box.remove(&child);
        }

        // App ids that have a focused window — so a deduped icon still
        // highlights when any of that app's windows is the active one.
        let focused_apps: HashSet<&str> = windows
            .iter()
            .filter(|w| w.focused)
            .map(|w| w.app_id.as_str())
            .collect();

        let mut seen = HashSet::new();
        for window in windows {
            if self.dedupe_window_icons && !seen.insert(window.app_id.as_str()) {
                continue;
            }

            let picture = Picture::builder().content_fit(ContentFit::ScaleDown).build();
            picture.add_css_class("window-icon");
            picture.set_size_request(self.window_icon_size, self.window_icon_size);
            // Highlight the focused window (or, when deduped, the app that owns it).
            let is_focused = if self.dedupe_window_icons {
                focused_apps.contains(window.app_id.as_str())
            } else {
                window.focused
            };
            if is_focused {
                picture.add_css_class("focused");
            }

            // Click the icon to focus that window. A GestureClick (rather than a
            // nested Button, which GTK disallows inside the workspace Button)
            // that claims the sequence so the parent workspace button's click
            // does not also fire.
            let gesture = gtk::GestureClick::new();
            let tx = self.tx.clone();
            let window_id = window.id.clone();
            let workspace_id = self.workspace_id;
            gesture.connect_pressed(move |gesture, _, _, _| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                tx.send_spawn(WorkspaceMessage::FocusWindow {
                    workspace_id,
                    address: window_id.clone(),
                });
            });
            picture.add_controller(gesture);

            windows_box.append(&picture);

            let provider = self.image_provider.clone();
            let app_id = window.app_id.clone();
            let size = self.window_icon_size;
            let pic = picture.clone();
            glib::spawn_future_local(async move {
                provider
                    .load_into_picture_silent(&app_id, size, true, &pic)
                    .await;
            });
        }
    }

    pub fn button(&self) -> &GtkButton {
        &self.button
    }

    pub fn set_label(&self, label: &str) {
        self.button.set_label(label);
    }

    pub fn open_state(&self) -> OpenState {
        self.open_state
    }

    pub fn set_open_state(&mut self, open_state: OpenState) {
        if self.open_state == open_state {
            return;
        }
        self.open_state = open_state;
        self.apply_open_state();
    }

    fn apply_open_state(&self) {
        let open_state = self.open_state;

        if open_state.is_visible() {
            self.button.add_css_class("visible");
        } else {
            self.button.remove_css_class("visible");
        }

        if open_state == OpenState::Focused {
            self.button.add_css_class("focused");
        } else {
            self.button.remove_css_class("focused");
        }

        if open_state == OpenState::Closed {
            self.button.add_css_class("inactive");
        } else {
            self.button.remove_css_class("inactive");
        }
    }

    pub fn set_urgent(&self, urgent: bool) {
        if urgent {
            self.button.add_css_class("urgent");
        } else {
            self.button.remove_css_class("urgent");
        }
    }

    pub fn workspace_id(&self) -> i64 {
        self.workspace_id
    }

    pub fn set_workspace_id(&mut self, id: i64) {
        self.workspace_id = id;
        if let Some(conn_id) = self.conn_id.take() {
            self.button.disconnect(conn_id);
        }
        let tx = self.tx.clone();
        let conn_id = self.button.connect_clicked(move |_item| {
            tx.send_spawn(WorkspaceMessage::FocusWorkspace(id));
        });
        self.conn_id = Some(conn_id);
    }

    pub fn monitor(&self) -> &str {
        &self.monitor
    }

    pub fn set_monitor(&mut self, monitor: &str) {
        self.monitor = monitor.to_string();
    }
}
