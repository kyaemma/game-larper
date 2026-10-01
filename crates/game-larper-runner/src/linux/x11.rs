//! The runner's X11 window, through x11rb's pure-Rust connection (no libX11/libxcb).
//!
//! Why these choices (X11 protocol / ICCCM / EWMH):
//!
//! - `override-redirect`: the window manager never manages, decorates, lists or focuses it,
//!   so it cannot steal focus or appear in a taskbar. It is still a real, mapped (`Viewable`)
//!   child of the root window that any client walking the tree with `QueryTree` sees.
//!   The trade-off: unmanaged windows are absent from `_NET_CLIENT_LIST` and carry no
//!   `WM_STATE`. If Discord only looked at managed windows it would not see this one; that is
//!   exactly what the manual X11 test has to answer.
//! - Off-screen at (-32000, -32000), 1×1: the X server honours the position of unmanaged
//!   windows, so it is never on any monitor (coordinates are 16-bit; this fits).
//! - `WM_CLASS` = `eldenring.exe` / `eldenring.exe` (Wine names its windows after the
//!   executable the same way), `WM_NAME`/`_NET_WM_NAME` = `eldenring` (the Win32 runner's
//!   title), `_NET_WM_PID` + `WM_CLIENT_MACHINE` so the window can be tied to this process.

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, MapState, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The same off-screen corner the Win32 runner uses.
const OFFSCREEN: (i16, i16) = (-32000, -32000);

pub struct Window {
    connection: RustConnection,
    pub id: u32,
    pub map_state: &'static str,
    pub override_redirect: bool,
    pub position: (i16, i16),
}

impl Window {
    /// Connect to `$DISPLAY`, then create, label and map the window.
    pub fn open(title: &str, class: &str) -> Result<Self, String> {
        let (connection, screen) = x11rb::connect(None).map_err(|error| error.to_string())?;
        let root = connection.setup().roots[screen].root;
        let id = connection
            .generate_id()
            .map_err(|error| error.to_string())?;
        let attributes = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::STRUCTURE_NOTIFY);
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                root,
                OFFSCREEN.0,
                OFFSCREEN.1,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &attributes,
            )
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        label(&connection, id, title, class).map_err(|error| format!("labelling: {error}"))?;
        connection
            .map_window(id)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        let attributes = connection
            .get_window_attributes(id)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let geometry = connection
            .get_geometry(id)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            connection,
            id,
            map_state: match attributes.map_state {
                MapState::VIEWABLE => "viewable",
                MapState::UNVIEWABLE => "mapped but unviewable",
                _ => "unmapped",
            },
            override_redirect: attributes.override_redirect,
            position: (geometry.x, geometry.y),
        })
    }

    /// Keep the window alive. Returns only when it or the X connection is gone.
    pub fn serve(self) -> String {
        loop {
            match self.connection.wait_for_event() {
                Ok(Event::DestroyNotify(event)) if event.window == self.id => {
                    return "destroyed by another client".into();
                }
                Ok(_) => {}
                Err(error) => return format!("X connection lost: {error}"),
            }
        }
    }
}

fn label(
    connection: &RustConnection,
    id: u32,
    title: &str,
    class: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let atom = |name: &str| -> Result<u32, Box<dyn std::error::Error>> {
        Ok(connection
            .intern_atom(false, name.as_bytes())?
            .reply()?
            .atom)
    };
    let utf8 = atom("UTF8_STRING")?;
    // WM_NAME is Latin-1 by definition; _NET_WM_NAME carries the exact UTF-8 title.
    if title.is_ascii() {
        connection.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?;
    }
    connection.change_property8(
        PropMode::REPLACE,
        id,
        atom("_NET_WM_NAME")?,
        utf8,
        title.as_bytes(),
    )?;
    // ICCCM: instance and class, each NUL-terminated.
    let wm_class = format!("{class}\0{class}\0");
    connection.change_property8(
        PropMode::REPLACE,
        id,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        wm_class.as_bytes(),
    )?;
    connection.change_property32(
        PropMode::REPLACE,
        id,
        atom("_NET_WM_PID")?,
        AtomEnum::CARDINAL,
        &[std::process::id()],
    )?;
    // EWMH pairs _NET_WM_PID with the machine it is valid on.
    if let Ok(host) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        connection.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLIENT_MACHINE,
            AtomEnum::STRING,
            host.trim_end().as_bytes(),
        )?;
    }
    connection.flush()?;
    Ok(())
}
