use std::{
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use smol_clip::Clipboard;

fn main() -> Result<()> {
    let now = Instant::now();

    loop {
        for _ in 0..20 {
            Clipboard::write("yoyoyo")?;
            // thread::sleep(Duration::from_millis(1));
            // println!("\nReading");
            assert_eq!("yoyoyo", Clipboard::read()?);
            // println!("\nReading done");
        }

        // thread::sleep(Duration::from_millis(100));

        assert_eq!("yoyoyo", Clipboard::read()?);
        println!("[{}]: success", now.elapsed().as_millis())
    }
}
