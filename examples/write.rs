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
    clipboard.write("Hello Clipboard manager!".into())?;
    thread::sleep(Duration::from_secs(6));
    assert_eq!("Hello Clipboard manager!", clipboard.read()?);
    return Ok(());

    let now = Instant::now();
    let mut i = 0;
    let clipboard = Clipboard::new()?;
    loop {
        for _ in 0..20 {
            i += 1;
            let text = format!("yoyo-{}", i);
            println!("\n-- Writing: '{}'", text);
            clipboard.write(text.to_owned())?;

            // thread::sleep(Duration::from_millis(100));
            // println!("\nReading");
            assert_eq!(text, clipboard.read()?);
            // println!("\nReading done");
        }

        println!("Sleeping a bit");
        thread::sleep(Duration::from_secs(6));

        // assert_eq!("yoyoyo", Clipboard::read()?);
        println!("[{}]: success", now.elapsed().as_millis())
    }
}
