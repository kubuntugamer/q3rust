//! The client: SDL window and audio, the Vulkan renderer, and the frame loop.
//!
//! Everything here is Rust. SDL is the only platform layer — it owns the
//! window, the Vulkan surface, the event queue and the audio device. There is
//! no `refexport_t`, no renderer module behind a C ABI and no C `main`.
//!
//! The client mounts the game's paks on startup and draws its console with the
//! engine's own charset out of `baseq3`, falling back to the built-in face when
//! there is no data to mount.
//!
//! On Linux the client is Wayland-only. SDL would otherwise fall back to X11
//! when no Wayland compositor answers, but the desktops this targets no longer
//! ship an X11 session, so the fallback is refused instead of quietly used.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ash::vk::{Handle, SurfaceKHR};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;
use sdl2::video::Window;
use sdl2::{EventPump, Sdl, VideoSubsystem};

use crate::assets::Assets;
use crate::audio::Audio;
use crate::cmd::CmdSystem;
use crate::console;
use crate::cvar::{Cvars, CVAR_ARCHIVE};
use crate::font::Face;
use crate::items::ItemField;
use crate::prediction::{Input, Prediction};
use crate::texture;
use crate::vk::{BlendMode, Texture, Vertex2D, Vulkan};
use crate::world::{self, Camera, World};

/// Set by the `quit` command and cleared when the client shuts down.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

