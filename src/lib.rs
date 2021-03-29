
use std::sync::Mutex;

use anyhow::Result;
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{
            ConnectionExt as _, CreateWindowAux, EventMask, GetPropertyType, PropMode,
            SelectionNotifyEvent, Time, WindowClass, SELECTION_NOTIFY_EVENT,
        },
        Event,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
};

use once_cell::sync::Lazy;

#[derive(Debug, Default, Clone)]
struct ClipboardCache {
    src_win_id: u32,
    data: String,
}

static CB_DATA: Lazy<Mutex<ClipboardCache>> = Lazy::new(|| Default::default());

x11rb::atom_manager! {
    pub Atoms: AtomCookies {
        CLIPBOARD,
        CLIPBOARD_MANAGER,
        SAVE_TARGETS,
        TARGETS,
        UTF8_STRING,
        XA_ATOM,
        _ABOARD_SELECTION,
    }
}
pub struct Clipboard {
    conn: RustConnection,
    win_id: u32,
    atoms: Atoms,
}

impl Clipboard {
    fn new() -> Result<Self> {
        let (conn, screen_num): (RustConnection, _) = RustConnection::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let win_id = conn.generate_id()?;

        // create window
        conn.create_window(
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
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        conn.flush()?;

        let atoms = Atoms::new(&conn)?.reply()?;

        Ok(Self {
            conn,
            win_id,
            atoms,
        })
    }

    pub fn read() -> Result<String> {
        let clipboard = Self::new().unwrap();
        {
            let cache = CB_DATA.lock().unwrap();
            println!("Getting selection owner for {}", clipboard.atom_name(clipboard.atoms.CLIPBOARD).unwrap());
            let owner_reply = clipboard.conn.get_selection_owner(clipboard.atoms.CLIPBOARD)?.reply()?;
            if owner_reply.owner != 0 && owner_reply.owner == cache.src_win_id {
                println!("Curr owner is identical to cache owner, returning");
                return Ok(cache.data.clone());
            }
        }
        // TODO: handle types other then UTF-8
        // request clipboard data
        println!("Calling `convert_selection`: {}", clipboard.atom_name(clipboard.atoms._ABOARD_SELECTION).unwrap());
        clipboard
            .conn
            .convert_selection(
                clipboard.win_id,
                clipboard.atoms.CLIPBOARD,
                clipboard.atoms.UTF8_STRING,
                clipboard.atoms._ABOARD_SELECTION,
                Time::CURRENT_TIME,
            )
            .unwrap();
        println!("Finished `convert_selection`");
        clipboard.conn.sync().unwrap();
        clipboard.conn.flush().unwrap();

        // read requested data
        loop {
            if let Event::SelectionNotify(event) = clipboard.conn.wait_for_event().unwrap() {
                println!("Event::SelectionNotify: {}", event.property);
                // TODO: handle chunking
                // check if this is what we requested
                if event.selection == clipboard.atoms.CLIPBOARD {
                    //println!("{}", clipboard.atom_name(event.property).unwrap());
                    let reply = clipboard
                        .conn
                        .get_property(
                            true,
                            event.requestor,
                            event.property,
                            event.target,
                            0,
                            0x1fffffff,
                        )
                        .unwrap()
                        .reply();
                    if let Ok(reply) = reply {
                        println!("Property.type: {}", clipboard.atom_name(reply.type_).unwrap());
                        clipboard.conn.sync().unwrap();
                        clipboard.conn.flush().unwrap();
    
                        // we found something
                        if reply.type_ == clipboard.atoms.UTF8_STRING {
                            break Ok(String::from_utf8(reply.value).unwrap());
                        }
                    } else { 
                        println!("reply was Err: {:?}", reply);
                    }
                } else {
                    println!("not what we were looking for")
                }
            } else {
                println!("not the event that we wanted")
            }
        }
    }

    fn atom_name(&self, atom: x11rb::protocol::xproto::Atom) -> Result<String> {
        String::from_utf8(self.conn.get_atom_name(atom)?.reply()?.name).map_err(Into::into)
    }

    fn write_events(
        &self,
        written: &mut bool,
        notified: &mut bool,
        event: Event,
        message: &str,
    ) -> Result<()> {
        let which = if *written { 2 } else { 1 };

        match event {
            Event::SelectionRequest(event) => {
                println!("SelectionRequest: '{}'", self.atom_name(event.target).unwrap());
                if event.target == self.atoms.TARGETS && event.selection == self.atoms.CLIPBOARD {
                    println!("{}: property is {}", which, self.atom_name(event.property)?);
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        event.requestor,
                        event.property,
                        self.atoms.XA_ATOM,
                        &[self.atoms.TARGETS, self.atoms.UTF8_STRING],
                    )?;
                } else if event.target == self.atoms.UTF8_STRING {
                    println!(
                        "{}: writing to target {}",
                        which,
                        self.atom_name(event.property)?
                    );
                    self.conn.change_property8(
                        PropMode::REPLACE,
                        event.requestor,
                        event.property,
                        self.atoms.UTF8_STRING,
                        message.as_bytes(),
                    )?;
                } else {
                    println!("THIS IS NOT SUPPOSED TO HAPPEN");
                    return Ok(());
                }

                self.conn.flush()?;
                self.conn.sync()?;

                self.conn.send_event(
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
                        property: event.property,
                    },
                )?;

                self.conn.flush()?;
                self.conn.sync()?;

                if event.target == self.atoms.UTF8_STRING {
                    *written = true;
                    println!("successfully written!");
                }
            }
            Event::PropertyNotify(event) => {
                let atom = self.atom_name(event.atom).unwrap();
                println!("PropertyNotify: state {:?}, atom: {}", event.state, atom);
            }
            Event::SelectionNotify(event) => {
                println!("SelectionNotify: '{}'", self.atom_name(event.target).unwrap());
                if event.target == self.atoms.SAVE_TARGETS {
                    *notified = true;
                    println!("successfully notified!");
                }
            }
            _ => (),
        }

        Ok(())
    }

    pub fn write(message: &str) -> Result<()> {
        println!();
        println!("new write");
        let clipboard = Self::new()?;

        println!("Setting selection owner to our window.");
        clipboard.conn.set_selection_owner(
            clipboard.win_id,
            clipboard.atoms.CLIPBOARD,
            Time::CURRENT_TIME,
        )?;
        clipboard.conn.flush()?;
        clipboard.conn.sync()?;

        clipboard.conn.delete_property(clipboard.win_id, clipboard.atoms._ABOARD_SELECTION)?;
        println!("Asking the clipboard manager to request the data.");
        clipboard.conn.convert_selection(
            clipboard.win_id,
            clipboard.atoms.CLIPBOARD_MANAGER,
            clipboard.atoms.SAVE_TARGETS,
            clipboard.atoms._ABOARD_SELECTION,
            Time::CURRENT_TIME,
        )?;
        clipboard.conn.flush()?;
        clipboard.conn.sync()?;

        let mut written = false;
        let mut notified = false;

        loop {
            if written && notified {
                let mut cache = CB_DATA.lock().unwrap();
                cache.src_win_id = clipboard.win_id;
                cache.data = message.to_string();
                println!("finished");
                break Ok(());
            }

            clipboard.write_events(
                &mut written,
                &mut notified,
                clipboard.conn.wait_for_event()?,
                message,
            )?;
        }
    }
}

