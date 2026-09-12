/// Url handler based on dbus
///
/// Note:
/// On linux, we use dbus to communicate between multiple rustdesk processes.
/// [Flutter]: handle uni links for linux
use dbus::blocking::Connection;
#[cfg(target_os = "linux")]
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus_crossroads::{Crossroads, IfaceBuilder};
use hbb_common::log;
#[cfg(feature = "flutter")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::{error::Error, fmt, time::Duration};

const DBUS_NAME: &str = "org.rustdesk.rustdesk";
const DBUS_PREFIX: &str = "/dbus";
const DBUS_METHOD_NEW_CONNECTION: &str = "NewConnection";
const DBUS_METHOD_NEW_CONNECTION_ID: &str = "id";
const DBUS_METHOD_RETURN: &str = "ret";
const DBUS_METHOD_RETURN_SUCCESS: &str = "ok";
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct DbusError(String);

impl fmt::Display for DbusError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SubnetDesk DBus Error: {}", self.0)
    }
}

impl Error for DbusError {}

/// invoke new connection from dbus
///
/// [Tips]:
/// How to test by CLI:
/// - use dbus-send command:
/// `dbus-send --session --print-reply --dest=org.rustdesk.rustdesk /dbus org.rustdesk.rustdesk.NewConnection string:'PEER_ID'`
pub fn invoke_new_connection(uni_links: String) -> Result<(), Box<dyn Error>> {
    log::info!("Starting dbus service for uni");
    let conn = Connection::new_session()?;
    let proxy = conn.with_proxy(DBUS_NAME, DBUS_PREFIX, DBUS_TIMEOUT);
    let (ret,): (String,) =
        proxy.method_call(DBUS_NAME, DBUS_METHOD_NEW_CONNECTION, (uni_links,))?;
    if ret != DBUS_METHOD_RETURN_SUCCESS {
        log::error!("error on call new connection to dbus server");
        return Err(Box::new(DbusError("not success".to_string())));
    }
    Ok(())
}

/// start dbus server
///
/// [Blocking]:
/// The function will block current thread to serve dbus server.
/// So it's suitable to spawn a new thread dedicated to dbus server.
pub fn start_dbus_server() -> Result<(), Box<dyn Error>> {
    let conn: Connection = Connection::new_session()?;
    let _ = conn.request_name(DBUS_NAME, false, true, false)?;
    let mut cr = Crossroads::new();
    let token = cr.register(DBUS_NAME, handle_client_message);
    cr.insert(DBUS_PREFIX, &[token], ());
    cr.serve(&conn)?;
    Ok(())
}