fn quit_command(_cmd: &mut CmdSystem) {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

/// `clear`: wipes the console scrollback.
fn clear_command(_cmd: &mut CmdSystem) {
    console::clear();
}

/// `echo`: prints its arguments, which is how the engine's own startup scripts
/// talk to the player.
fn echo_command(cmd: &mut CmdSystem) {
    console::print(&cmd.args_from(1));
}

/// The `console` shader's art: the opaque backdrop and the additive glow that
/// the shader sums over it.
struct ConsoleArt {
    base: texture::Image,
    glow: Option<texture::Image>,
}

/// Media the client draws with but does not own logic for.
struct Media {
    face: Face,
    /// The console backdrop, if the game shipped one.
    console: Option<ConsoleArt>,
    /// One line per thing the loader did, for the console.
    log: Vec<String>,
}

/// Reads an image by the name a shader gives it, falling back from `.tga` to
/// the `.jpg` beside it the way `R_LoadImage` does: `gfx/misc/console02.tga`
/// is shipped as `console02.jpg`, and the stock game relies on that.
fn read_image(assets: &Assets, name: &str) -> Result<(texture::Image, String), String> {
    let mut candidates = vec![name.to_string()];
    if name.to_ascii_lowercase().ends_with(".tga") {
        let mut jpeg = name.to_string();
        jpeg.replace_range(name.len() - 3.., "jpg");
        candidates.push(jpeg);
    }

    let mut last = String::new();
    for candidate in candidates {
        let Some(bytes) = assets.read(&candidate) else {
            last = format!("{candidate}: not in any pak");
            continue;
        };
        let decoded = if candidate.to_ascii_lowercase().ends_with(".jpg") {
            texture::jpeg(&bytes)
        } else {
            texture::tga(&bytes)
        };
        match decoded {
            Ok(image) => return Ok((image, candidate)),
            Err(e) => last = format!("{candidate}: {e}"),
        }
    }
    Err(last)
}

/// Reads the charset and the console backdrop out of the mounted paks.
///
/// A missing charset is not fatal: the console still works with the built-in
/// face, which is the whole point of having one.
fn load_media(assets: Option<&Assets>) -> Media {
    let mut log = Vec::new();

    let face = assets
        .and_then(|assets| match read_image(assets, "gfx/2d/bigchars.tga") {
            Ok((image, from)) => {
                match Face::from_charset(image.width, image.height, image.rgba) {
                    Ok(face) => {
                        log.push(format!(
                            "charset: {from}, {}x{}",
                            image.width, image.height
                        ));
                        Some(face)
                    }
                    Err(e) => {
                        log.push(format!("charset unusable ({e}); using the built-in face"));
                        None
                    }
                }
            }
            Err(e) => {
                log.push(format!("charset will not load ({e}); using the built-in face"));
                None
            }
        })
        .unwrap_or_else(|| {
            log.push("charset: built-in 5x7 face (no game data)".to_string());
            Face::builtin()
        });

    // The `console` shader's stages: the backdrop, then an additive glow. A
    // missing glow is not an error; the backdrop alone still looks right.
    let console = assets.and_then(|assets| {
        let base = match read_image(assets, "gfx/misc/console01.tga") {
            Ok((image, from)) => {
                log.push(format!(
                    "console backdrop: {from}, {}x{}",
                    image.width, image.height
                ));
                image
            }
            Err(e) => {
                log.push(format!("console backdrop will not load: {e}"));
                return None;
            }
        };
        let glow = match read_image(assets, "gfx/misc/console02.tga") {
            Ok((image, from)) => {
                log.push(format!(
                    "console glow: {from}, {}x{}",
                    image.width, image.height
                ));
                Some(image)
            }
            Err(e) => {
                log.push(format!("console glow will not load: {e}"));
                None
            }
        };
        Some(ConsoleArt { base, glow })
    });

    Media {
        face,
        console,
        log,
    }
}

/// The renderer as the client sees it: a swapchain plus the media it needs.
pub struct Renderer {
    vk: Vulkan,
    white: Texture,
    font: Texture,
    face: Face,
    /// The `console` shader's stages, once uploaded.
    backdrop: Option<Backdrop>,
}

/// The console shader's two stages as textures: the opaque backdrop and the
/// additive glow shot through it.
struct Backdrop {
    base: Texture,
    glow: Option<Texture>,
}

impl Renderer {
    fn new(
        entry: ash::Entry,
        instance: ash::Instance,
        surface: SurfaceKHR,
        width: u32,
        height: u32,
        face: Face,
        console: Option<ConsoleArt>,
    ) -> Result<Self, String> {
        let mut vk = Vulkan::new(entry, instance, surface, width, height)?;
        let white = vk.create_texture(1, 1, &[255, 255, 255, 255])?;
        let font = vk.create_texture(face.atlas_width, face.atlas_height, &face.atlas)?;
        // The console stages repeat: their `tcMod scale 2 1` runs the texture
        // coordinates past 1.0, and unclamped shader images wrap.
        let backdrop = match console.as_ref() {
            Some(art) => {
                let base = vk.create_repeating_texture(art.base.width, art.base.height, &art.base.rgba)?;
                let glow = match art.glow.as_ref() {
                    Some(glow) => {
                        Some(vk.create_repeating_texture(glow.width, glow.height, &glow.rgba)?)
                    }
                    None => None,
                };
                Some(Backdrop { base, glow })
            }
            None => None,
        };
        Ok(Renderer {
            vk,
            white,
            font,
            face,
            backdrop,
        })
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        self.vk.resize(width, height)
    }

    fn size(&self) -> (u32, u32) {
        let extent = self.vk.extent();
        (extent.width, extent.height)
    }

    fn device_name(&self) -> String {
        self.vk.device_name()
    }

    fn face(&self) -> &Face {
        &self.face
    }

    /// Fills a rectangle in screen pixels with a colour, through the same
    /// textured pipeline everything else uses.
    fn fill(&mut self, x: f32, y: f32, w: f32, h: f32, color: [u8; 4]) -> Result<(), String> {
        let (x1, y1) = (x + w, y + h);
        let vertex = |px: f32, py: f32| Vertex2D {
            pos: [px, py],
            uv: [0.5, 0.5],
            color,
        };
        let quad = [
            vertex(x, y),
            vertex(x1, y),
            vertex(x1, y1),
            vertex(x, y),
            vertex(x1, y1),
            vertex(x, y1),
        ];
        self.vk.draw_quads(&self.white, &quad)
    }

    /// Draws one line of text, as a single batch.
    fn text(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        scale: f32,
        color: [u8; 4],
    ) -> Result<(), String> {
        let mut quads = Vec::with_capacity(text.chars().count() * 6);
        self.face.push_text(&mut quads, text, x, y, scale, color);
        self.vk.draw_quads(&self.font, &quads)
    }

    /// Draws the console backdrop over the window, following the `console`
    /// shader in `scripts/gfx.shader`: `console01.tga` stretched over the
    /// console with `tcMod scroll .02 0` and `tcMod scale 2 1`, then
    /// `console02` added on top with `tcMod turb 0 .1 0 .1`, the same scale
    /// and `tcMod scroll 0.2 .1`. Returns `false` when the game shipped no
    /// backdrop, so the caller can fill a flat colour instead.
    fn draw_backdrop(&mut self, width: f32, height: f32, seconds: f32) -> Result<bool, String> {
        let Renderer { vk, backdrop, .. } = self;
        let Some(Backdrop { base, glow }) = backdrop else {
            return Ok(false);
        };
        let color = [255, 255, 255, 255];

        // `tcMod scroll .02 0` then `tcMod scale 2 1`: the image repeats
        // twice across the console and drifts sideways twice as fast as the
        // raw scroll rate.
        let scrolled = (0.02 * seconds).fract();
        let base_quad = quad(
            width,
            height,
            |_x, _y, u, v| [(u + scrolled) * 2.0, v],
            color,
        );
        vk.draw_quads(base, &base_quad)?;

        // The glow's turbulence is a sine of the vertex's virtual-screen
        // position, exactly as `RB_CalcTurbulentTexCoords` computes it, so
        // the 640x480 space the shader draws in has to come back.
        if let Some(glow) = glow {
            let now = 0.1 * seconds;
            let scroll = [(0.2 * seconds).fract(), (0.1 * seconds).fract()];
            let glow_quad = quad(
                width,
                height,
                |x, y, u, v| {
                    let (x, y) = (x * 640.0 / width, y * 480.0 / height);
                    let turn = std::f32::consts::TAU;
                    let s = u + (turn * (x / 128.0 * 0.125 + now)).sin() * 0.1;
                    let t = v + (turn * (y / 128.0 * 0.125 + now)).sin() * 0.1;
                    [s * 2.0 + scroll[0], t + scroll[1]]
                },
                color,
            );
            vk.draw_quads_additive(glow, &glow_quad)?;
        }
        Ok(true)
    }

    fn shutdown(self) {
        let Renderer {
            mut vk,
            white,
            font,
            backdrop,
            ..
        } = self;
        if let Some(Backdrop { base, glow }) = backdrop {
            vk.destroy_texture(base);
            if let Some(glow) = glow {
                vk.destroy_texture(glow);
            }
        }
        vk.destroy_texture(white);
        vk.destroy_texture(font);
    }
}

/// The console's quad, with a stage's `tcMod`s applied to the texture
/// coordinates: `stretch` gets the corner position, its base 0..1 texture
/// coordinate, and returns the coordinate to sample.
fn quad(
    width: f32,
    height: f32,
    stretch: impl Fn(f32, f32, f32, f32) -> [f32; 2],
    color: [u8; 4],
) -> [Vertex2D; 6] {
    let corner = |x: f32, y: f32, u: f32, v: f32| Vertex2D {
        pos: [x, y],
        uv: stretch(x, y, u, v),
        color,
    };
    [
        corner(0.0, 0.0, 0.0, 0.0),
        corner(width, 0.0, 1.0, 0.0),
        corner(width, height, 1.0, 1.0),
        corner(0.0, 0.0, 0.0, 0.0),
        corner(width, height, 1.0, 1.0),
        corner(0.0, height, 0.0, 1.0),
    ]
}

/// Engine state that survives a frame: SDL, the command buffer, the cvars, the
/// renderer and the clock.
pub struct Client {
    _sdl: Sdl,
    _video: VideoSubsystem,
    _window: Window,
    event_pump: EventPump,
    audio: Option<Audio>,
    cmd: CmdSystem,
    cvars: Cvars,
    renderer: Renderer,
    start: Instant,
    last_frame: Instant,
    frames: u64,
    /// Rows of console text that fit on screen, from the last frame. Page
    /// keys scroll by this much.
    console_rows: usize,
    /// The predicted player, when a map loaded.
    prediction: Option<Prediction>,
    /// The map's static geometry, once it uploaded.
    world: Option<World>,
    /// The map's pickups and which of them are on the map right now.
    items: Option<ItemField>,
    /// Movement keys held this frame.
    input: Input,
}

impl Client {
    /// Brings the subsystems up: the game data, SDL video and audio, the window
    /// and its Vulkan surface, then the renderer on that surface.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        QUIT_REQUESTED.store(false, Ordering::SeqCst);

        // A TV wants the panel's own mode. `Q3_WIDTH`/`Q3_HEIGHT` pick a size
        // and `Q3_FULLSCREEN=1` asks for borderless desktop fullscreen, which
        // SDL maps to the native mode without a mode switch.
        let fullscreen = std::env::var("Q3_FULLSCREEN")
            .map(|value| value != "0")
            .unwrap_or(false);
        // A typo or an impossible size must not build a window the desktop
        // cannot show: a fullscreen window at a mode the panel lacks is how a
        // resolution test takes the whole session with it. The request is
        // clamped, and what the surface grants is what gets drawn and reported.
        const MIN_DIMENSION: u32 = 320;
        const MAX_DIMENSION: u32 = 16384;
        let requested = |key: &str, fallback: u32| -> u32 {
            std::env::var(key)
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .map(|value| value.clamp(MIN_DIMENSION, MAX_DIMENSION))
                .unwrap_or(fallback)
        };
        let requested_width = requested("Q3_WIDTH", width);
        let requested_height = requested("Q3_HEIGHT", height);
        let (mut width, mut height) = (requested_width, requested_height);

        // Game data first: the charset and the console backdrop decide what the
        // first frame looks like.
        let (assets, media) = match Assets::mount_default() {
            Ok(assets) => {
                let media = load_media(Some(&assets));
                (Some(assets), media)
            }
            Err(e) => {
                let mut media = load_media(None);
                media.log.push(format!("game data: {e}"));
                (None, media)
            }
        };
        let Media { face, console, log } = media;

        // The map to predict and draw: `$Q3_MAP` overrides the stock first
        // map. The bytes are kept for the renderer as well.
        let map_name = std::env::var("Q3_MAP").unwrap_or_else(|_| "q3dm1".to_string());
        let mut map_line = "map: none (no game data)".to_string();
        let prediction = assets.as_ref().and_then(|assets| {
            match Prediction::load(assets, &map_name) {
                Ok(prediction) => {
                    map_line = format!("map: {}", prediction.map());
                    Some(prediction)
                }
                Err(e) => {
                    map_line = format!("map: {e}");
                    None
                }
            }
        });
        let map_bytes = assets.as_ref().and_then(|assets| {
            assets
                .read(&format!("maps/{map_name}.bsp"))
                .map(|bytes| bytes.into_owned())
        });

        // Wayland only on Linux. The hint has to be set before SDL brings the
        // video subsystem up, and the driver is checked afterwards so an
        // explicit `SDL_VIDEODRIVER=x11` cannot sneak the X11 session back in.
        #[cfg(target_os = "linux")]
        if std::env::var_os("SDL_VIDEODRIVER").is_none() {
            sdl2::hint::set("SDL_VIDEODRIVER", "wayland");
        }
        let sdl = sdl2::init()?;
        let video = sdl.video()?;
        #[cfg(target_os = "linux")]
        {
            let driver = video.current_video_driver();
            if driver != "wayland" {
                return Err(format!(
                    "this client requires Wayland; SDL started the {driver} video driver"
                ));
            }
        }
        // The panel's own bounds: asking for more than the desktop has would
        // otherwise give a window that covers every output at once.
        let mut clamped_to_desktop = false;
        if let Ok(mode) = video.desktop_display_mode(0) {
            let desktop_width = mode.w.max(1) as u32;
            let desktop_height = mode.h.max(1) as u32;
            if width > desktop_width || height > desktop_height {
                width = width.min(desktop_width);
                height = height.min(desktop_height);
                clamped_to_desktop = true;
            }
        }

        let mut window_builder = video.window("quake3 (rust)", width, height);
        window_builder.vulkan().position_centered();
        if fullscreen {
            window_builder.fullscreen_desktop();
        }
        let window = window_builder
            .build()
            .map_err(|e| format!("creating the window: {e}"))?;
        let event_pump = sdl.event_pump()?;

        // Relative motion is what mouse look needs; the console releases the
        // pointer while it is open.
        let _ = sdl.mouse().set_relative_mouse_mode(true);

        let audio = match Audio::init(&sdl) {
            Ok(audio) => Some(audio),
            Err(e) => {
                eprintln!("audio disabled: {e}");
                None
            }
        };

        let extensions = window.vulkan_instance_extensions()?;
        let (entry, instance) = crate::vk::create_instance(&extensions)?;
        let surface = window
            .vulkan_create_surface(instance.handle().as_raw() as usize)
            .map_err(|e| format!("creating the Vulkan surface: {e}"))?;
        let mut renderer = Renderer::new(
            entry,
            instance,
            SurfaceKHR::from_raw(surface),
            width,
            height,
            face,
            console,
        )?;

        // The world is uploaded after the renderer exists, since its batches
        // live in device memory. A map that will not parse still leaves the
        // console usable.
        let mut world = None;
        if let (Some(bytes), Some(assets)) = (map_bytes.as_deref(), assets.as_ref()) {
            match World::build(&mut renderer.vk, assets, bytes) {
                Ok(built) => {
                    println!(
                        "world: {} surfaces, {} triangles, {} textures, {} shaders without art, {} items",
                        built.drawn,
                        built.vertices.len() / 3,
                        built.textures().len(),
                        built.missing.len(),
                        built.items.len()
                    );
                    world = Some(built);
                }
                Err(e) => console::print(&format!("world: {e}")),
            }
        }

        // The live pickups, from the same marks the world drew its boxes for.
        let items = world.as_ref().map(|world| ItemField::new(&world.items));

        let mut cmd = CmdSystem::new();
        cmd.add_command("quit", Some(quit_command));
        cmd.add_command("clear", Some(clear_command));
        cmd.add_command("echo", Some(echo_command));

        let mut cvars = Cvars::new();
        cvars.get("com_maxfps", "125", CVAR_ARCHIVE);
        cvars.get("com_timescale", "1", 0);
        cvars.get("name", "Player", CVAR_ARCHIVE);
        cvars.get("rate", "25000", CVAR_ARCHIVE);
        cvars.get("cg_fov", "90", CVAR_ARCHIVE);
        cvars.get("sensitivity", "5", CVAR_ARCHIVE);
        cvars.get("r_fullscreen", if fullscreen { "1" } else { "0" }, CVAR_ARCHIVE);

        let audio_line = match audio.as_ref() {
            Some(audio) => {
                let spec = audio.spec();
                format!(
                    "audio: {} Hz, {} channels, {} samples",
                    spec.freq, spec.channels, spec.samples
                )
            }
            None => "audio: unavailable".to_string(),
        };
        // The swapchain has the final say on the size; report that rather than
        // the request, so a mode the panel does not have stays visible.
        let (granted_width, granted_height) = renderer.size();
        cvars.get("r_width", &granted_width.to_string(), CVAR_ARCHIVE);
        cvars.get("r_height", &granted_height.to_string(), CVAR_ARCHIVE);
        let mut display_line = format!("display: {granted_width}x{granted_height}");
        if fullscreen {
            display_line.push_str(", fullscreen desktop; the panel picks the mode");
        } else if clamped_to_desktop {
            display_line.push_str(&format!(
                " (Q3_WIDTH/Q3_HEIGHT asked {requested_width}x{requested_height}, clamped to the desktop)"
            ));
        } else if granted_width != requested_width || granted_height != requested_height {
            display_line.push_str(&format!(
                " (asked {requested_width}x{requested_height}; the surface granted its own size)"
            ));
        }

        let data_line = match assets.as_ref() {
            Some(assets) => format!(
                "game data: {} paks, {:.1} MB, from {}",
                assets.len(),
                assets.bytes() as f64 / (1024.0 * 1024.0),
                assets.source_directory().display()
            ),
            None => "game data: none mounted".to_string(),
        };
        let mut banner = format!(
            "Quake III Arena - Rust client {}\nrenderer: Vulkan, {}\n{audio_line}\n{data_line}\
             \n{display_line}\n",
            crate::version(),
            renderer.device_name(),
        );
        banner.push_str(&map_line);
        banner.push('\n');
        if let Some(world) = world.as_ref() {
            banner.push_str(&format!(
                "world: {} surfaces, {} triangles, {} textures\n",
                world.drawn,
                world.vertices.len() / 3,
                world.textures().len()
            ));
            if !world.missing.is_empty() {
                banner.push_str(&format!(
                    "world: {} shaders had no image; drawing the checker\n",
                    world.missing.len()
                ));
            }
        }
        for line in &log {
            banner.push_str(line);
            banner.push('\n');
        }
        for error in assets.as_ref().map(|assets| assets.errors()).unwrap_or(&[]) {
            banner.push_str(&format!("pak problem: {error}\n"));
        }
        banner.push_str(
            "console open: Esc closes it, Tab completes, Up/Down walks history\n\
             move: WASD, jump: Space, turn: Left/Right, look: mouse\n\
             display: Q3_WIDTH/Q3_HEIGHT pick a resolution, Q3_FULLSCREEN=1 a native TV mode\n\
             cg_fov widens with the aspect ratio; the vertical view stays 4:3",
        );
        console::print(&banner);

        Ok(Client {
            _sdl: sdl,
            _video: video,
            _window: window,
            event_pump,
            audio,
            cmd,
            cvars,
            renderer,
            start: Instant::now(),
            last_frame: Instant::now(),
            frames: 0,
            console_rows: 1,
            prediction,
            world,
            items,
            input: Input::default(),
        })
    }

    /// Milliseconds since startup, the client's `Sys_Milliseconds` equivalent.
    pub fn millis(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn quit_requested(&self) -> bool {
        QUIT_REQUESTED.load(Ordering::SeqCst)
    }

    /// Runs one command line, as the console or a config file would.
    pub fn execute(&mut self, text: &str) {
        self.cmd.execute_string(text);
    }

    /// Echoes the pending input line and runs it. The console lock is released
    /// before the command runs, so a command that prints cannot deadlock.
    fn submit_input(&mut self) {
        let line = console::lock().take_line();
        if let Some(line) = line {
            console::print(&format!("] {line}"));
            self.cmd.execute_string(&line);
        }
    }

    /// Completes the first word against the command table: a unique match is
    /// filled in, several are listed.
    fn complete(&mut self) {
        let mut console = console::lock();
        let typed = console.input();
        let word = typed.split_whitespace().next().unwrap_or("").to_string();
        if word.is_empty() {
            return;
        }

        let mut matches = Vec::new();
        self.cmd.command_completion(|name| {
            if name.to_ascii_lowercase().starts_with(&word.to_ascii_lowercase()) {
                matches.push(name.to_string());
            }
        });
        matches.sort();

        if matches.len() == 1 {
            console.set_input(&format!("{} ", matches[0]));
        } else if !matches.is_empty() {
            console.print(&matches.join(" "));
        }
    }

    /// Grabs the pointer for mouse look, and gives it back while the console
    /// is open so the cursor can be used there.
    fn sync_mouse(&self) {
        let open = console::lock().is_open();
        let _ = self._sdl.mouse().set_relative_mouse_mode(!open);
    }

    /// Routes a movement key. Returns `false` when the console owns the key,
    /// so the caller can fall through to the console's own bindings.
    fn movement_key(&mut self, key: Keycode, down: bool) -> bool {
        if console::lock().is_open() {
            return false;
        }
        let slot = match key {
            Keycode::W => &mut self.input.forward,
            Keycode::S => &mut self.input.back,
            Keycode::A => &mut self.input.left,
            Keycode::D => &mut self.input.right,
            Keycode::Space => &mut self.input.jump,
            Keycode::Left => &mut self.input.turn_left,
            Keycode::Right => &mut self.input.turn_right,
            _ => return false,
        };
        *slot = down;
        true
    }

    /// Routes one key press into the console. Returns `false` when the client
    /// should exit.
    fn key(&mut self, key: Keycode, repeat: bool) -> bool {
        // Page keys scroll even when the console is closed, as the C client
        // does, so they are handled before the open check.
        match key {
            Keycode::PageUp => {
                console::lock().scroll_up(self.console_rows.saturating_sub(1).max(1));
                return true;
            }
            Keycode::PageDown => {
                console::lock().scroll_down(self.console_rows.saturating_sub(1).max(1));
                return true;
            }
            _ => {}
        }

        if !console::lock().is_open() {
            if key == Keycode::Escape && !repeat {
                // Keys held when the console opens would otherwise stay down.
                self.input = Input::default();
                console::lock().set_open(true);
                self.sync_mouse();
            }
            return true;
        }

        match key {
            Keycode::Escape => {
                if !repeat {
                    console::lock().toggle();
                    self.sync_mouse();
                }
            }
            Keycode::Return | Keycode::KpEnter => self.submit_input(),
            Keycode::Backspace => console::lock().pop_char(),
            Keycode::Delete => console::lock().delete_char(),
            Keycode::Left => console::lock().move_left(),
            Keycode::Right => console::lock().move_right(),
            Keycode::Home => console::lock().move_home(),
            Keycode::End => console::lock().move_end(),
            Keycode::Up => console::lock().history_previous(),
            Keycode::Down => console::lock().history_next(),
            Keycode::Tab => self.complete(),
            _ => {}
        }
        true
    }

    /// Handles the pending SDL events. Returns `false` when the client should
    /// exit.
    fn pump_events(&mut self) -> Result<bool, String> {
        // Collected first: handling an event borrows `self` mutably, which the
        // live iterator would not allow.
        let events: Vec<Event> = self.event_pump.poll_iter().collect();
        for event in events {
            match event {
                Event::Quit { .. } => return Ok(false),
                Event::Window {
                    win_event: WindowEvent::Resized(width, height)
                    | WindowEvent::SizeChanged(width, height),
                    ..
                } => {
                    self.renderer.resize(width.max(1) as u32, height.max(1) as u32)?;
                }
                Event::KeyDown {
                    keycode: Some(key),
                    repeat,
                    ..
                } => {
                    if !self.movement_key(key, true) && !self.key(key, repeat) {
                        return Ok(false);
                    }
                }
                Event::KeyUp {
                    keycode: Some(key), ..
                } => {
                    self.movement_key(key, false);
                }
                Event::MouseMotion { xrel, yrel, .. } => {
                    if !console::lock().is_open() {
                        let sensitivity = self
                            .cvars
                            .find("sensitivity")
                            .map(|cvar| cvar.value)
                            .unwrap_or(5.0);
                        // `m_yaw` and `m_pitch` are 0.022 in the C client.
                        let scale = sensitivity * 0.022;
                        if let Some(prediction) = self.prediction.as_mut() {
                            prediction.turn(-(xrel as f32) * scale, (yrel as f32) * scale);
                        }
                    }
                }
                Event::TextInput { text, .. } => {
                    let mut console = console::lock();
                    if console.is_open() {
                        for ch in text.chars() {
                            console.insert_char(ch);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(true)
    }

    /// Draws the map under the console, through the depth-tested world
    /// pipeline. The camera sits at the predicted origin at eye height.
    fn draw_world(&mut self) -> Result<(), String> {
        let Some(world) = self.world.as_ref() else {
            return Ok(());
        };
        let Some(prediction) = self.prediction.as_ref() else {
            return Ok(());
        };

        let origin = prediction.origin();
        let camera = Camera {
            origin: [
                origin[0],
                origin[1],
                origin[2] + world::DEFAULT_VIEW_HEIGHT,
            ],
            yaw: prediction.yaw(),
            pitch: prediction.pitch(),
        };
        let fov = self
            .cvars
            .find("cg_fov")
            .map(|cvar| cvar.value)
            .unwrap_or(world::DEFAULT_FOV);
        let (width, height) = self.renderer.size();
        let aspect = width as f32 / height.max(1) as f32;
        let mvp = world::view_projection(&camera, aspect, fov);

        // The skybox first: it writes no depth, so the map draws over it.
        if let Some(sky) = world.sky.as_ref() {
            sky.draw(&mut self.renderer.vk, &camera, &mvp)?;
        }

        // A scrolling texmod is the one part of a stage that needs the clock.
        let seconds = self.millis() as f32 / 1000.0;
        for batch in &world.batches {
            let vertices = &world.vertices[batch.first..batch.first + batch.count];
            for stage in &batch.stages {
                let (_, texture) = &world.textures()[stage.texture];
                let uv = [
                    stage.uv[0],
                    stage.uv[1],
                    stage.uv[2] * seconds,
                    stage.uv[3] * seconds,
                ];
                self.renderer
                    .vk
                    .draw_stage(texture, vertices, &mvp, uv, stage.blend, stage.lit)?;
            }
        }

        // The pickups still on the map: a box each, coloured by kind. The
        // colour rides in the vertex colour, which is the lit path's tint, and
        // the white texture leaves it alone.
        if let Some(field) = self.items.as_ref() {
            let vertices = field.vertices();
            if !vertices.is_empty() {
                self.renderer.vk.draw_stage(
                    &self.renderer.white,
                    vertices,
                    &mvp,
                    [1.0, 1.0, 0.0, 0.0],
                    BlendMode::Opaque,
                    true,
                )?;
            }
        }
        Ok(())
    }

    /// Draws the console: the backdrop, the scrollback, and the input line with
    /// its caret.
    fn draw_console(&mut self) -> Result<(), String> {
        let (width, height) = self.renderer.size();
        let (width, height) = (width as f32, height as f32);
        let scale = self.renderer.face().default_scale(height as u32);
        let advance = self.renderer.face().advance(scale);
        let line_height = self.renderer.face().line_height(scale);

        let columns = (((width - 16.0) / advance).floor() as usize).max(8);
        console::lock().set_columns(columns);

        let rows = (((height - line_height * 2.0) / line_height).floor() as usize).max(1);
        self.console_rows = rows;

        let console = console::lock();
        if !console.is_open() {
            return Ok(());
        }

        let seconds = self.millis() as f32 / 1000.0;
        if !self.renderer.draw_backdrop(width, height, seconds)? {
            self.renderer.fill(0.0, 0.0, width, height, [8, 8, 10, 255])?;
        }

        let margin = 8.0;
        let mut y = margin;
        for line in console.visible(rows) {
            self.renderer
                .text(line, margin, y, scale, [235, 235, 235, 255])?;
            y += line_height;
        }

        // The input line, with the prompt the C console uses.
        let prompt = "] ";
        let input = console.input();
        let cursor = console.cursor();
        self.renderer
            .text(prompt, margin, y, scale, [160, 200, 160, 255])?;
        self.renderer.text(
            &input,
            margin + advance * prompt.len() as f32,
            y,
            scale,
            [255, 255, 255, 255],
        )?;

        // A caret over the character it sits before, blinking at two hertz.
        if (self.millis() / 500) % 2 == 0 {
            let caret_x = margin + advance * (prompt.len() + cursor) as f32;
            self.renderer.fill(
                caret_x,
                y,
                2.0,
                self.renderer.face().line_height(scale),
                [235, 235, 235, 255],
            )?;
        }

        Ok(())
    }

    /// One line of predicted position while the console is closed, so live
    /// movement is visible without a 3D view.
    fn draw_status(&mut self) -> Result<(), String> {
        if console::lock().is_open() {
            return Ok(());
        }
        let Some(prediction) = self.prediction.as_ref() else {
            return Ok(());
        };
        let (_, height) = self.renderer.size();
        let scale = self.renderer.face().default_scale(height) * 0.75;
        let line_height = self.renderer.face().line_height(scale);

        // The player's own numbers first: health, armour and the clip in hand.
        let status = format!(
            "HEALTH {}   ARMOR {}   AMMO {}",
            prediction.health(),
            prediction.armor(),
            prediction.ammo()
        );
        self.renderer
            .text(&status, 8.0, 8.0, scale, [235, 235, 235, 255])?;

        let [x, y, z] = prediction.origin();
        let line = format!(
            "{x:.0} {y:.0} {z:.0}   {:.0} u/s   {}",
            prediction.speed(),
            if prediction.on_ground() { "ground" } else { "air" }
        );
        self.renderer.text(
            &line,
            8.0,
            8.0 + line_height,
            scale,
            [180, 180, 180, 255],
        )
    }

    /// Handles the pending SDL events and draws one frame. Returns `false`
    /// when the client should exit.
    pub fn frame(&mut self) -> Result<bool, String> {
        if !self.pump_events()? {
            return Ok(false);
        }
        if self.quit_requested() {
            return Ok(false);
        }

        let now = self.millis();
        if let Some(prediction) = self.prediction.as_mut() {
            prediction.step(now, &self.input);
        }
        // Bring back what has waited long enough, then take what the player is
        // standing in.
        if let (Some(prediction), Some(field)) = (self.prediction.as_mut(), self.items.as_mut()) {
            field.refresh(now);
            for class_name in prediction.collect(field, now) {
                console::print(&format!("picked up {class_name}"));
            }
        }

        self.renderer.vk.begin_frame([0.0, 0.0, 0.0, 1.0])?;
        self.draw_world()?;
        self.draw_console()?;
        self.draw_status()?;
        self.renderer.vk.end_frame()?;

        self.frames += 1;
        self.last_frame = Instant::now();

        // Frame limiter, from com_maxfps.
        let maxfps = self
            .cvars
            .find("com_maxfps")
            .map(|cvar| cvar.integer)
            .unwrap_or(0);
        if maxfps > 0 {
            let budget = Duration::from_secs_f64(1.0 / maxfps as f64);
            if let Some(remaining) = budget.checked_sub(self.last_frame.elapsed()) {
                std::thread::sleep(remaining);
            }
        }
        Ok(true)
    }

    /// Releases the renderer, its media and the audio device.
    pub fn shutdown(self) {
        let Client {
            mut renderer,
            world,
            audio,
            ..
        } = self;
        drop(audio);
        // The world's textures belong to the renderer, so they go first.
        if let Some(world) = world {
            world.shutdown(&mut renderer.vk);
        }
        renderer.shutdown();
        QUIT_REQUESTED.store(false, Ordering::SeqCst);
    }
}
