use arboard::Error;
use log::{error, trace, warn};
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::{Mutex, RwLock};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, WaitTimeoutResult,
    },
    thread::JoinHandle,
    time::Duration,
};
use thiserror::Error;
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{
            AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, GetPropertyType, PropMode,
            Screen, SelectionNotifyEvent, SelectionRequestEvent, Time, WindowClass,
            SELECTION_NOTIFY_EVENT,
        },
        Event,
    },
    rust_connection::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError, RustConnection},
    wrapper::ConnectionExt as _,
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
};

type Result<T, E = Error> = std::result::Result<T, E>;

fn into_unknown<E: std::error::Error>(err: E) -> Error {
    Error::Unknown {
        description: err.to_string(),
    }
}

// This is locked in write mode ONLY when the object is created and
// destroyed. All the rest of the time, everyone is allowed to keep a
// (const) reference to the inner `Arc<ClipboardInner>`, which saves a bunch of
// locking and unwraping in code.
static CLIPBOARD: Lazy<RwLock<Option<GlobalClipboard>>> = Lazy::new(|| Default::default());

x11rb::atom_manager! {
    pub Atoms: AtomCookies {
        CLIPBOARD,
        CLIPBOARD_MANAGER,
        SAVE_TARGETS,
        TARGETS,
        UTF8_STRING,
        ATOM,

        // This is just some random name for the property on our window, into which
        // the clipboard owner writes the data we requested.
        _BOOP,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ManagerHandoverState {
    Idle,
    InProgress,
    Finished,
}

struct GlobalClipboard {
    context: Arc<ClipboardContext>,

    /// Join handle to the thread which serves selection requests.
    server_handle: Option<JoinHandle<()>>,
}

struct XContext {
    conn: RustConnection,
    win_id: u32,
}

struct ClipboardContext {
    /// The context for the thread which serves clipboard read
    /// requests coming to us.
    server: XContext,
    atoms: Atoms,
    data: RwLock<Option<String>>,
    handover_state: Mutex<ManagerHandoverState>,
    handover_cv: Condvar,
    manager_written: AtomicBool,
    manager_notified: AtomicBool,
}

impl XContext {
    fn new() -> Result<Self> {
        // create a new connection to an X11 server
        let (conn, screen_num): (RustConnection, _) =
            RustConnection::connect(None).map_err(into_unknown)?;
        let screen = conn.setup().roots.get(screen_num).ok_or(Error::Unknown {
            description: String::from("no screen found"),
        })?;
        let win_id = conn.generate_id().map_err(into_unknown)?;

        // create the window
        conn.create_window(
            // copy as much as possible from the parent, because no other specific input is needed
            COPY_DEPTH_FROM_PARENT,
            win_id,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::COPY_FROM_PARENT,
            COPY_FROM_PARENT,
            // don't subscribe to any special events because we are requesting everything we need ourselves
            &CreateWindowAux::new(),
        )
        .map_err(into_unknown)?;
        conn.flush().map_err(into_unknown)?;

        Ok(Self { conn, win_id })
    }
}

impl ClipboardContext {
    fn new() -> Result<Self> {
        let server = XContext::new()?;
        let atoms = Atoms::new(&server.conn)
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?;

        Ok(Self {
            server,
            atoms,
            data: RwLock::default(),
            handover_state: Mutex::new(ManagerHandoverState::Idle),
            handover_cv: Condvar::new(),
            manager_written: AtomicBool::new(false),
            manager_notified: AtomicBool::new(false),
        })
    }

    fn write(&self, message: String) -> Result<()> {
        // only if the current owner isn't us, we try to become it
        if !self.is_owner()? {
            let selection = self.atoms.CLIPBOARD;
            let server_win = self.server.win_id;

            self.server
                .conn
                .set_selection_owner(server_win, selection, Time::CURRENT_TIME)
                .map_err(|_| Error::ClipboardOccupied)?;
        }

        // Just setting the data, and the `serve_requests` will take care of the rest.
        *self.data.write() = Some(message);

        Ok(())
    }

    fn read(&self) -> Result<Option<String>> {
        // if we are the current owner, we can get the current clipboard ourselves
        if self.is_owner()? {
            let data = self.data.read();
            return Ok(data.clone());
        }
        let reader = XContext::new()?;

        trace!(
            "Calling `convert_selection` on: {}",
            self.atom_name(self.atoms._BOOP).unwrap()
        );

        // request to convert the clipboard selection to our data type(s)
        reader
            .conn
            .convert_selection(
                reader.win_id,
                self.atoms.CLIPBOARD,
                self.atoms.UTF8_STRING,
                self.atoms._BOOP,
                Time::CURRENT_TIME,
            )
            .map_err(|_| Error::ConversionFailure)?;
        reader.conn.flush().map_err(into_unknown)?;

        trace!("Finished `convert_selection`");

        loop {
            match reader.conn.wait_for_event().map_err(into_unknown)? {
                // a selection exists
                Event::SelectionNotify(event) => {
                    trace!(
                        "Event::SelectionNotify: {}, {}, {}",
                        event.selection,
                        event.target,
                        event.property
                    );

                    // selection has no type = no selection present
                    let none: u32 = AtomEnum::NONE.into();
                    if event.property == none {
                        return Ok(None);
                    }

                    // TODO: handle chunking
                    // check if this is what we requested
                    if event.selection == self.atoms.CLIPBOARD {
                        // request the selection
                        let reply = reader
                            .conn
                            .get_property(
                                true,
                                event.requestor,
                                event.property,
                                // request the type that arrived
                                event.target,
                                0,
                                // the length is corrected to X11 specifications by x11rb
                                u32::MAX,
                            )
                            .map_err(into_unknown)?
                            .reply()
                            .map_err(into_unknown)?;

                        trace!("Property.type: {:?}", self.atom_name(reply.type_));

                        // we found something
                        if reply.type_ == self.atoms.UTF8_STRING {
                            let message = String::from_utf8(reply.value)
                                .map_err(|_| Error::ConversionFailure)?;

                            break Ok(Some(message));
                        } else {
                            // this should never happen, we have sent a request only for supported types
                            break Err(Error::Unknown {
                                description: String::from("incorrect type received from clipboard"),
                            });
                        }
                    } else {
                        log::trace!("a foreign selection arrived")
                    }
                }
                _ => log::trace!("an unrequested event arrived"),
            }
        }
    }

    fn is_owner(&self) -> Result<bool> {
        let current = self
            .server
            .conn
            .get_selection_owner(self.atoms.CLIPBOARD)
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?
            .owner;

        Ok(current == self.server.win_id)
    }

    fn atom_name(&self, atom: x11rb::protocol::xproto::Atom) -> Result<String> {
        String::from_utf8(
            self.server
                .conn
                .get_atom_name(atom)
                .map_err(into_unknown)?
                .reply()
                .map_err(into_unknown)?
                .name,
        )
        .map_err(into_unknown)
    }

    fn handle_selection_request(&self, event: SelectionRequestEvent) -> Result<()> {
        println!(
            "SelectionRequest: '{}'",
            self.atom_name(event.target).unwrap()
        );
        let success;
        if event.target == self.atoms.TARGETS && event.selection == self.atoms.CLIPBOARD {
            println!("property is {}", self.atom_name(event.property)?);
            self.server.conn.change_property32(
                PropMode::REPLACE,
                event.requestor,
                event.property,
                self.atoms.ATOM,
                &[self.atoms.TARGETS, self.atoms.UTF8_STRING],
            )?;
            success = true;
        } else if event.target == self.atoms.UTF8_STRING {
            println!("writing to target {}", self.atom_name(event.property)?);
            let data = self.data.read().unwrap();
            if let Some(string) = &*data {
                self.server.conn.change_property8(
                    PropMode::REPLACE,
                    event.requestor,
                    event.property,
                    event.target,
                    string.as_bytes(),
                )?;
                success = true;
            } else {
                // This must mean that we lost ownership of the data
                // since the other side requested the selection.
                // Let's respond with the property set to none.
                success = false;
            }
        } else {
            println!("THIS IS NOT SUPPOSED TO HAPPEN");
            return Ok(());
        }

        self.server.conn.flush()?;

        let property: u32 = if success {
            event.property
        } else {
            AtomEnum::NONE.into()
        };
        self.server.conn.send_event(
            false,
            event.requestor,
            EventMask::NO_EVENT,
            SelectionNotifyEvent {
                response_type: SELECTION_NOTIFY_EVENT,
                sequence: event.sequence,
                time: event.time,
                requestor: event.requestor,
                selection: event.selection,
                target: event.target,
                property: property,
            },
        )?;

        self.server.conn.flush()?;

        if event.target == self.atoms.UTF8_STRING {
            println!("successfully written!");
        }

        Ok(())
    }

    fn ask_clipboard_manager_to_request_our_data(&self) -> Result<()> {
        if self.server.win_id == 0 {
            // This shouldn't really ever happen but let's just check.
            error!("The server's window id was 0. This is unexpected");
            return Ok(());
        }
        if self.data.read().unwrap().is_none() {
            // If we don't have any data, there's nothing to do.
            return Ok(());
        }
        let sel_owner = self
            .server
            .conn
            .get_selection_owner(self.atoms.CLIPBOARD)?
            .reply()?;
        if sel_owner.owner != self.server.win_id {
            // We are not owning the clipboard, nothing to do.
            return Ok(());
        }

        // It's important that we lock the state before sending the request
        // because we don't want the request server thread to lock the state
        // after the request but before we can lock it here.
        let mut handover_state = self.handover_state.lock().unwrap();

        println!("Sending the data to the clipboard manager");
        self.server.conn.convert_selection(
            self.server.win_id,
            self.atoms.CLIPBOARD_MANAGER,
            self.atoms.SAVE_TARGETS,
            self.atoms._BOOP,
            Time::CURRENT_TIME,
        )?;
        self.server.conn.flush()?;

        *handover_state = ManagerHandoverState::InProgress;
        let handover_start = std::time::Instant::now();
        let handover_max_duration = Duration::from_millis(100);
        macro_rules! timed_out {
            () => {
                warn!("Could not hand the clipboard contents over to the clipboard manager. The request timed out.");
                return Ok(());
            }
        }
        loop {
            if handover_start.elapsed() >= handover_max_duration {
                // The `wait_timeout` method can wake up spuriously. If this always happens
                // before reaching the timeout, the codvar never times out and the handover
                // might never finish. This would produce an infinite loop here.
                // To protect agains this, we check if a certain time has elapsed since the
                // start of the loop.
                timed_out!();
            }
            match self
                .handover_cv
                .wait_timeout(handover_state, handover_max_duration)
            {
                Ok((new_guard, status)) => {
                    if *new_guard == ManagerHandoverState::Finished {
                        return Ok(());
                    }
                    if status.timed_out() {
                        timed_out!();
                    }
                    handover_state = new_guard;
                }
                Err(e) => {
                    panic!("Clipboard: The server thread paniced while having the `handover_state` locked. Error: {}", e);
                }
            }
        }

        Ok(())
    }
}

fn serve_requests(clipboard: Arc<ClipboardContext>) {
    fn handover_finished(
        clip: &Arc<ClipboardContext>,
        mut handover_state: MutexGuard<ManagerHandoverState>,
    ) {
        println!("Finishing handover");
        *handover_state = ManagerHandoverState::Finished;

        // Not sure if unlocking the mutext is necessary here but better safe than sorry.
        drop(handover_state);

        clip.handover_cv.notify_all();
    }

    loop {
        let event = match clipboard.server.conn.wait_for_event() {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to poll event for clipboard: {}", e);
                return;
            }
        };
        match event {
            Event::DestroyNotify(_) => {
                // This window is being destroyed.
                return;
            }
            Event::SelectionClear(event) => {
                // Someone else has new content in the clipboard, so it is
                // notifying us that we should delete our data now.
                println!("`SelectionClear`");
                if event.selection == clipboard.atoms.CLIPBOARD {
                    let mut data = clipboard.data.write().unwrap();
                    *data = None;
                }
            }
            Event::SelectionRequest(event) => {
                // Someone is requesting the clipboard content from us.
                if let Err(e) = clipboard.handle_selection_request(event) {
                    error!(
                        "Received a `SelectionRequest`, but failed to handle it: {}",
                        e
                    );
                } else if event.selection == clipboard.atoms.CLIPBOARD_MANAGER {
                    println!("SelectionRequest from the clipboard manager");
                    let handover_state = clipboard.handover_state.lock().unwrap();
                    if *handover_state == ManagerHandoverState::InProgress {
                        clipboard.manager_written.store(true, Ordering::Relaxed);
                        if clipboard.manager_notified.load(Ordering::Relaxed) {
                            handover_finished(&clipboard, handover_state);
                        }
                    }
                }
            }
            Event::SelectionNotify(event) => {
                // We've requested the clipboard content and this is the
                // answer. Considering that this thread is not responsible
                // for reading clipboard contents, this must come from
                // the clipboard manager signaling that the data was
                // handed over successfully.
                if event.selection != clipboard.atoms.CLIPBOARD_MANAGER {
                    error!("Received a `SelectionNotify` from a selection other than the CLIPBOARD_MANAGER. This is unexpected in this thread.");
                    continue;
                }
                println!("SelectionNotify from the clipboard manager");
                let handover_state = clipboard.handover_state.lock().unwrap();
                if *handover_state == ManagerHandoverState::InProgress {
                    clipboard.manager_notified.store(true, Ordering::Relaxed);

                    // One would think that we could also finish if the property
                    // here is set 0, because that indicates failure. However
                    // this is not the case; for example on KDE plasma 5.18, we
                    // immediately get a SelectionNotify with property set to 0,
                    // but following that, we also get a valid SelectionRequest
                    // from the clipboard manager.
                    if clipboard.manager_written.load(Ordering::Relaxed) {
                        handover_finished(&clipboard, handover_state);
                    }
                }
            }
            // We've requested the clipboard content and this is the
            // answer.
            Event::PropertyNotify(event) => {
                println!("PropertyNotify: NOT YET IMPLEMENTED!");
            }
            _ => (),
        }
    }
}

pub struct Clipboard {
    inner: Arc<ClipboardContext>,
}

impl Clipboard {
    pub fn new() -> Result<Self> {
        println!("TODO: SAVE THE CLIPBOARD CONTEXT TO THE GLOBAL OBJECT");
        let ctx = Arc::new(ClipboardContext::new()?);
        {
            let ctx = Arc::clone(&ctx);
            std::thread::spawn(move || {
                serve_requests(ctx);
            });
        }
        Ok(Self { inner: ctx })
    }

    pub fn read(&self) -> Result<String> {
        self.inner.read()
    }

    pub fn write(&self, message: String) -> Result<()> {
        self.inner.write(message)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        println!("THE DROP TRAIT IS NOT YET IMPLEMENTED!");
        if let Err(e) = self.inner.ask_clipboard_manager_to_request_our_data() {
            error!(
                "Could not hand the clipboard data over to the clipboard manager: {}",
                e
            );
        }
        // self.conn.destroy_window(self.win_id).unwrap();
    }
}