// TODO(upstream window_manager): `rustdesk-org/window_manager`'s Linux plugin
// raises the main window via a bare `gtk_window_present(get_window(self))`
// (linux/window_manager_plugin.cc), i.e. with no timestamp, so GTK treats it
// as GDK_CURRENT_TIME. Every modern WM's focus-stealing prevention (Mutter,
// KWin) distrusts an activation request with no timestamp tied to a real,
// recent input event -- and there IS no such event here: this handler runs
// on the dbus-crossroads server thread in response to an IPC call from the
// tray process (`tray.rs`'s `open_func` -> `invoke_new_connection`), not from
// a live GTK event, so `gtk_window_present()` gets silently deferred or
// ignored. Observed as: clicking "Open" (or a favorite) in the tray menu
// takes up to ~30s to actually show the window, but immediately succeeds if
// the user right-clicks the tray icon again right after -- that fresh, truly
// timestamped input event is apparently what lets the WM reconcile the
// pending activation. `xdotool windowactivate` sends a proper EWMH
// `_NET_ACTIVE_WINDOW` client message (source_indication=2, "pager"), which
// WMs are specced to honor from an external tool without the same
// timestamp scrutiny -- so it reliably raises the window where the bare
// `gtk_window_present()` call does not.
//
// This is a best-effort workaround, not a proper fix: it shells out to the
// optional `xdotool` CLI (not a hard dependency; failure here is silently
// swallowed and just leaves the pre-existing behavior) and targets "any
// window belonging to this process" rather than a specific window handle.
// Remove this call (and this function) once `window_manager`'s Linux plugin
// calls `gtk_window_present_with_time()` with a real server timestamp
// (e.g. via `gdk_x11_get_server_time()`) instead of `gtk_window_present()`.
//
// Runs on a detached thread and retries for about a second: if the main
// window is currently hidden (e.g. minimized to tray), `xdotool` can only
// activate it once it is actually mapped, and that mapping happens
// asynchronously on the Dart side (windowOnTop() -> windowManager.show())
// in response to the very `on_url_scheme_received` event pushed right after
// this function is called -- so the window may well not exist yet the
// moment we'd otherwise try. A single immediate attempt raced that and
// mostly lost. `run_cmds` only returns captured stdout and swallows the
// child's exit status, so there is no clean way to detect "no window found
// yet" versus "activated" -- retrying a few times is crude but harmless
// (xdotool no-ops instantly when nothing matches) and covers the real
// window-creation latency the single-shot version did not.
// KWin (KDE Plasma's compositor/WM) applies a stricter focus-stealing-
// prevention policy than GNOME/Mutter to an externally-sourced EWMH
// _NET_ACTIVE_WINDOW request -- the same xdotool call below that activates
// a window within ~0.3s under Mutter has been observed (live-tested,
// 2026-09-12) taking several seconds under KWin, until some unrelated fresh
// user input event (e.g. right-clicking the tray again) happens to make
// KWin finally honor the pending request. KWin exposes its own scripting
// D-Bus interface (org.kde.KWin, /Scripting) that can set
// `workspace.activeWindow` directly -- an internal, privileged KWin
// mechanism, not an external activation *request* -- which sidesteps that
// policy entirely. Verified live: the same PID-based lookup activated a
// backgrounded window instantly under KWin 6.7.4 via this path, where
// xdotool alone was unreliable.
//
// Best-effort and KDE-specific: on any other desktop (no org.kde.KWin on
// the session bus) the very first D-Bus call fails immediately and this
// silently falls through, changing nothing. Kept alongside the xdotool
// attempt below (not as a replacement) since that one already works fine
// on GNOME and other WMs.
// A shared temp dir keyed only on a predictable value (like a PID) is a
// symlink/overwrite target for another local user. Create a privately-owned,
// uniquely-named directory instead -- `create_dir` fails on an existing
// entry (no TOCTOU window to race), so a pre-planted symlink or directory at
// the chosen name simply causes a retry with a new random name rather than
// being followed or overwritten.
#[cfg(target_os = "linux")]
fn create_private_temp_dir(prefix: &str) -> Result<std::path::PathBuf, Box<dyn Error>> {
    for _ in 0..8 {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", hbb_common::rand::random::<u64>()));
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
                return Ok(dir);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(Box::new(err)),
        }
    }
    Err(Box::new(DbusError(
        "could not create a private temp dir after 8 attempts".to_string(),
    )))
}

#[cfg(target_os = "linux")]
fn activate_via_kwin_script(pid: u32) -> Result<(), Box<dyn Error>> {
    let script_dir = create_private_temp_dir("subnetdesk-kwin-activate")?;
    let script_path = script_dir.join(format!("{pid}.js"));
    // `workspace.windowList()` (KWin 5) was renamed to the `workspace.windows`
    // property in KWin 6's scripting API; support either.
    //
    // Setting `activeWindow` alone is not enough: live-tested (2026-09-12),
    // the main window ends up in KWin's window list with `minimized: true`
    // (window_manager's `.show()` on Linux doesn't restore a minimized
    // window itself), and `workspace.activeWindow = win` silently does
    // nothing for a still-minimized window. Explicitly clearing `minimized`
    // first is what actually brings it on screen.
    let script = format!(
        "var wins = (typeof workspace.windowList === 'function') ? workspace.windowList() : workspace.windows;\n\
         for (var i = 0; i < wins.length; i++) {{\n\
         \tif (wins[i].pid == {pid}) {{\n\
         \t\twins[i].minimized = false;\n\
         \t\tworkspace.activeWindow = wins[i];\n\
         \t}}\n\
         }}\n"
    );
    std::fs::File::create(&script_path)?.write_all(script.as_bytes())?;
    let plugin_name = format!("subnetdesk-activate-{pid}");

    let conn = Connection::new_session()?;
    let proxy = conn.with_proxy("org.kde.KWin", "/Scripting", DBUS_TIMEOUT);
    let (script_id,): (i32,) = proxy.method_call(
        "org.kde.kwin.Scripting",
        "loadScript",
        (script_path.to_string_lossy().to_string(), plugin_name.clone()),
    )?;
    let script_obj_path = format!("/Scripting/Script{script_id}");
    let script_proxy = conn.with_proxy("org.kde.KWin", script_obj_path, DBUS_TIMEOUT);
    let _: () = script_proxy.method_call("org.kde.kwin.Script", "run", ())?;
    let _: (bool,) =
        proxy.method_call("org.kde.kwin.Scripting", "unloadScript", (plugin_name,))?;
    let _ = std::fs::remove_dir_all(&script_dir);
    Ok(())
}

