#[cfg(feature = "workspaces+hyprland")]
use super::WorkspaceWindow;
#[cfg(feature = "bindmode+hyprland")]
use super::{BindModeClient, BindModeUpdate};
#[cfg(feature = "keyboard+hyprland")]
use super::{KeyboardLayoutClient, KeyboardLayoutUpdate};
use super::{Visibility, Workspace};
use crate::channels::SyncSenderExt;
use crate::{arc_mut, lock, spawn_blocking};
use hyprland::Result;
use hyprland::ctl::switch_xkb_layout;
use hyprland::data::{Devices, Workspace as HWorkspace, Workspaces};
use hyprland::dispatch::{
    Dispatch, DispatchType, WindowIdentifier, WorkspaceIdentifierWithSpecial,
};
use hyprland::event_listener::EventListener;
use hyprland::prelude::*;
use hyprland::shared::Address;
use hyprland::shared::{HyprDataVec, WorkspaceType};
#[cfg(feature = "workspaces+hyprland")]
use serde::Deserialize;
#[cfg(feature = "workspaces+hyprland")]
use std::io::{Read, Write};
#[cfg(feature = "workspaces+hyprland")]
use std::os::unix::net::UnixStream;
use tokio::sync::broadcast::{Receiver, Sender, channel};
use tracing::{debug, error, info, warn};

#[cfg(feature = "workspaces")]
use super::WorkspaceUpdate;

#[derive(Debug)]
struct TxRx<T> {
    tx: Sender<T>,
    _rx: Receiver<T>,
}
impl<T: Clone> TxRx<T> {
    fn new() -> Self {
        // Broadcast capacity. This is slack for scheduler jitter, NOT the
        // correctness guarantee: a `broadcast` channel drops the oldest messages
        // under lag, and for the workspaces module a dropped `Windows` snapshot
        // leaves a button's icons — and the addresses their click closures
        // captured — stale. What actually bounds the risk is that each
        // compositor event now costs a *single* message: focus changes send one
        // lightweight `WindowFocusChanged` rather than re-snapshotting every
        // workspace (see `add_active_window_changed_handler`), and a full
        // snapshot is one atomic `Windows` message rather than one per workspace
        // (see `send_window_snapshot`). A subscriber must therefore fall dozens
        // of distinct events behind to lag at all. If `Channel lagged` warnings
        // ever reappear, the fix is not a bigger buffer (it only widens the
        // window) but resync-on-lag — re-requesting a snapshot on `Lagged`,
        // since the module's state is a pure function of the latest snapshot.
        // See beads ironbar-cvz.5.
        let (tx, rx) = channel(32);
        Self { tx, _rx: rx }
    }
}

#[derive(Debug)]
pub struct Client {
    #[cfg(feature = "workspaces+hyprland")]
    workspace: TxRx<WorkspaceUpdate>,

    #[cfg(feature = "workspaces+hyprland")]
    use_lua_dispatch: bool,

    #[cfg(feature = "keyboard+hyprland")]
    keyboard_layout: TxRx<KeyboardLayoutUpdate>,

    #[cfg(feature = "bindmode+hyprland")]
    bindmode: TxRx<BindModeUpdate>,
}

impl Client {
    pub(crate) fn new() -> Self {
        let instance = Self {
            #[cfg(feature = "workspaces+hyprland")]
            workspace: TxRx::new(),
            #[cfg(feature = "workspaces+hyprland")]
            use_lua_dispatch: detect_lua_config(),
            #[cfg(feature = "keyboard+hyprland")]
            keyboard_layout: TxRx::new(),
            #[cfg(feature = "bindmode+hyprland")]
            bindmode: TxRx::new(),
        };

        instance.listen_events();
        instance
    }

    fn listen_events(&self) {
        info!("Starting Hyprland event listener");

        #[cfg(feature = "workspaces+hyprland")]
        let workspace_tx = self.workspace.tx.clone();

        #[cfg(feature = "keyboard+hyprland")]
        let keyboard_layout_tx = self.keyboard_layout.tx.clone();

        #[cfg(feature = "bindmode+hyprland")]
        let bindmode_tx = self.bindmode.tx.clone();

        spawn_blocking(move || {
            let mut event_listener = EventListener::new();

            // we need a lock to ensure events don't run at the same time
            let lock = arc_mut!(());

            // cache the active workspace since Hyprland doesn't give us the prev active
            #[cfg(feature = "workspaces+hyprland")]
            Self::listen_workspace_events(&workspace_tx, &mut event_listener, &lock);

            #[cfg(feature = "keyboard+hyprland")]
            Self::listen_keyboard_events(&keyboard_layout_tx, &mut event_listener, &lock);

            #[cfg(feature = "bindmode+hyprland")]
            Self::listen_bindmode_events(&bindmode_tx, &mut event_listener, &lock);

            if let Err(err) = event_listener.start_listener() {
                error!("Failed to start listener: {err:#}");
            }
        });
    }

