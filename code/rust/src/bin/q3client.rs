//! The engine's entry point: a Rust `main` over the Rust client.
//!
//! SDL owns the window, the event queue and the audio device; Vulkan draws.

use q3rust::client::Client;

const DEFAULT_WIDTH: u32 = 1024;
const DEFAULT_HEIGHT: u32 = 768;

fn main() {
    // `Q3_FRAMES` exits after that many frames, for automated runs.
    let frame_limit: Option<u64> = std::env::var("Q3_FRAMES")
        .ok()
        .and_then(|value| value.parse().ok());

    let mut client = match Client::new(DEFAULT_WIDTH, DEFAULT_HEIGHT) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("starting the client: {e}");
            std::process::exit(1);
        }
    };

    loop {
        match client.frame() {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                eprintln!("drawing a frame: {e}");
                break;
            }
        }
        if let Some(limit) = frame_limit {
            if client.frames() >= limit {
                break;
            }
        }
    }

    println!(
        "stopped after {} frames, {} ms",
        client.frames(),
        client.millis()
    );
    client.shutdown();
}