#[cfg(target_os = "linux")]
fn activate_main_window_workaround() {
    let pid = std::process::id();
    std::thread::spawn(move || {
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(200));
            if let Err(err) = activate_via_kwin_script(pid) {
                log::debug!("KWin scripting activation unavailable/failed: {err}");
            }
            if let Err(err) =
                crate::platform::run_cmds(&format!("xdotool search --pid {pid} windowactivate"))
            {
                log::debug!("xdotool windowactivate workaround failed (xdotool missing?): {err}");
                return;
            }
        }
    });
}

fn handle_client_message(builder: &mut IfaceBuilder<()>) {
    // register new connection dbus
    builder.method(
        DBUS_METHOD_NEW_CONNECTION,
        (DBUS_METHOD_NEW_CONNECTION_ID,),
        (DBUS_METHOD_RETURN,),
        move |_, _, (_uni_links,): (String,)| {
            #[cfg(target_os = "linux")]
            activate_main_window_workaround();
            #[cfg(feature = "flutter")]
            {
                use crate::flutter;
                let data = HashMap::from([
                    ("name", "on_url_scheme_received"),
                    ("url", _uni_links.as_str()),
                ]);
                let event = serde_json::ser::to_string(&data).unwrap_or("".to_string());
                match crate::flutter::push_global_event(flutter::APP_TYPE_MAIN, event) {
                    None => log::error!("failed to find main event stream"),
                    Some(false) => {
                        log::error!("failed to add dbus message to flutter global dbus stream.")
                    }
                    Some(true) => {}
                }
            }
            return Ok((DBUS_METHOD_RETURN_SUCCESS.to_string(),));
        },
    );
}

// A host may run its own screen-locking power-save script that powers the
// physical monitor off (via org.gnome.Mutter.DisplayConfig's PowerSaveMode
// property) when the session locks, to save power/wear on an unattended
// machine. On GNOME, an incoming SubnetDesk connection while the screen is
// in that state can capture a monitor that's powered off (no frames ever
// arrive -- "Connection successful, waiting for image..." forever), and
// the user's own script has no way to know a remote viewer just connected,
// so it never wakes the display back up. Real fix: SubnetDesk wakes the
// display itself on a new connection (mirroring the exact D-Bus call such
// a script would use), and re-blanks it after the last viewer disconnects
// if the screen is still locked, so the power-saving behavior isn't lost.
//
// Best-effort and silent: this only matters on GNOME (org.gnome.ScreenSaver
// / org.gnome.Mutter.DisplayConfig on the session bus); on any other
// desktop, or one where the screen isn't locked, the very first check
// below is a no-op.
#[cfg(target_os = "linux")]
fn is_screen_locked(conn: &Connection) -> Result<bool, Box<dyn Error>> {
    let screensaver = conn.with_proxy(
        "org.gnome.ScreenSaver",
        "/org/gnome/ScreenSaver",
        DBUS_TIMEOUT,
    );
    let (locked,): (bool,) =
        screensaver.method_call("org.gnome.ScreenSaver", "GetActive", ())?;
    Ok(locked)
}

#[cfg(target_os = "linux")]
fn set_display_power_save_mode(conn: &Connection, mode: i32) -> Result<(), Box<dyn Error>> {
    let display_config = conn.with_proxy(
        "org.gnome.Mutter.DisplayConfig",
        "/org/gnome/Mutter/DisplayConfig",
        DBUS_TIMEOUT,
    );
    display_config.set("org.gnome.Mutter.DisplayConfig", "PowerSaveMode", mode)?;
    Ok(())
}

// Process-wide, not per-`Server`: the physical display is a single resource
// regardless of which `Server` instance (LAN listener, web gateway) mediates
// a given connection, so viewer accounting lives here rather than on any one
// `Server`'s connection map.
//
// `CONNECT_GENERATION` follows the same "newest generation wins" pattern
// used in `libs/scrap/src/wayland/display.rs` (`SNAPSHOT_GENERATION`) to
// invalidate a stale async result: it only bumps when a monitor viewer
// *connects*, so a `reblank_display_if_still_locked` task scheduled by a
// disconnect can tell, after its grace period, whether a reconnect happened
// in the meantime and abandon itself instead of blanking a display a fresh
// viewer is now looking at.
#[cfg(target_os = "linux")]
static MONITOR_VIEWERS: AtomicI64 = AtomicI64::new(0);
#[cfg(target_os = "linux")]
static CONNECT_GENERATION: AtomicU64 = AtomicU64::new(0);
// Only set when we actually changed the power-save mode ourselves, so a
// disconnect never blanks a display that was already on before any viewer
// connected (e.g. the screen wasn't locked, or another session already
// consumed the wake).
#[cfg(target_os = "linux")]
static WOKE_DISPLAY: AtomicBool = AtomicBool::new(false);