    #[cfg(feature = "workspaces+hyprland")]
    fn listen_workspace_events(
        tx: &Sender<WorkspaceUpdate>,
        event_listener: &mut EventListener,
        lock: &std::sync::Arc<std::sync::Mutex<()>>,
    ) {
        let active = Self::get_active_workspace().map_or_else(
            |err| {
                error!("Failed to get active workspace: {err:#?}");
                None
            },
            Some,
        );
        let active = arc_mut!(active);

        {
            let tx = tx.clone();
            let lock = lock.clone();
            let active = active.clone();

            event_listener.add_workspace_added_handler(move |event| {
                let _lock = lock!(lock);
                debug!("Added workspace: {event:?}");

                let workspace_name = get_workspace_name(event.name);
                let prev_workspace = lock!(active);

                let workspace = Self::get_workspace(&workspace_name, prev_workspace.as_ref());

                match workspace {
                    Ok(Some(workspace)) => {
                        tx.send_expect(WorkspaceUpdate::Add(workspace));
                    }
                    Err(e) => error!("Failed to get workspace: {e:#}"),
                    _ => {}
                }
            });
        }

        {
            let tx = tx.clone();
            let lock = lock.clone();
            let active = active.clone();

            event_listener.add_workspace_changed_handler(move |event| {
                let _lock = lock!(lock);

                let mut prev_workspace = lock!(active);

                debug!(
                    "Received workspace change: {:?} -> {event:?}",
                    prev_workspace.as_ref().map(|w| &w.id)
                );

                let workspace_name = get_workspace_name(event.name);
                let workspace = Self::get_workspace(&workspace_name, prev_workspace.as_ref());

                match workspace {
                    Ok(Some(workspace)) if !workspace.visibility.is_focused() => {
                        Self::send_focus_change(&mut prev_workspace, workspace, &tx);
                    }
                    Ok(None) => {
                        error!("Unable to locate workspace");
                    }
                    Err(e) => error!("Failed to get workspace: {e:#}"),
                    _ => {}
                }
            });
        }

        {
            let tx = tx.clone();
            let lock = lock.clone();
            let active = active.clone();

            event_listener.add_active_monitor_changed_handler(move |event_data| {
                let _lock = lock!(lock);
                let Some(workspace_type) = event_data.workspace_name else {
                    warn!("Received active monitor change with no workspace name");
                    return;
                };

                let mut prev_workspace = lock!(active);

                debug!(
                    "Received active monitor change: {:?} -> {workspace_type:?}",
                    prev_workspace.as_ref().map(|w| &w.name)
                );

                let workspace_name = get_workspace_name(workspace_type);
                let workspace = Self::get_workspace(&workspace_name, prev_workspace.as_ref());

                match workspace {
                    Ok(Some(workspace)) if !workspace.visibility.is_focused() => {
                        Self::send_focus_change(&mut prev_workspace, workspace, &tx);
                    }
                    Ok(None) => {
                        error!("Unable to locate workspace");
                    }
                    Err(e) => error!("Failed to get workspace: {e:#}"),
                    _ => {}
                }
            });
        }

        {
            let tx = tx.clone();
            let lock = lock.clone();

            event_listener.add_workspace_moved_handler(move |event_data| {
                let _lock = lock!(lock);
                let workspace_type = event_data.name;

                let mut prev_workspace = lock!(active);
                debug!(
                    "Received workspace move: {:?} -> {workspace_type:?}",
                    prev_workspace.as_ref().map(|w| &w.name)
                );

                let workspace_name = get_workspace_name(workspace_type);
                let workspace = Self::get_workspace(&workspace_name, prev_workspace.as_ref());

                match workspace {
                    Ok(Some(workspace)) => {
                        tx.send_expect(WorkspaceUpdate::Move(workspace.clone()));
                        if !workspace.visibility.is_focused() {
                            Self::send_focus_change(&mut prev_workspace, workspace, &tx);
                        }
                    }
                    Ok(None) => {
                        error!("Unable to locate workspace");
                    }
                    Err(e) => error!("Failed to get workspace: {e:#}"),
                }
            });
        }

        {
            let tx = tx.clone();
            let lock = lock.clone();

            event_listener.add_workspace_renamed_handler(move |data| {
                let _lock = lock!(lock);
                debug!("Received workspace rename: {data:?}");

                tx.send_expect(WorkspaceUpdate::Rename {
                    id: data.id as i64,
                    name: data.name,
                });
            });
        }

        {
            let tx = tx.clone();
            let lock = lock.clone();

            event_listener.add_workspace_deleted_handler(move |data| {
                let _lock = lock!(lock);
                debug!("Received workspace destroy: {data:?}");
                tx.send_expect(WorkspaceUpdate::Remove(data.id as i64));
            });
        }

        {
            let tx = tx.clone();
            let lock = lock.clone();

            event_listener.add_urgent_state_changed_handler(move |address| {
                let _lock = lock!(lock);
                debug!("Received urgent state: {address:?}");

                let clients = match hyprland::data::Clients::get() {
                    Ok(clients) => clients,
                    Err(err) => {
                        error!("Failed to get clients: {err}");
                        return;
                    }
                };
                clients.iter().find(|c| c.address == address).map_or_else(
                    || {
                        error!("Unable to locate client");
                    },
                    |c| {
                        tx.send_expect(WorkspaceUpdate::Urgent {
                            id: c.workspace.id as i64,
                            urgent: true,
                        });
                    },
                );
            });
        }

        // Window open/close/move: re-snapshot windows-per-workspace so the
        // workspaces module can refresh per-workspace icons. A no-op for the
        // module unless `show_window_icons` is enabled.
        {
            let tx = tx.clone();
            let lock = lock.clone();
            event_listener.add_window_opened_handler(move |data| {
                let _lock = lock!(lock);
                debug!("Received window open: {data:?}");
                Self::send_window_snapshot(&tx);
            });
        }

        {
            let tx = tx.clone();
            let lock = lock.clone();
            event_listener.add_window_closed_handler(move |address| {
                let _lock = lock!(lock);
                debug!("Received window close: {address:?}");
                Self::send_window_snapshot(&tx);
            });
        }

        {
            let tx = tx.clone();
            let lock = lock.clone();
            event_listener.add_window_moved_handler(move |data| {
                let _lock = lock!(lock);
                debug!("Received window move: {data:?}");
                Self::send_window_snapshot(&tx);
            });
        }

        // Active-window change: emit a lightweight focus update so the icon
        // highlight follows focus. A focus change never alters which windows
        // sit on which workspace — only which is highlighted — so a full
        // re-snapshot (2 IPC calls + one broadcast message per workspace) would
        // be the dominant, and least justified, source of channel traffic, and
        // clicking an icon itself triggers a focus change (a self-reinforcing
        // loop). The event already carries the new active address, so this
        // costs no IPC. See beads ironbar-cvz.3.
        {
            let tx = tx.clone();
            let lock = lock.clone();
            event_listener.add_active_window_changed_handler(move |data| {
                let _lock = lock!(lock);
                debug!("Received active window change: {data:?}");
                let address = data.map(|d| d.address.to_string());
                tx.send_expect(WorkspaceUpdate::WindowFocusChanged { address });
            });
        }
    }

