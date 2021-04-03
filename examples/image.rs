use arboard::ImageData;
use simple_logger::SimpleLogger;
use smol_clip::Clipboard;

fn main() {
    let logger = SimpleLogger::new().init().unwrap();

    let ctx = Clipboard::new().unwrap();

    #[rustfmt::skip]
	let bytes = [
		255, 100, 100, 255,
		100, 255, 100, 100,
		100, 100, 255, 100,
		0, 0, 0, 255,
	];
    let img_data = ImageData {
        width: 2,
        height: 2,
        bytes: bytes.as_ref().into(),
    };
    // ctx.set_image(img_data).unwrap();
    // std::thread::sleep(std::time::Duration::from_secs(5));

    let img = ctx.get_image().unwrap();
    println!("Received bytes: {:?}", img.bytes);
}
