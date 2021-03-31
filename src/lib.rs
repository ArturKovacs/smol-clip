use std::{
    sync::{Arc, RwLock},
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
                .set_selection_owner(server_win, selection, Time::CURRENT_TIME);
        }

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
                // Let's respond with property set to none.
                success = false;
                // warn!("Got a selection request but we have no data.");
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
}

fn serve_requests(clipboard: Arc<ClipboardContext>) {
    loop {
        if let Err(e) = clipboard.server.conn.flush() {
            error!("Could not flush the X connection. Error: {}", e);
        }
        let event = match clipboard.server.conn.poll_for_event() {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to poll event for clipboard: {}", e);
                return;
            }
        };
        let event = match event {
            Some(e) => e,
            None => {
                // If there was no event, just wait a bit and then loop again.
                std::thread::sleep(Duration::from_millis(4));
                continue;
            }
        };
        match event {
            Event::DestroyNotify(_) => {
                // This window is being destroyed.
                return;
            }
            Event::SelectionClear(event) => {
                // Someone else has new content in the clipboard, so is
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
                }
            }
            Event::SelectionNotify(_) => {
                // We've requested the clipboard content and this is the
                // answer.
                warn!("Received a `SelectionNotify` in the clipboard `serve_requests` function. This is unexpected.");
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
        // self.conn.destroy_window(self.win_id).unwrap();
    }
}
