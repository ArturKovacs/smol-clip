use std::{
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use log::LevelFilter;
use smol_clip::Clipboard;

use simple_logger::SimpleLogger;

fn main() -> Result<()> {
    let _logger = SimpleLogger::new()
        .with_level(LevelFilter::Trace)
        .init()
        .unwrap();
    let clipboard = Clipboard::new()?;
    let img = clipboard.get_image()?;
    println!("Received {:#} bytes", img.bytes.len());
    return Ok(());
}
