use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, MutexGuard, RwLock, WaitTimeoutResult,
    },
    thread::JoinHandle,
    time::Duration,
};

use anyhow::{anyhow, Result};
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
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
};

use log::{error, warn};

use once_cell::sync::{Lazy, OnceCell};

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
        let (conn, screen_num): (RustConnection, _) = RustConnection::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let win_id = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            win_id,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        )?;
        conn.flush()?;
        Ok(Self { conn, win_id })
    }
}

impl ClipboardContext {
    fn new() -> Result<Self> {
        let server = XContext::new()?;
        let atoms = Atoms::new(&server.conn)?.reply()?;
        Ok(Self {
            server,
            atoms,
            data: Default::default(),
            handover_state: Mutex::new(ManagerHandoverState::Idle),
            handover_cv: Condvar::new(),
            manager_written: AtomicBool::new(false),
            manager_notified: AtomicBool::new(false),
        })
    }

    fn write(&self, message: String) -> Result<()> {
        println!();
        println!("new write");

        let selection = self.atoms.CLIPBOARD;
        let server_win = self.server.win_id;
        let owner = self.server.conn.get_selection_owner(selection)?.reply()?;
        if owner.owner != server_win {
            self.server
                .conn
                .set_selection_owner(server_win, selection, Time::CURRENT_TIME)?;
        }

        // Just setting the data, and the `serve_requests` will take care of the rest.
        let mut data = self.data.write().unwrap();
        *data = Some(message);

        Ok(())
    }

    fn read(&self) -> Result<String> {
        let cb_owner = self
            .server
            .conn
            .get_selection_owner(self.atoms.CLIPBOARD)?
            .reply()?;
        if cb_owner.owner == self.server.win_id {
            let data = self.data.read().unwrap();
            return data.clone().ok_or(anyhow!("No data available"));
        }
        let reader = XContext::new()?;

        println!(
            "Calling `convert_selection`: {}",
            self.atom_name(self.atoms._BOOP).unwrap()
        );

        //////////////////////////////////////////////////////////////////////
        // THIS SHOULDN'T BE NEEDED.
        // But if this wait is not here, then the `write` example fails at some point.
        // std::thread::park_timeout(std::time::Duration::from_millis(200));
        //////////////////////////////////////////////////////////////////////

        reader.conn.convert_selection(
            reader.win_id,
            self.atoms.CLIPBOARD,
            self.atoms.UTF8_STRING,
            self.atoms._BOOP,
            Time::CURRENT_TIME,
        )?;
        println!("Finished `convert_selection`");
        // std::thread::park_timeout(std::time::Duration::from_millis(500));
        // read requested data

        // Given that we use poll for event, we must flush outgoing events here.
        // (wait for events would do this automatically, but we don't want to block)
        reader.conn.flush()?;
        loop {
            // std::thread::park_timeout(Duration::from_millis(2));
            let event = if let Some(event) = reader.conn.poll_for_event().unwrap() {
                event
            } else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            match event {
                Event::SelectionNotify(event) => {
                    println!(
                        "Event::SelectionNotify: {}, {}, {}",
                        event.selection, event.target, event.property
                    );
                    let none: u32 = AtomEnum::NONE.into();
                    if event.property == none {
                        return Err(anyhow!("No data available"));
                    }
                    // TODO: handle chunking
                    // check if this is what we requested
                    if event.selection == self.atoms.CLIPBOARD {
                        //println!("{}", self.atom_name(event.property).unwrap());
                        let reply = reader
                            .conn
                            .get_property(
                                true,
                                event.requestor,
                                event.property,
                                AtomEnum::ANY,
                                0,
                                0x1fffffff,
                            )?
                            .reply();
                        reader.conn.flush()?;
                        if let Ok(reply) = reply {
                            println!("Property.type: {}", self.atom_name(reply.type_).unwrap());
                            reader.conn.flush()?;

                            // we found something
                            if reply.type_ == self.atoms.UTF8_STRING {
                                break Ok(String::from_utf8(reply.value).unwrap());
                            }
                        } else {
                            println!("reply was Err: {:?}", reply);
                        }
                    } else {
                        println!("not what we were looking for")
                    }
                }
                Event::PropertyNotify(event) => {
                    println!("PropertyNotify: {}", event.atom);
                }
                _ => println!("not the event that we wanted"),
            }
        }
    }

    fn atom_name(&self, atom: x11rb::protocol::xproto::Atom) -> Result<String> {
        String::from_utf8(self.server.conn.get_atom_name(atom)?.reply()?.name).map_err(Into::into)
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
