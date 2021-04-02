use arboard::Error;
use log::{error, info, trace, warn};
use once_cell::sync::Lazy;
use parking_lot::{Condvar, Mutex, MutexGuard, RwLock};
use std::{
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
    usize,
};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{
            AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
            SelectionNotifyEvent, SelectionRequestEvent, Time, WindowClass, SELECTION_NOTIFY_EVENT,
        },
        Event,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
};

type Result<T, E = Error> = std::result::Result<T, E>;

fn into_unknown<E: std::error::Error>(err: E) -> Error {
    Error::Unknown {
        description: err.to_string(),
    }
}

static THREAD_GUARD: AtomicBool = AtomicBool::new(false);

// This is locked in write mode ONLY when the object is created and
// destroyed. All the rest of the time, everyone is allowed to keep a
// (const) reference to the inner `Arc<ClipboardInner>`, which saves a bunch of
// locking and unwraping in code.

// TODO THIS PROBABLY NEEDS TO BE A MUTEXT INSTEAD OF AN RW LOCK
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
    server_handle: JoinHandle<()>,
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

        let event_mask =
            // Just in case that some program reports SelectionNotify events
            // with XCB_EVENT_MASK_PROPERTY_CHANGE mask.
            EventMask::PROPERTY_CHANGE |
            // To receive DestroyNotify event and stop the message loop.
            EventMask::STRUCTURE_NOTIFY;
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
            &CreateWindowAux::new().event_mask(event_mask),
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
            self.server.conn.flush().map_err(into_unknown)?;
        }

        // Just setting the data, and the `serve_requests` will take care of the rest.
        *self.data.write() = Some(message);

        Ok(())
    }

    fn read(&self) -> Result<String> {
        // if we are the current owner, we can get the current clipboard ourselves
        /*if self.is_owner()? {
            let data = self.data.read();
            return data.clone().ok_or(Error::ContentNotAvailable);
        }
        if let Some(data) = self.data.read().clone() {
            return Ok(data)
        }*/
        let reader = XContext::new()?;

        trace!(
            "Calling `convert_selection` on: {:?}",
            self.atom_name(self.atoms._BOOP)
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
            .map_err(into_unknown)?;
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
                    if event.property == NONE {
                        return Err(Error::ContentNotAvailable);
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

                            break Ok(message);
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
        trace!("SelectionRequest: '{:?}'", self.atom_name(event.target));
        let success;
        // we are asked for a list of supported conversion targets
        if event.target == self.atoms.TARGETS && event.selection == self.atoms.CLIPBOARD {
            trace!("property is {}", self.atom_name(event.property)?);
            self.server
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    event.property,
                    // TODO: change to `AtomEnum::ATOM`
                    self.atoms.ATOM,
                    // TODO: add `SAVE_TARGETS` to signal support for clipboard manager
                    &[self.atoms.TARGETS, self.atoms.UTF8_STRING],
                )
                .map_err(into_unknown)?;
            self.server.conn.flush().map_err(into_unknown)?;
            success = true;
        }
        // we are asked to send a the data in a supported UTF8 format
        else if event.target == self.atoms.UTF8_STRING {
            trace!("writing to target {}", self.atom_name(event.property)?);
            let data = self.data.read();
            if let Some(string) = &*data {
                self.server
                    .conn
                    .change_property8(
                        PropMode::REPLACE,
                        event.requestor,
                        event.property,
                        // TODO: be more precise and say `self.atoms.UTF8_STRING`, we can encapsulate this when we add more types
                        event.target,
                        string.as_bytes(),
                    )
                    .map_err(into_unknown)?;
                self.server.conn.flush().map_err(into_unknown)?;
                success = true;
            } else {
                // TODO: we should still continue sending data
                // This must mean that we lost ownership of the data
                // since the other side requested the selection.
                // Let's respond with the property set to none.
                success = false;
            }
        } else {
            trace!("received a foreign event");
            return Ok(());
        }

        // on failure we notify the requester of it
        let property = if success {
            event.property
        } else {
            AtomEnum::NONE.into()
        };
        // tell the requestor that we finished sending data
        self.server
            .conn
            .send_event(
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
                    property,
                },
            )
            .map_err(into_unknown)?;

        self.server.conn.flush().map_err(into_unknown)
    }

    fn ask_clipboard_manager_to_request_our_data(&self) -> Result<()> {
        if self.server.win_id == 0 {
            // This shouldn't really ever happen but let's just check.
            error!("The server's window id was 0. This is unexpected");
            return Ok(());
        }
        if !self.is_owner()? {
            // We are not owning the clipboard, nothing to do.
            return Ok(());
        }
        if self.data.read().is_none() {
            // If we don't have any data, there's nothing to do.
            return Ok(());
        }

        // It's important that we lock the state before sending the request
        // because we don't want the request server thread to lock the state
        // after the request but before we can lock it here.
        let mut handover_state = self.handover_state.lock();

        println!("Sending the data to the clipboard manager");
        self.server
            .conn
            .convert_selection(
                self.server.win_id,
                self.atoms.CLIPBOARD_MANAGER,
                self.atoms.SAVE_TARGETS,
                self.atoms._BOOP,
                Time::CURRENT_TIME,
            )
            .map_err(into_unknown)?;
        self.server.conn.flush().map_err(into_unknown)?;

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
            let result = self
                .handover_cv
                .wait_for(&mut handover_state, handover_max_duration);

            if result.timed_out() {
                timed_out!();
            }

            if *handover_state == ManagerHandoverState::Finished {
                return Ok(());
            }
        }
    }
}

struct ThreadGuard;

