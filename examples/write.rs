use std::{
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use smol_clip::Clipboard;

fn main() -> Result<()> {
    let now = Instant::now();
    let mut i = 0;
    loop {
        for _ in 0..20 {
            i += 1;
            let text = format!("yoyo-{}", i);
            println!("\n-- Writing: '{}'", text);
            Clipboard::write(&text)?;

            // thread::sleep(Duration::from_millis(1));
            // println!("\nReading");
            assert_eq!(text, Clipboard::read()?);
            // println!("\nReading done");
        }

        // thread::sleep(Duration::from_millis(100));

        // assert_eq!("yoyoyo", Clipboard::read()?);
        println!("[{}]: success", now.elapsed().as_millis())
    }
}
