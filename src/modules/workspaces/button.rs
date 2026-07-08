use super::open_state::OpenState;
use crate::channels::AsyncSenderExt;
use crate::clients::compositor::WorkspaceWindow;
use crate::image;
use crate::image::IconButton;
use crate::modules::workspaces::{WorkspaceItemContext, WorkspaceMessage};
use glib::signal::SignalHandlerId;
use gtk::Button as GtkButton;
use gtk::prelude::*;
use gtk::{Align, ContentFit, Orientation, Picture};
use std::collections::HashMap;
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

        for icon in resolve_window_icons(windows, self.dedupe_window_icons) {
            let picture = Picture::builder()
                .content_fit(ContentFit::ScaleDown)
                // Don't shrink below the requested size: on a narrow bar with
                // many icons GTK would otherwise compress each Picture down to
                // a few pixels ("dots"). Keeps icons full size on every monitor.
                .can_shrink(false)
                .build();
            picture.add_css_class("window-icon");
            picture.set_size_request(self.window_icon_size, self.window_icon_size);
            // Center rather than fill so the box doesn't stretch the icon (and
            // its background tile) vertically — keeps the tile square.
            picture.set_valign(Align::Center);
            picture.set_halign(Align::Center);
            if icon.focused {
                picture.add_css_class("focused");
            }

            // Click the icon to focus that window. A GestureClick (rather than a
            // nested Button, which GTK disallows inside the workspace Button)
            // that claims the sequence so the parent workspace button's click
            // does not also fire.
            let gesture = gtk::GestureClick::new();
            let tx = self.tx.clone();
            let address = icon.address.clone();
            let workspace_id = self.workspace_id;
            gesture.connect_pressed(move |gesture, _, _, _| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                tx.send_spawn(WorkspaceMessage::FocusWindow {
                    workspace_id,
                    address: address.clone(),
                });
            });
            picture.add_controller(gesture);

            windows_box.append(&picture);

            let provider = self.image_provider.clone();
            let app_id = icon.app_id.clone();
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

/// A window icon to render inside a workspace button.
#[derive(Debug, PartialEq, Eq)]
struct ResolvedIcon {
    app_id: String,
    /// Address of the window to focus when the icon is clicked. When deduped,
    /// this is the first window of the app.
    address: String,
    focused: bool,
}

/// Resolves the window icons to render for a workspace.
///
/// Without dedupe: one icon per window, in order, each with its own focus state
/// and address. With dedupe: one icon per distinct app id (first occurrence
/// wins for order and click target), focused if ANY window of that app is
/// focused.
fn resolve_window_icons(windows: &[WorkspaceWindow], dedupe: bool) -> Vec<ResolvedIcon> {
    if !dedupe {
        return windows
            .iter()
            .map(|w| ResolvedIcon {
                app_id: w.app_id.clone(),
                address: w.id.clone(),
                focused: w.focused,
            })
            .collect();
    }

    let mut icons: Vec<ResolvedIcon> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for w in windows {
        if let Some(&i) = index.get(w.app_id.as_str()) {
            if w.focused {
                icons[i].focused = true;
            }
        } else {
            index.insert(w.app_id.as_str(), icons.len());
            icons.push(ResolvedIcon {
                app_id: w.app_id.clone(),
                address: w.id.clone(),
                focused: w.focused,
            });
        }
    }
    icons
}

#[cfg(test)]
mod tests {
    use super::resolve_window_icons;
    use crate::clients::compositor::WorkspaceWindow;

    fn win(app: &str, addr: &str, focused: bool) -> WorkspaceWindow {
        WorkspaceWindow {
            id: addr.to_string(),
            app_id: app.to_string(),
            focused,
        }
    }

    #[test]
    fn no_dedupe_keeps_every_window_in_order() {
        let windows = vec![
            win("firefox", "0x1", false),
            win("firefox", "0x2", true),
            win("kitty", "0x3", false),
        ];
        let icons = resolve_window_icons(&windows, false);
        assert_eq!(icons.len(), 3);
        assert_eq!(icons[0].address, "0x1");
        assert!(icons[1].focused);
        assert_eq!(icons[2].app_id, "kitty");
    }

    #[test]
    fn dedupe_collapses_apps_keeps_first_address_and_any_focus() {
        let windows = vec![
            win("firefox", "0x1", false),
            win("firefox", "0x2", true),
            win("kitty", "0x3", false),
        ];
        let icons = resolve_window_icons(&windows, true);
        assert_eq!(icons.len(), 2);
        // First firefox window is the click target,
        assert_eq!(icons[0].app_id, "firefox");
        assert_eq!(icons[0].address, "0x1");
        // but it is focused because a later firefox window is focused.
        assert!(icons[0].focused);
        assert_eq!(icons[1].app_id, "kitty");
        assert!(!icons[1].focused);
    }
}