    /// Snapshots all open windows, buckets them by workspace, and emits a single
    /// atomic [`WorkspaceUpdate::Windows`] event covering every current
    /// workspace.
    ///
    /// Empty workspaces get an empty vec so their icons clear. Two IPC calls
    /// per invocation — acceptable at window-event frequency for a status bar.
    #[cfg(feature = "workspaces+hyprland")]
    fn send_window_snapshot(tx: &Sender<WorkspaceUpdate>) {
        use std::collections::HashMap;

        let clients = match hyprland::data::Clients::get() {
            Ok(clients) => clients,
            Err(err) => {
                error!("Failed to get clients for window snapshot: {err}");
                return;
            }
        };

        // The currently focused window, so its icon can be highlighted.
        let active_address = hyprland::data::Client::get_active()
            .ok()
            .flatten()
            .map(|c| c.address.to_string());

        let mut by_workspace: HashMap<i64, Vec<WorkspaceWindow>> = HashMap::new();
        for client in clients.iter() {
            let id = client.address.to_string();
            let focused = active_address.as_deref() == Some(id.as_str());
            by_workspace
                .entry(client.workspace.id as i64)
                .or_default()
                .push(WorkspaceWindow {
                    id,
                    app_id: client.class.clone(),
                    focused,
                });
        }

        match Workspaces::get() {
            Ok(workspaces) => {
                let snapshot = workspaces
                    .into_iter()
                    .map(|workspace| {
                        let id = workspace.id as i64;
                        let windows = by_workspace.remove(&id).unwrap_or_default();
                        (id, windows)
                    })
                    .collect();
                tx.send_expect(WorkspaceUpdate::Windows(snapshot));
            }
            Err(err) => error!("Failed to get workspaces for window snapshot: {err}"),
        }
    }

