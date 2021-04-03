use std::{
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use smol_clip::Clipboard;

use simple_logger::SimpleLogger;

fn main() -> Result<()> {
    // Test clipboard manager
    let _logger = SimpleLogger::new().init().unwrap();
    let clipboard = Clipboard::new()?;
    clipboard.set_text("Hello Clipboard manager!".into())?;
    thread::sleep(Duration::from_secs(6));
    assert!(Clipboard::is_alive());
    assert_eq!("Hello Clipboard manager!", clipboard.get_text()?);
    return Ok(());

    let now = Instant::now();
    let mut i = 0;
    let clipboard = Clipboard::new()?;
    loop {
        for _ in 0..20 {
            i += 1;
            let text = format!("yoyo-{}", i);
            println!("\n-- Writing: '{}'", text);
            clipboard.set_text(text.to_owned())?;

            // thread::sleep(Duration::from_millis(100));
            // println!("\nReading");
            assert_eq!(text, clipboard.get_text()?);
            // println!("\nReading done");
        }

        println!("Sleeping a bit");
        thread::sleep(Duration::from_secs(6));

        // assert_eq!("yoyoyo", Clipboard::read()?);
        println!("[{}]: success", now.elapsed().as_millis())
    }
}