impl ThreadGuard {
    fn new() -> Self {
        THREAD_GUARD.store(true, Ordering::SeqCst);
        ThreadGuard
    }
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        THREAD_GUARD.store(false, Ordering::SeqCst);
    }
}

fn serve_requests(clipboard: Arc<ClipboardContext>) -> Result<(), Box<dyn std::error::Error>> {
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

    trace!("Started serve reqests thread.");

    let mut written = false;
    let mut notified = false;

    loop {
        match clipboard
            .server
            .conn
            .wait_for_event()
            .map_err(into_unknown)?
        {
            Event::DestroyNotify(_) => {
                // This window is being destroyed.
                trace!("Clipboard server window is being destroyed");
                return Ok(());
            }
            Event::SelectionClear(event) => {
                // TODO: check if this works
                // Someone else has new content in the clipboard, so it is
                // notifying us that we should delete our data now.
                trace!("Somebody else owns the clipboard now");
                if event.selection == clipboard.atoms.CLIPBOARD {
                    let mut data = clipboard.data.write();
                    *data = None;
                }
            }
            Event::SelectionRequest(event) => {
                trace!(
                    "{} - selection is: {}",
                    line!(),
                    clipboard.atom_name(event.selection)?
                );
                // Someone is requesting the clipboard content from us.
                clipboard
                    .handle_selection_request(event)
                    .map_err(into_unknown)?;

                // if we are in the progress of saving to the clipboard manager
                // make sure we save that we have finished writing
                let handover_state = clipboard.handover_state.lock();
                if *handover_state == ManagerHandoverState::InProgress {
                    written = true;
                    // if we have written and notified, make sure to notify that we are done
                    if notified {
                        handover_finished(&clipboard, handover_state);
                    }
                }
            }
            Event::SelectionNotify(event) => {
                trace!("{}", line!());
                // We've requested the clipboard content and this is the
                // answer. Considering that this thread is not responsible
                // for reading clipboard contents, this must come from
                // the clipboard manager signaling that the data was
                // handed over successfully.
                if event.selection != clipboard.atoms.CLIPBOARD_MANAGER {
                    error!("Received a `SelectionNotify` from a selection other than the CLIPBOARD_MANAGER. This is unexpected in this thread.");
                    continue;
                }
                trace!("SelectionNotify from the clipboard manager");
                let handover_state = clipboard.handover_state.lock();
                if *handover_state == ManagerHandoverState::InProgress {
                    notified = true;

                    // One would think that we could also finish if the property
                    // here is set 0, because that indicates failure. However
                    // this is not the case; for example on KDE plasma 5.18, we
                    // immediately get a SelectionNotify with property set to 0,
                    // but following that, we also get a valid SelectionRequest
                    // from the clipboard manager.
                    if written {
                        handover_finished(&clipboard, handover_state);
                    }
                }
            }
            // TODO: remove when we know more
            // We've requested the clipboard content and this is the
            // answer.
            Event::PropertyNotify(event) => {
                info!("PropertyNotify: NOT YET IMPLEMENTED!");
            }
            event @ _ => {
                trace!("Received unexpected event: {:?}", event);
            }
        }
    }
}

pub struct Clipboard {
    inner: Arc<ClipboardContext>,
}

impl Clipboard {
    pub fn is_alive() -> bool {
        THREAD_GUARD.load(Ordering::SeqCst)
    }

    pub fn new() -> Result<Self> {
        // We are locking the global in write mode (exclusive)
        // to ensure that this is the invocation where we
        // initialize it if it doesn't exist.
        let mut global_cb = CLIPBOARD.write();
        if let Some(global_cb) = &*global_cb {
            return Ok(Self {
                inner: Arc::clone(&global_cb.context),
            });
        }
        // At this point we know that the clipboard does not exists.
        let ctx = Arc::new(ClipboardContext::new()?);
        let join_handle;
        {
            let ctx = Arc::clone(&ctx);
            join_handle = std::thread::spawn(move || {
                let _guard = ThreadGuard::new();

                if let Err(error) = serve_requests(ctx) {
                    error!("Worker thread errored with: {}", error);
                }
            });
        }
        *global_cb = Some(GlobalClipboard {
            context: Arc::clone(&ctx),
            server_handle: join_handle,
        });
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
        // There are always at least 3 owners:
        // the global, the server thread, and the `Clipboard::inner`
        const MIN_OWNERS: usize = 3;

        // We start with locking the global guard to prevent  race
        // conditions below.
        let mut global_cb = CLIPBOARD.write();
        if Arc::strong_count(&self.inner) == MIN_OWNERS {
            // If the are the only owers of the clipboard are ourselves and
            // the global object, then we should destroy the global object,
            // and send the data to the clipboard manager

            trace!("{}", line!());

            if let Err(e) = self.inner.ask_clipboard_manager_to_request_our_data() {
                error!(
                    "Could not hand the clipboard data over to the clipboard manager: {}",
                    e
                );
            }
            trace!("{}", line!());
            if let Err(e) = self
                .inner
                .server
                .conn
                .destroy_window(self.inner.server.win_id)
            {
                error!("Failed to destroy the clipboard window. Error: {}", e);
                return;
            }
            trace!("{}", line!());
            if let Err(e) = self.inner.server.conn.flush() {
                error!("Failed to flush the clipboard window. Error: {}", e);
                return;
            }
            trace!("{}", line!());
            if let Some(global_cb) = global_cb.take() {
                trace!("{}", line!());
                if let Err(_) = global_cb.server_handle.join() {
                    error!("The clipboard server thread paniced.");
                }
                trace!("{}", line!());
            }
        }
    }
}