    #[cfg(feature = "keyboard+hyprland")]
    fn listen_keyboard_events(
        keyboard_layout_tx: &Sender<KeyboardLayoutUpdate>,
        event_listener: &mut EventListener,
        lock: &std::sync::Arc<std::sync::Mutex<()>>,
    ) {
        let tx = keyboard_layout_tx.clone();
        let lock = lock.clone();

        event_listener.add_layout_changed_handler(move |layout_event| {
            let _lock = lock!(lock);

            let layout = if layout_event.layout_name.is_empty() {
                // FIXME: This field is empty due to bug in `hyprland-rs_0.4.0-alpha.3`. Which is already fixed in last betas

                // The layout may be empty due to a bug in `hyprland-rs`, because of which the `layout_event` is incorrect.
                //
                // Instead of:
                // ```
                // LayoutEvent {
                //     keyboard_name: "keychron-keychron-c2",
                //     layout_name: "English (US)",
                // }
                // ```
                //
                // We get:
                // ```
                // LayoutEvent {
                //     keyboard_name: "keychron-keychron-c2,English (US)",
                //     layout_name: "",
                // }
                // ```
                // 
                // Here we are trying to recover `layout_name` from `keyboard_name`

                let layout = layout_event.keyboard_name.as_str().split(',').nth(1);
                let Some(layout) = layout else {
                    error!(
                        "Failed to get layout from string: {}. The failed logic is a workaround for a bug in `hyprland 0.4.0-alpha.3`", layout_event.keyboard_name);
                    return;
                };

                layout.into()
            }
            else {
                layout_event.layout_name
            };

            debug!("Received layout: {layout:?}");
            tx.send_expect(KeyboardLayoutUpdate(layout));
        });
    }

    #[cfg(feature = "bindmode+hyprland")]
    fn listen_bindmode_events(
        bindmode_tx: &Sender<BindModeUpdate>,
        event_listener: &mut EventListener,
        lock: &std::sync::Arc<std::sync::Mutex<()>>,
    ) {
        let tx = bindmode_tx.clone();
        let lock = lock.clone();

        event_listener.add_sub_map_changed_handler(move |bind_mode| {
            let _lock = lock!(lock);
            debug!("Received bind mode: {bind_mode:?}");

            tx.send_expect(BindModeUpdate {
                name: bind_mode,
                pango_markup: false,
            });
        });
    }

    /// Sends a `WorkspaceUpdate::Focus` event
    /// and updates the active workspace cache.
    #[cfg(feature = "workspaces+hyprland")]
    fn send_focus_change(
        prev_workspace: &mut Option<Workspace>,
        workspace: Workspace,
        tx: &Sender<WorkspaceUpdate>,
    ) {
        tx.send_expect(WorkspaceUpdate::Focus {
            old: prev_workspace.take(),
            new: workspace.clone(),
        });

        tx.send_expect(WorkspaceUpdate::Urgent {
            id: workspace.id,
            urgent: false,
        });

        prev_workspace.replace(workspace);
    }

    /// Gets a workspace by name from the server, given the active workspace if known.
    #[cfg(feature = "workspaces+hyprland")]
    fn get_workspace(name: &str, active: Option<&Workspace>) -> Result<Option<Workspace>> {
        let workspace = Workspaces::get()?.into_iter().find_map(|w| {
            if w.name == name {
                let vis = Visibility::from((&w, active.map(|w| w.name.as_ref()), &|w| {
                    create_is_visible()(w)
                }));

                Some(Workspace::from((vis, w)))
            } else {
                None
            }
        });

        Ok(workspace)
    }