/// Record that a screen/monitor viewer connected. Call before
/// [`wake_display_if_locked`].
#[cfg(target_os = "linux")]
pub fn note_monitor_connected() {
    MONITOR_VIEWERS.fetch_add(1, Ordering::SeqCst);
    CONNECT_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Record that a screen/monitor viewer disconnected. Returns the number of
/// monitor viewers still connected, so the caller only schedules a re-blank
/// once this reaches zero.
#[cfg(target_os = "linux")]
pub fn note_monitor_disconnected() -> i64 {
    MONITOR_VIEWERS.fetch_sub(1, Ordering::SeqCst) - 1
}

/// Wake the display if the screen is currently locked and powered down.
/// Called when a new monitor/screen viewer connection is accepted.
#[cfg(target_os = "linux")]
pub fn wake_display_if_locked() {
    std::thread::spawn(|| {
        let run = || -> Result<(), Box<dyn Error>> {
            let conn = Connection::new_session()?;
            if is_screen_locked(&conn)? {
                set_display_power_save_mode(&conn, 0)?;
                WOKE_DISPLAY.store(true, Ordering::SeqCst);
            }
            Ok(())
        };
        if let Err(err) = run() {
            log::debug!("wake_display_if_locked unavailable/failed (not GNOME?): {err}");
        }
    });
}

/// Pure decision logic for [`reblank_display_if_still_locked`]'s grace-period
/// recheck, split out so it's unit-testable without a live D-Bus session:
/// re-blanking is only still warranted if no viewer connected since this
/// task was scheduled, and no viewer is connected right now.
#[cfg(target_os = "linux")]
fn should_reblank(generation_at_schedule: u64, current_generation: u64, viewer_count: i64) -> bool {
    generation_at_schedule == current_generation && viewer_count <= 0
}

/// Re-blank the display after the last monitor viewer disconnects, if the
/// screen is still locked -- restoring the power-saving behavior that
/// [`wake_display_if_locked`] temporarily overrode. Only call this once
/// [`note_monitor_disconnected`] reports the monitor-viewer count has
/// actually reached zero.
#[cfg(target_os = "linux")]
pub fn reblank_display_if_still_locked() {
    let generation_at_schedule = CONNECT_GENERATION.load(Ordering::SeqCst);
    std::thread::spawn(move || {
        // Short grace period in case of a near-immediate reconnect, similar
        // in spirit to a user power-save script's own debounce.
        std::thread::sleep(Duration::from_secs(5));
        if !should_reblank(
            generation_at_schedule,
            CONNECT_GENERATION.load(Ordering::SeqCst),
            MONITOR_VIEWERS.load(Ordering::SeqCst),
        ) {
            // A viewer (re)connected during the grace period; leave the
            // display as-is for their session.
            return;
        }
        if !WOKE_DISPLAY.swap(false, Ordering::SeqCst) {
            // We never woke the display ourselves, so there's nothing of
            // ours to restore.
            return;
        }
        let run = || -> Result<(), Box<dyn Error>> {
            let conn = Connection::new_session()?;
            if is_screen_locked(&conn)? {
                set_display_power_save_mode(&conn, 3)?;
            }
            Ok(())
        };
        if let Err(err) = run() {
            log::debug!("reblank_display_if_still_locked unavailable/failed (not GNOME?): {err}");
        }
    });
}

#[cfg(all(test, target_os = "linux"))]
mod reblank_tests {
    use super::should_reblank;

    #[test]
    fn reblanks_when_generation_unchanged_and_no_viewers() {
        assert!(should_reblank(3, 3, 0));
    }

    #[test]
    fn skips_when_a_viewer_reconnected_during_the_grace_period() {
        // A reconnect bumps CONNECT_GENERATION, so the snapshot taken at
        // schedule time no longer matches -- this is the exact race the
        // review comment flagged.
        assert!(!should_reblank(3, 4, 0));
    }

    #[test]
    fn skips_when_a_viewer_is_still_connected() {
        assert!(!should_reblank(3, 3, 1));
    }
}