    /// Gets the active workspace from the server.
    fn get_active_workspace() -> Result<Workspace> {
        let w = HWorkspace::get_active().map(|w| Workspace::from((Visibility::focused(), w)))?;
        Ok(w)
    }
}

#[cfg(feature = "workspaces+hyprland")]
impl super::WorkspaceClient for Client {
    fn focus(&self, id: i64) {
        let res = if self.use_lua_dispatch {
            let arg = format!("{{workspace=\"{id}\"}}");
            Dispatch::call(DispatchType::Custom("hl.dsp.focus", &arg))
        } else {
            let identifier = WorkspaceIdentifierWithSpecial::Id(id as i32);
            Dispatch::call(DispatchType::Workspace(identifier))
        };

        if let Err(e) = res {
            error!("Couldn't focus workspace '{id}': {e:#}");
        }
    }

    fn focus_window(&self, workspace_id: i64, id: String) {
        // Bring the window's workspace onto the monitor the icon was clicked on
        // FIRST, then focus the window — focusing the window alone ignores the
        // current monitor and jumps to the window's home monitor.
        let res = if self.use_lua_dispatch {
            Dispatch::call(DispatchType::Custom(
                "hl.dsp.focus",
                &focus_workspace_here_arg(workspace_id),
            ))
            .and_then(|()| {
                Dispatch::call(DispatchType::Custom("hl.dsp.focus", &focus_window_arg(&id)))
            })
        } else {
            let workspace = workspace_id.to_string();
            Dispatch::call(DispatchType::Custom(
                "focusworkspaceoncurrentmonitor",
                &workspace,
            ))
            .and_then(|()| {
                Dispatch::call(DispatchType::FocusWindow(WindowIdentifier::Address(
                    Address::new(id.clone()),
                )))
            })
        };

        if let Err(e) = res {
            error!("Couldn't focus window '{id}' on workspace '{workspace_id}': {e:#}");
        }
    }

    fn subscribe(&self) -> Receiver<WorkspaceUpdate> {
        let rx = self.workspace.tx.subscribe();

        let active_id = HWorkspace::get_active().ok().map(|active| active.name);
        let is_visible = create_is_visible();

        match Workspaces::get() {
            Ok(workspaces) => {
                let workspaces = workspaces
                    .into_iter()
                    .map(|w| {
                        let vis = Visibility::from((&w, active_id.as_deref(), &is_visible));
                        Workspace::from((vis, w))
                    })
                    .collect();

                self.workspace
                    .tx
                    .send_expect(WorkspaceUpdate::Init(workspaces));

                // Seed the initial per-workspace window icons.
                Self::send_window_snapshot(&self.workspace.tx);
            }
            Err(e) => {
                error!("Failed to get workspaces: {e:#}");
            }
        }

        rx
    }
}

#[cfg(feature = "keyboard+hyprland")]
impl KeyboardLayoutClient for Client {
    fn set_next_active(&self) {
        let Ok(devices) = Devices::get() else {
            error!("Failed to get devices");
            return;
        };

        let device = devices
            .keyboards
            .iter()
            .find(|k| k.main)
            .map(|k| k.name.clone());

        if let Some(device) = device {
            if let Err(e) =
                switch_xkb_layout::call(device, switch_xkb_layout::SwitchXKBLayoutCmdTypes::Next)
            {
                error!("Failed to switch keyboard layout due to Hyprland error: {e}");
            }
        } else {
            error!("Failed to get keyboard device from hyprland");
        }
    }

    fn subscribe(&self) -> Receiver<KeyboardLayoutUpdate> {
        let rx = self.keyboard_layout.tx.subscribe();

        match Devices::get().map(|devices| {
            devices
                .keyboards
                .iter()
                .find(|k| k.main)
                .map(|k| k.active_keymap.clone())
        }) {
            Ok(Some(layout)) => {
                self.keyboard_layout
                    .tx
                    .send_expect(KeyboardLayoutUpdate(layout));
            }
            Ok(None) => error!("Failed to get current keyboard layout hyprland"),
            Err(err) => error!("Failed to get devices: {err:#?}"),
        }

        rx
    }
}

#[cfg(feature = "bindmode+hyprland")]
impl BindModeClient for Client {
    fn subscribe(&self) -> super::Result<Receiver<BindModeUpdate>> {
        Ok(self.bindmode.tx.subscribe())
    }
}

/// Builds the `hl.dsp.focus` argument that pulls a workspace onto the *current*
/// monitor, the Lua-config-provider equivalent of the legacy
/// `focusworkspaceoncurrentmonitor` dispatcher.
#[cfg(feature = "workspaces+hyprland")]
fn focus_workspace_here_arg(workspace_id: i64) -> String {
    format!("{{workspace=\"{workspace_id}\",on_current_monitor=true}}")
}

/// Builds the `hl.dsp.focus` argument that focuses a window by address, the
/// Lua-config-provider equivalent of the legacy `focuswindow` dispatcher.
#[cfg(feature = "workspaces+hyprland")]
fn focus_window_arg(address: &str) -> String {
    format!("{{window=\"address:{address}\"}}")
}

#[cfg(feature = "workspaces+hyprland")]
fn detect_lua_config() -> bool {
    match get_hyprland_config_provider() {
        Ok(provider) => provider == "lua",
        Err(err) => {
            warn!("Failed to detect Hyprland config provider, assuming legacy: {err}");
            false
        }
    }
}

#[cfg(feature = "workspaces+hyprland")]
#[derive(Deserialize)]
struct HyprlandStatus {
    #[serde(rename = "configProvider")]
    config_provider: String,
}

#[cfg(feature = "workspaces+hyprland")]
fn get_hyprland_config_provider() -> std::result::Result<String, Box<dyn std::error::Error>> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("UID").map(|uid| format!("/run/user/{uid}")))?;
    let instance = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")?;
    let socket_path = format!("{runtime_dir}/hypr/{instance}/.socket.sock");

    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(b"j/status")?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    Ok(serde_json::from_str::<HyprlandStatus>(&response)?.config_provider)
}

fn get_workspace_name(name: WorkspaceType) -> String {
    match name {
        WorkspaceType::Regular(name) => name,
        WorkspaceType::Special(name) => name.unwrap_or_default(),
    }
}

/// Creates a function which determines if a workspace is visible.
///
/// This function makes a Hyprland call that allocates so it should be cached when possible,
/// but it is only valid so long as workspaces do not change so it should not be stored long term
fn create_is_visible() -> impl Fn(&HWorkspace) -> bool {
    let monitors = hyprland::data::Monitors::get().map_or(Vec::new(), HyprDataVec::to_vec);

    move |w| monitors.iter().any(|m| m.active_workspace.id == w.id)
}

impl From<(Visibility, HWorkspace)> for Workspace {
    fn from((visibility, workspace): (Visibility, HWorkspace)) -> Self {
        Self {
            id: workspace.id as i64,
            index: workspace.id as i64,
            name: workspace.name,
            monitor: workspace.monitor,
            visibility,
        }
    }
}

impl<'a, 'f, F> From<(&'a HWorkspace, Option<&str>, F)> for Visibility
where
    F: FnOnce(&'f HWorkspace) -> bool,
    'a: 'f,
{
    fn from((workspace, active_name, is_visible): (&'a HWorkspace, Option<&str>, F)) -> Self {
        if Some(workspace.name.as_str()) == active_name {
            Self::focused()
        } else if is_visible(workspace) {
            Self::visible()
        } else {
            Self::Hidden
        }
    }
}

#[cfg(all(test, feature = "workspaces+hyprland"))]
mod tests {
    use super::{focus_window_arg, focus_workspace_here_arg};

    // These arguments are Lua source evaluated by the compositor, not Rust the
    // compiler can check. A typo does not fail the build and does not raise an
    // error at the call site — the dispatch simply does nothing, which is the
    // failure mode the Lua-config-provider bug (#1548) presented as. Pin the
    // exact strings.

    #[test]
    fn focus_workspace_here_arg_pins_the_current_monitor() {
        assert_eq!(
            focus_workspace_here_arg(4),
            "{workspace=\"4\",on_current_monitor=true}"
        );
    }

    #[test]
    fn focus_window_arg_addresses_the_window() {
        assert_eq!(focus_window_arg("0x55c0"), "{window=\"address:0x55c0\"}");
    }
}
