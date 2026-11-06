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
use std::sync::Arc;
use std::time::{Duration, Instant};

use ash::vk::{Handle, SurfaceKHR};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;
use sdl2::mouse::MouseButton;
use sdl2::video::Window;
use sdl2::{EventPump, Sdl, VideoSubsystem};

use crate::assets::Assets;
use crate::audio::Audio;
use crate::bg_misc;
use crate::bg_pmove::{
    WP_BFG, WP_GAUNTLET, WP_GRENADE_LAUNCHER, WP_LIGHTNING,    WP_MACHINEGUN, WP_ROCKET_LAUNCHER, WP_SHOTGUN, WP_PLASMAGUN, WP_RAILGUN,
};
use crate::cmd::CmdSystem;
use crate::console;
use crate::cvar::{Cvars, CVAR_ARCHIVE};
use crate::font::Face;
use crate::items::{self, ItemField};
use crate::prediction::{Input, Prediction};
use crate::targets::TargetField;
use crate::texture;
use crate::vk::{BlendMode, Texture, Vertex2D, Vulkan};
use crate::wav;
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

/// A brief mark a shot leaves: where it is, how big, its colour, and how long
/// it stays. The impact mark and the muzzle flash are the same thing at
/// different sizes and lives.
struct Spark {
    at: [f32; 3],
    half: f32,
    color: [u8; 4],
    until: u32,
}

/// A shot that is a line in the air: where it starts, where it stops, its
/// colour, and how long it stays. The railgun and lightning gun leave these.
struct Beam {
    from: [f32; 3],
    to: [f32; 3],
    color: [u8; 4],
    until: u32,
}

/// A practice post's shot: a slow bolt that flies where you were when it was
/// fired. Nothing about it follows you.
struct Bolt {
    at: [f32; 3],
    velocity: [f32; 3],
    until: u32,
}

/// How long the red wash of a fresh hit lasts, in milliseconds.
const HURT_FLASH_MS: u32 = 250;

/// How close a post has to be to shoot, how often it fires, how fast its bolt
/// flies, and what it takes off you when it lands.
const BOLT_RANGE: f32 = 900.0;
const BOLT_INTERVAL: u32 = 1_400;
const BOLT_SPEED: f32 = 700.0;
const BOLT_DAMAGE: i32 = 7;
/// How hard a bolt is pulled down on the way, so it lobs rather than flying
/// flat and can drop onto someone behind a ledge.
const BOLT_GRAVITY: f32 = 320.0;
/// How long a post that has just stood back up takes to aim again.
const RECOVERY_GRACE_MS: u32 = 700;

/// How much of your movement a post allows for when it fires. A post that
/// aimed exactly where you are would never touch a moving target; one that
/// aimed perfectly ahead would be unbeatable, so it takes half the lead.
const BOLT_LEAD: f32 = 0.5;

/// Where a post aims to catch you: ahead of where you are, by the time the
/// bolt takes to arrive.
fn bolt_lead(eye: [f32; 3], velocity: [f32; 3], flight: f32) -> [f32; 3] {
    [
        eye[0] + velocity[0] * flight * BOLT_LEAD,
        eye[1] + velocity[1] * flight * BOLT_LEAD,
        eye[2] + velocity[2] * flight * BOLT_LEAD,
    ]
}

/// How far the crosshair's arms sit from the middle: tight when you stand
/// still, opening up as you move and wider still in the air.
fn crosshair_gap(speed: f32, on_ground: bool) -> f32 {
    let moving = (speed / 40.0).min(8.0);
    let air = if on_ground { 0.0 } else { 5.0 };
    3.0 + moving + air
}

/// The velocity that sends a bolt from `from` to `to`, aimed high enough that
/// gravity brings it down onto the point it was aimed at.
fn bolt_aim(from: [f32; 3], to: [f32; 3]) -> [f32; 3] {
    let distance = away(from, to).max(1.0);
    let speed = BOLT_SPEED / distance;
    let flight = distance / BOLT_SPEED;
    [
        (to[0] - from[0]) * speed,
        (to[1] - from[1]) * speed,
        (to[2] - from[2]) * speed + 0.5 * BOLT_GRAVITY * flight,
    ]
}

fn away(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// How far a point is from the line between `from` and `to`, so a fast bolt
/// cannot step clean over the player between frames.
fn away_from_line(point: [f32; 3], from: [f32; 3], to: [f32; 3]) -> f32 {
    let line = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    let length = line[0] * line[0] + line[1] * line[1] + line[2] * line[2];
    if length < 1.0e-6 {
        return away(point, from);
    }
    let along = ((point[0] - from[0]) * line[0]
        + (point[1] - from[1]) * line[1]
        + (point[2] - from[2]) * line[2])
        / length;
    let along = along.clamp(0.0, 1.0);
    away(
        point,
        [
            from[0] + line[0] * along,
            from[1] + line[1] * along,
            from[2] + line[2] * along,
        ],
    )
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
    /// Where recent shots landed, and when the mark should go away.
    sparks: Vec<Spark>,
    /// The lines the railgun and lightning gun left in the air.
    beams: Vec<Beam>,
    /// Bolts the practice posts have in the air.
    bolts: Vec<Bolt>,
    /// When the posts may fire again, so they do not all fire at once.
    next_bolt: u32,
    /// The last frame's clock, for the distance a bolt travels in one frame.
    bolts_moved_at: u32,
    /// The practice posts built from the map's own markers.
    targets: Option<TargetField>,
    /// Where the rounds in the air are, whether they are plasma, and whether
    /// they arc, copied from the move each frame.
    rockets: Vec<([f32; 3], bool, bool)>,
    /// The machinegun's shot sounds, decoded once and shared per shot.
    sounds: Vec<Arc<Vec<f32>>>,
    /// The shotgun's break, the rocket's launch, and what it leaves behind.
    shotgun_sound: Option<Arc<Vec<f32>>>,
    rocket_sound: Option<Arc<Vec<f32>>>,
    explosion_sound: Option<Arc<Vec<f32>>>,
    /// The BFG's hum, played once when a charge starts.
    charge_sound: Option<Arc<Vec<f32>>>,
    /// The one fire sound each weapon with its own, by weapon number.
    fire_sounds: Vec<(i32, Arc<Vec<f32>>)>,
    /// The grunt when something hurts you. The player model's sound is the
    /// stock `sarge` one; the client draws no player model to disagree.
    pain_sound: Option<Arc<Vec<f32>>>,
    /// The fall when your health runs out.
    death_sound: Option<Arc<Vec<f32>>>,
    /// Ricochets for a shot that stops on the map, and the lightning gun's own
    /// crackle for its hits.
    ricochets: Vec<Arc<Vec<f32>>>,
    lightning_hits: Vec<Arc<Vec<f32>>>,
    /// Which ricochet or crackle comes next.
    next_impact: usize,
    /// When the weapon in hand stops kicking after the last shot.
    kick_until: u32,
    /// When the red wash of a fresh hit should have faded out.
    hurt_until: u32,
    /// When the hit marker on the crosshair should have faded out.
    hit_until: u32,
    /// How many practice posts have been put on the floor this life.
    posts_down: u32,
    /// Whether the player was dead last frame, so a respawn can be noticed.
    was_dead: bool,
    /// The pickup sounds, by the path the item table names.
    pickup_sounds: std::collections::HashMap<&'static str, Arc<Vec<f32>>>,
    /// Which of those the next shot uses, so repeated fire varies.
    next_sound: usize,
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

        // The machinegun's four shot sounds, decoded once into the mixer's
        // format; every shot reuses one of them.
        let mut sounds = Vec::new();
        let mut shotgun_sound: Option<Arc<Vec<f32>>> = None;
        let mut rocket_sound: Option<Arc<Vec<f32>>> = None;
        let mut explosion_sound: Option<Arc<Vec<f32>>> = None;
        let mut charge_sound: Option<Arc<Vec<f32>>> = None;
        let mut fire_sounds: Vec<(i32, Arc<Vec<f32>>)> = Vec::new();
        let mut pain_sound: Option<Arc<Vec<f32>>> = None;
        let mut death_sound: Option<Arc<Vec<f32>>> = None;
        let mut ricochets = Vec::new();
        let mut lightning_hits = Vec::new();
        let mut pickup_sounds = std::collections::HashMap::new();
        if let Some(assets) = assets.as_ref() {
            let load = |path: &str| -> Option<Arc<Vec<f32>>> {
                let bytes = assets.read(path)?;
                match wav::decode(&bytes) {
                    Ok(samples) => Some(Arc::new(samples)),
                    Err(e) => {
                        eprintln!("{path}: {e:?}");
                        None
                    }
                }
            };
            for name in ["machgf1b", "machgf2b", "machgf3b", "machgf4b"] {
                if let Some(sound) = load(&format!("sound/weapons/machinegun/{name}.wav")) {
                    sounds.push(sound);
                }
            }
            // The other two weapons only ever need one sound each, plus the
            // blast the rocket leaves.
            shotgun_sound = load("sound/weapons/shotgun/sshotf1b.wav");
            rocket_sound = load("sound/weapons/rocket/rocklf1a.wav");
            explosion_sound = load("sound/weapons/rocket/rocklx1a.wav");
            charge_sound = load("sound/weapons/bfg/bfg_hum.wav");
            // The player's own voice. The client draws no player model, so the
            // stock `sarge` voice is used for every grunt and fall.
            pain_sound = load("sound/player/sarge/pain50_1.wav");
            death_sound = load("sound/player/sarge/death1.wav");
            for name in ["ric1", "ric2", "ric3"] {
                if let Some(sound) = load(&format!("sound/weapons/machinegun/{name}.wav")) {
                    ricochets.push(sound);
                }
            }
            for name in ["lg_hit", "lg_hit2", "lg_hit3"] {
                if let Some(sound) = load(&format!("sound/weapons/lightning/{name}.wav")) {
                    lightning_hits.push(sound);
                }
            }
            // The weapons that had no sound of their own were firing with the
            // machinegun's. These are the ones the paks hold.
            for (weapon, path) in [
                (WP_GRENADE_LAUNCHER, "sound/weapons/grenade/grenlf1a.wav"),
                (WP_LIGHTNING, "sound/weapons/lightning/lg_fire.wav"),
                (WP_RAILGUN, "sound/weapons/railgun/railgf1a.wav"),
                (WP_PLASMAGUN, "sound/weapons/plasma/hyprbf1a.wav"),
                (WP_BFG, "sound/weapons/bfg/bfg_fire.wav"),
            ] {
                if let Some(sound) = load(path) {
                    fire_sounds.push((weapon, sound));
                }
            }
            // The pickup sounds: every path the item table can name, loaded
            // once and looked up when something is taken.
            for path in [
                "sound/items/s_health.wav",
                "sound/items/m_health.wav",
                "sound/items/l_health.wav",
                "sound/items/n_health.wav",
                "sound/items/armourfield.wav",
                "sound/items/holdable.wav",
                "sound/items/quaddamage.wav",
                "sound/items/protect.wav",
                "sound/items/haste.wav",
                "sound/items/invisibility.wav",
                "sound/items/regen.wav",
                "sound/items/flight.wav",
                "sound/misc/w_pkup.wav",
                "sound/misc/am_pkup.wav",
            ] {
                if let Some(sound) = load(path) {
                    pickup_sounds.insert(path, sound);
                }
            }
        }

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
                        "world: {} surfaces, {} triangles, {} textures, {} shaders without art, {} items, {} item models",
                        built.drawn,
                        built.vertices.len() / 3,
                        built.textures().len(),
                        built.missing.len(),
                        built.items.len(),
                        built.item_models.len()
                    );
                    world = Some(built);
                }
                Err(e) => console::print(&format!("world: {e}")),
            }
        }

        // The live pickups, from the same marks the world drew its boxes for.
        let items = world.as_ref().map(|world| ItemField::new(&world.items));
        // Practice posts on the map's `target_position` markers, so there is
        // something to shoot before the game has enemies.
        let targets = map_bytes
            .as_deref()
            .map(|bytes| TargetField::new(&crate::targets::positions(bytes)));

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
             move: WASD, jump: Space, turn: Left/Right, look: mouse, fire: left mouse\n\
             weapons: 1-9 switch to a weapon you are carrying\n\
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
            targets,
            rockets: Vec::new(),
            sparks: Vec::new(),
            beams: Vec::new(),
            bolts: Vec::new(),
            next_bolt: 0,
            bolts_moved_at: 0,
            sounds,
            shotgun_sound,
            rocket_sound,
            explosion_sound,
            charge_sound,
            fire_sounds,
            pain_sound,
            death_sound,
            ricochets,
            lightning_hits,
            next_impact: 0,
            kick_until: 0,
            hurt_until: 0,
            hit_until: 0,
            posts_down: 0,
            was_dead: false,
            pickup_sounds,
            next_sound: 0,
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
            // The weapon keys, bound the way the stock config binds them: 1 is
            // the gauntlet, 2 the machinegun, and so on up to 9 for the bfg.
            let weapon = match key {
                Keycode::Num1 => Some(WP_GAUNTLET),
                Keycode::Num2 => Some(WP_MACHINEGUN),
                Keycode::Num3 => Some(WP_SHOTGUN),
                Keycode::Num4 => Some(WP_GRENADE_LAUNCHER),
                Keycode::Num5 => Some(WP_ROCKET_LAUNCHER),
                Keycode::Num6 => Some(WP_LIGHTNING),
                Keycode::Num7 => Some(WP_RAILGUN),
                Keycode::Num8 => Some(WP_PLASMAGUN),
                Keycode::Num9 => Some(WP_BFG),
                _ => None,
            };
            if let Some(weapon) = weapon {
                if !repeat {
                    if let Some(prediction) = self.prediction.as_mut() {
                        prediction.select_weapon(weapon);
                    }
                }
                return true;
            }

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
                Event::MouseButtonDown {
                    mouse_btn: MouseButton::Left,
                    ..
                } => {
                    if !console::lock().is_open() {
                        self.input.attack = true;
                    }
                }
                Event::MouseButtonUp {
                    mouse_btn: MouseButton::Left,
                    ..
                } => self.input.attack = false,
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

    /// Fits a held weapon's mesh into a box low on the right of the screen.
    ///
    /// The longest axis of the model runs across the screen and the next
    /// longest up it, so a mesh of any units and any orientation fills the same
    /// box. Depth is flat and at the near end, so the weapon draws over the
    /// map. The box is this client's own choice: the cgame that holds the real
    /// view-model offsets is not in this tree.
    fn view_model_matrix(model: &world::ViewModel, kick: f32) -> [f32; 16] {
        let size = [
            model.maxs[0] - model.mins[0],
            model.maxs[1] - model.mins[1],
            model.maxs[2] - model.mins[2],
        ];
        let centre = [
            (model.mins[0] + model.maxs[0]) * 0.5,
            (model.mins[1] + model.maxs[1]) * 0.5,
            (model.mins[2] + model.maxs[2]) * 0.5,
        ];

        let mut axes = [0usize, 1, 2];
        axes.sort_by(|a, b| {
            size[*b]
                .partial_cmp(&size[*a])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let (across, up) = (axes[0], axes[1]);
        let across_scale = 0.70 / size[across].max(0.01);
        let up_scale = 0.70 / size[up].max(0.01);

        // Column-major, mapping model axes to screen x (0.05..0.75) and y
        // (-0.95..-0.25).
        let mut fitted = [0.0f32; 16];
        fitted[across * 4] = across_scale;
        fitted[up * 4 + 1] = up_scale;
        fitted[12] = 0.40 - across_scale * centre[across];
        fitted[13] = -0.60 - up_scale * centre[up] + kick;
        fitted[14] = 0.0;
        fitted[15] = 1.0;
        fitted
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

        // The marks this frame's shots left.
        for spark in &self.sparks {
            let mark = items::cube(spark.at, spark.half, spark.color);
            self.renderer.vk.draw_stage(
                &self.renderer.white,
                &mark,
                &mvp,
                [1.0, 1.0, 0.0, 0.0],
                BlendMode::Opaque,
                true,
            )?;
        }

        // The lines the last railgun and lightning shots left. They add to the
        // frame so they glow rather than sit flat.
        for beam in &self.beams {
            let line = items::beam(beam.from, beam.to, 2.0, beam.color);
            if !line.is_empty() {
                self.renderer.vk.draw_stage(
                    &self.renderer.white,
                    &line,
                    &mvp,
                    [1.0, 1.0, 0.0, 0.0],
                    BlendMode::Add,
                    false,
                )?;
            }
        }

        // The bolts the posts have in the air.
        for bolt in &self.bolts {
            let shot = items::cube(bolt.at, 3.0, [235, 130, 255, 255]);
            self.renderer.vk.draw_stage(
                &self.renderer.white,
                &shot,
                &mvp,
                [1.0, 1.0, 0.0, 0.0],
                BlendMode::Opaque,
                true,
            )?;
        }

        // The practice posts still standing.
        if let Some(field) = self.targets.as_ref() {
            let posts = field.vertices();
            if !posts.is_empty() {
                self.renderer.vk.draw_stage(
                    &self.renderer.white,
                    &posts,
                    &mvp,
                    [1.0, 1.0, 0.0, 0.0],
                    BlendMode::Opaque,
                    true,
                )?;
            }
        }

        // The rounds in the air. A lobbed grenade has a mesh in the paks, so it
        // is drawn as the grenade it is and tumbles as it flies; the rest have
        // no mesh and stay small bright boxes on their way.
        if !self.rockets.is_empty() {
            let mut flying = Vec::new();
            let spin = self.millis() as f32 * 0.4;
            for (at, plasma, arcs) in &self.rockets {
                if *arcs {
                    if let Some(mesh) = world.grenade.as_ref() {
                        let placed = world::place(&mvp, *at, spin);
                        for surface in mesh {
                            let (_, texture) = &world.textures()[surface.texture];
                            self.renderer.vk.draw_stage(
                                texture,
                                &surface.vertices,
                                &placed,
                                [1.0, 1.0, 0.0, 0.0],
                                BlendMode::Opaque,
                                surface.lit,
                            )?;
                        }
                        continue;
                    }
                }
                let color = if *plasma {
                    [150, 200, 255, 255]
                } else {
                    [255, 240, 200, 255]
                };
                flying.extend(items::cuboid(*at, [-4.0; 3], [4.0; 3], color));
            }
            self.renderer.vk.draw_stage(
                &self.renderer.white,
                &flying,
                &mvp,
                [1.0, 1.0, 0.0, 0.0],
                BlendMode::Opaque,
                true,
            )?;
        }

        // The pickups still on the map, as the models the paks hold. Each one
        // reuses its kind's triangles, moved to the origin the map gave it.
        // If a map's models all failed to load, the coloured marker boxes
        // stand in rather than the items vanishing.
        let now = self.millis();
        if let Some(field) = self.items.as_ref() {
            if world.item_models.is_empty() {
                if !field.vertices().is_empty() {
                    self.renderer.vk.draw_stage(
                        &self.renderer.white,
                        field.vertices(),
                        &mvp,
                        [1.0, 1.0, 0.0, 0.0],
                        BlendMode::Opaque,
                        true,
                    )?;
                }
            } else {
                for item in field.items() {
                    if !item.available(now) {
                        continue;
                    }
                    let Some(model) = world
                        .item_models
                        .iter()
                        .find(|model| model.item_index == item.item_index)
                    else {
                        continue;
                    };
                    // The stock game spins and bobs a pickup so it reads as
                    // something to take rather than map decoration. The rates
                    // are the client's own choice: the cgame that holds the
                    // original numbers is not in this tree.
                    let phase = item.origin[0] + item.origin[1];
                    let spin = (now as f32 + phase * 7.0) * 0.04;
                    let bob = 5.0 * ((now as f32 + phase * 50.0) / 700.0).sin();
                    let mut at = item.origin;
                    at[2] += bob;
                    let placed = world::place(&mvp, at, spin);
                    for surface in &model.surfaces {
                        let (_, texture) = &world.textures()[surface.texture];
                        self.renderer.vk.draw_stage(
                            texture,
                            &surface.vertices,
                            &placed,
                            [1.0, 1.0, 0.0, 0.0],
                            BlendMode::Opaque,
                            surface.lit,
                        )?;
                    }
                }
            }
        }

        // The weapon in hand. There are no view models in this installation, so
        // the third-person mesh is fitted to a box low on the right instead.
        if let Some(model) = world
            .view_models
            .iter()
            .find(|model| model.weapon == prediction.weapon())
        {
            // The weapon dips for a moment after each shot.
            let kick = if self.millis() < self.kick_until {
                -0.05
            } else {
                0.0
            };
            let fitted = Self::view_model_matrix(model, kick);
            for surface in &model.surfaces {
                let (_, texture) = &world.textures()[surface.texture];
                self.renderer.vk.draw_stage(
                    texture,
                    &surface.vertices,
                    &fitted,
                    [1.0, 1.0, 0.0, 0.0],
                    BlendMode::Opaque,
                    surface.lit,
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
        let (width, height) = self.renderer.size();
        let scale = self.renderer.face().default_scale(height) * 0.75;
        let line_height = self.renderer.face().line_height(scale);

        // The player's own numbers first: health, armour, and which weapon and
        // clip are in hand. There is no first-person model yet, so the name is
        // how the readout says what is being held.
        let health = prediction.health();
        let armor = prediction.armor();
        let ammo = prediction.ammo();
        let weapon = bg_misc::class_of_weapon(prediction.weapon())
            .map(|name| name.trim_start_matches("weapon_"))
            .unwrap_or("none");
        let mut status = format!(
            "HEALTH {health}   ARMOR {armor}   {weapon} {ammo}   DOWN {}",
            self.posts_down
        );
        // A charged weapon shows how far the held charge has got, since
        // nothing else on screen says when it is ready to let go.
        let now = self.millis();
        let charging = prediction.charge(now);
        if charging > 0.0 {
            status.push_str(&format!("   CHARGING {}%", (charging * 100.0) as u32));
        }
        let [x, y, z] = prediction.origin();
        let line = format!(
            "{x:.0} {y:.0} {z:.0}   {:.0} u/s   {}",
            prediction.speed(),
            if prediction.on_ground() { "ground" } else { "air" }
        );

        // A crosshair in the middle of the frame. It is drawn by the client
        // because the cgame that would draw one is not in this tree.
        let mid_x = width as f32 * 0.5;
        let mid_y = height as f32 * 0.5;
        let gap = match self.prediction.as_ref() {
            Some(prediction) => crosshair_gap(prediction.speed(), prediction.on_ground()),
            None => crosshair_gap(0.0, true),
        };
        let length = 7.0;
        for (x, y, w, h) in [
            (mid_x - gap - length, mid_y - 1.0, length, 2.0),
            (mid_x + gap, mid_y - 1.0, length, 2.0),
            (mid_x - 1.0, mid_y - gap - length, 2.0, length),
            (mid_x - 1.0, mid_y + gap, 2.0, length),
        ] {
            self.renderer.fill(x, y, w, h, [235, 235, 235, 190])?;
        }
        // A shot that hits a post flares the corners of the crosshair amber.
        if now < self.hit_until {
            for (x, y, w, h) in [
                (mid_x - 12.0, mid_y - 12.0, 6.0, 2.0),
                (mid_x + 6.0, mid_y - 12.0, 6.0, 2.0),
                (mid_x - 12.0, mid_y + 10.0, 6.0, 2.0),
                (mid_x + 6.0, mid_y + 10.0, 6.0, 2.0),
            ] {
                self.renderer.fill(x, y, w, h, [255, 210, 90, 255])?;
            }
        }

        // A fresh hit washes the frame red and fades out over a quarter second.
        if now < self.hurt_until {
            let left = (self.hurt_until - now) as f32 / HURT_FLASH_MS as f32;
            self.renderer.fill(
                0.0,
                0.0,
                width as f32,
                height as f32,
                [190, 25, 25, (110.0 * left) as u8],
            )?;
        }

        // A health bar along the bottom left, so health reads at a glance and
        // not only by the number.
        let bar_width = 220.0;
        let bar_height = 14.0;
        let left = 8.0;
        let top = height as f32 - 8.0 - bar_height;
        self.renderer.fill(
            left - 2.0,
            top - 2.0,
            bar_width + 4.0,
            bar_height + 4.0,
            [0, 0, 0, 150],
        )?;
        let colour = if health > 50 {
            [90, 200, 90, 255]
        } else if health > 25 {
            [225, 190, 60, 255]
        } else {
            [215, 60, 60, 255]
        };
        let filled = bar_width * (health.clamp(0, 100) as f32 / 100.0);
        self.renderer.fill(left, top, filled, bar_height, colour)?;

        // Armour is a second, thinner bar above health, so armour that is
        // soaking hits can be seen going down.
        let armour_height = 7.0;
        let armour_top = top - 4.0 - armour_height;
        self.renderer.fill(
            left - 2.0,
            armour_top - 2.0,
            bar_width + 4.0,
            armour_height + 4.0,
            [0, 0, 0, 150],
        )?;
        let armour = armor.clamp(0, 100) as f32;
        if armour > 0.0 {
            self.renderer.fill(
                left,
                armour_top,
                bar_width * armour / 100.0,
                armour_height,
                [95, 170, 235, 255],
            )?;
        }

        // Dead, the frame dims and a countdown says how long until you are back,
        // so a respawn is not a surprise.
        if prediction.dead() {
            self.renderer.fill(
                0.0,
                0.0,
                width as f32,
                height as f32,
                [0, 0, 0, 140],
            )?;
            let big = self.renderer.face().default_scale(height);
            let text = format!("DOWN   {:.1}", prediction.respawn_in() as f32 / 1000.0);
            let text_width = self.renderer.face().width(&text, big);
            self.renderer.text(
                &text,
                (width as f32 - text_width) * 0.5,
                height as f32 * 0.42,
                big,
                [235, 95, 85, 255],
            )?;
        }

        self.renderer
            .text(&status, 8.0, 8.0, scale, [235, 235, 235, 255])?;
        self.renderer.text(
            &line,
            8.0,
            8.0 + line_height,
            scale,
            [180, 180, 180, 255],
        )
    }

    /// Lets the practice posts fight back: the nearest standing post within
    /// range fires a slow bolt at where the player is, and a bolt that arrives
    /// hurts. A post shoots at a place, not at a target, so you can walk out of
    /// a bolt's way.
    fn update_bolts(&mut self, now: u32) {
        let Some(prediction) = self.prediction.as_mut() else {
            return;
        };
        let origin = prediction.origin();
        let eye = [origin[0], origin[1], origin[2] + 22.0];
        let step = ((now.wrapping_sub(self.bolts_moved_at)) as f32 / 1000.0).min(0.1);
        self.bolts_moved_at = now;

        if now >= self.next_bolt && prediction.health() > 0 {
            if let Some(field) = self.targets.as_ref() {
                let nearest = field
                    .targets()
                    .iter()
                    .filter(|target| target.alive())
                    // A post only fires when the map leaves it a clear line.
                    .filter(|target| {
                        let chest = [
                            target.origin[0],
                            target.origin[1],
                            target.origin[2] + 24.0,
                        ];
                        prediction.sees(chest, eye)
                    })
                    .map(|target| (target.origin, away(target.origin, eye)))
                    .filter(|(_, distance)| *distance < BOLT_RANGE)
                    .min_by(|a, b| {
                        a.1.partial_cmp(&b.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                if let Some((from, _)) = nearest {
                    self.next_bolt = now + BOLT_INTERVAL;
                    let flight = away(from, eye) / BOLT_SPEED;
                    let aim = bolt_lead(eye, prediction.velocity(), flight);
                    self.bolts.push(Bolt {
                        at: from,
                        velocity: bolt_aim(from, aim),
                        until: now + 6_000,
                    });
                }
            }
        }

        let mut damage = 0;
        let mut strikes = Vec::new();
        self.bolts.retain_mut(|bolt| {
            let before = bolt.at;
            bolt.velocity[2] -= BOLT_GRAVITY * step;
            for axis in 0..3 {
                bolt.at[axis] += bolt.velocity[axis] * step;
            }
            // A bolt that reaches the floor or a wall stops there.
            if prediction.solid(bolt.at) {
                strikes.push(bolt.at);
                return false;
            }
            if away_from_line(eye, before, bolt.at) < 36.0 {
                damage += BOLT_DAMAGE;
                strikes.push(bolt.at);
                return false;
            }
            bolt.until > now
        });
        // A fading trail behind each bolt, so an incoming shot can be tracked
        // and stepped out of.
        for at in self.bolts.iter().map(|bolt| bolt.at) {
            self.sparks.push(Spark {
                at,
                half: 1.8,
                color: [170, 100, 225, 255],
                until: now + 80,
            });
        }
        for at in strikes {
            self.sparks.push(Spark {
                at,
                half: 5.0,
                color: [235, 130, 255, 255],
                until: now + 120,
            });
        }
        if damage > 0 {
            let was_alive = prediction.health() > 0;
            prediction.hurt(damage);
            // Say so out loud: a grunt for a hit and the fall for a death.
            let sound = if prediction.health() <= 0 && was_alive {
                self.death_sound.clone()
            } else {
                self.pain_sound.clone()
            };
            if let (Some(sound), Some(audio)) = (sound, self.audio.as_ref()) {
                audio.play_at(sound, eye);
            }
        }
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
            let before = prediction.weapon();
            let health_before = prediction.health();
            prediction.step(now, &self.input);
            // Any loss of health, from a bolt, a fall or your own rocket, shows
            // as a red wash for a moment.
            if prediction.health() < health_before {
                self.hurt_until = now + HURT_FLASH_MS;
            }
            // A new life starts clean: the tally goes back to zero, bolts fired
            // at where you fell are gone, and the posts give you a moment.
            let dead_now = prediction.dead();
            if self.was_dead && !dead_now {
                self.posts_down = 0;
                self.bolts.clear();
                self.next_bolt = now + 1_000;
            }
            self.was_dead = dead_now;
            // Say so when a change finishes, so the keys are not silent.
            if prediction.weapon() != before {
                if let Some(name) = bg_misc::class_of_weapon(prediction.weapon()) {
                    console::print(&format!("weapon: {name}"));
                }
            }
            // A rocket in flight is drawn like a shot's spark, and the ones that
            // arrive this frame are felt, heard and applied to the posts.
            self.rockets = prediction
                .projectiles()
                .iter()
                .map(|shot| (shot.origin, shot.plasma, shot.gravity))
                .collect();
            for blast in prediction.take_blasts() {
                let at = blast.at;
                self.sparks.push(Spark {
                    at,
                    half: 12.0,
                    color: if blast.radius < crate::prediction::ROCKET_RADIUS {
                        [150, 200, 255, 255]
                    } else {
                        [255, 190, 120, 255]
                    },
                    until: now + 200,
                });
                if let (Some(sound), Some(audio)) =
                    (self.explosion_sound.clone(), self.audio.as_ref())
                {
                    audio.play_at(sound, at);
                }
                if let Some(field) = self.targets.as_mut() {
                    let before = field.downed();
                    field.blast(at, blast.radius, blast.damage, now);
                    self.posts_down += (field.downed() - before) as u32;
                }
                // A rocket fired into the floor hurts the one who fired it, less
                // the further away the blast is.
                let eye = prediction.origin();
                let away = ((eye[0] - at[0]).powi(2)
                    + (eye[1] - at[1]).powi(2)
                    + (eye[2] - at[2]).powi(2))
                .sqrt();
                let radius = blast.radius;
                if away < radius {
                    // Your own rocket hurts more than a plasma ball does.
                    let hurt = ((1.0 - away / radius) * blast.damage as f32 * 0.5) as i32;
                    if hurt > 0 {
                        prediction.hurt(hurt);
                    }
                }
            }
            // A short-lived spark at each rocket leaves a trail behind it.
            for (at, plasma, _) in self.rockets.clone() {
                self.sparks.push(Spark {
                    at,
                    half: 2.5,
                    color: if plasma {
                        [170, 210, 255, 255]
                    } else {
                        [255, 210, 150, 255]
                    },
                    until: now + 120,
                });
            }
        }
        // The ear is where the camera is, so sounds can fall off with distance.
        if let (Some(audio), Some(prediction)) = (self.audio.as_ref(), self.prediction.as_ref()) {
            audio.set_listener(prediction.origin(), prediction.yaw());
        }

        // Bring back what has waited long enough, then take what the player is
        // standing in.
        if let (Some(prediction), Some(field)) = (self.prediction.as_mut(), self.items.as_mut()) {
            field.refresh(now);
            for class_name in prediction.collect(field, now) {
                console::print(&format!("picked up {class_name}"));
                if let Some(audio) = self.audio.as_ref() {
                    if let Some(sound) = items::pickup_sound(class_name)
                        .and_then(|path| self.pickup_sounds.get(path))
                    {
                        audio.play_at(Arc::clone(sound), prediction.origin());
                    }
                }
            }
        }
        // The trigger. A hit leaves a brief mark where the bullet stopped.
        if self.input.attack {
            if let Some(prediction) = self.prediction.as_mut() {
                // A charge starting is the only warning the BFG gives before
                // it lets go, so the hum marks the moment the trigger holds.
                let charging_before = prediction.charge(now);
                let shots = prediction.fire(now);
                if charging_before == 0.0 && prediction.charge(now) > 0.0 {
                    if let (Some(sound), Some(audio)) =
                        (self.charge_sound.clone(), self.audio.as_ref())
                    {
                        audio.play_at(sound, prediction.origin());
                    }
                }
                for shot in shots {
                    // The flash sits just past the muzzle, along the shot, and
                    // the weapon kicks for a moment so firing has some weight.
                    let forward = shot.forward();
                    self.kick_until = now + 120;
                    self.sparks.push(Spark {
                        at: [
                            shot.from[0] + forward[0] * 24.0,
                            shot.from[1] + forward[1] * 24.0,
                            shot.from[2] + forward[2] * 24.0,
                        ],
                        half: 7.0,
                        color: [255, 250, 210, 255],
                        until: now + 45,
                    });
                    self.sparks.push(Spark {
                        at: [
                            shot.from[0] + forward[0] * 16.0,
                            shot.from[1] + forward[1] * 16.0,
                            shot.from[2] + forward[2] * 16.0,
                        ],
                        half: 3.0,
                        color: [255, 235, 180, 255],
                        until: now + 30,
                    });
                    if shot.hit {
                        self.sparks.push(Spark {
                            at: shot.to,
                            half: 2.0,
                            color: [255, 225, 140, 255],
                            until: now + 150,
                        });
                        // Hear where the shot stopped, not only where it began.
                        let sounds = if prediction.weapon() == WP_LIGHTNING {
                            &self.lightning_hits
                        } else {
                            &self.ricochets
                        };
                        if let (Some(sound), Some(audio)) = (
                            sounds.get(self.next_impact % sounds.len().max(1)).cloned(),
                            self.audio.as_ref(),
                        ) {
                            self.next_impact += 1;
                            audio.play_at(sound, shot.to);
                        }
                    }
                    if shot.splash > 0.0 {
                        self.sparks.push(Spark {
                            at: shot.to,
                            half: 12.0,
                            color: [255, 190, 120, 255],
                            until: now + 200,
                        });
                        if let (Some(sound), Some(audio)) =
                            (self.explosion_sound.clone(), self.audio.as_ref())
                        {
                            audio.play_at(sound, shot.to);
                        }
                    }
                    // Each weapon fires with its own sound.
                    let weapon = prediction.weapon();
                    if weapon == WP_RAILGUN || weapon == WP_LIGHTNING {
                        self.beams.push(Beam {
                            from: shot.from,
                            to: shot.to,
                            color: if weapon == WP_RAILGUN {
                                [120, 210, 255, 255]
                            } else {
                                [190, 180, 255, 255]
                            },
                            until: now + if weapon == WP_RAILGUN { 110 } else { 70 },
                        });
                    }
                    let fired = match weapon {
                        WP_SHOTGUN => self.shotgun_sound.clone(),
                        WP_ROCKET_LAUNCHER => self.rocket_sound.clone(),
                        _ => self
                            .fire_sounds
                            .iter()
                            .find(|(number, _)| *number == weapon)
                            .map(|(_, sound)| Arc::clone(sound))
                            .or_else(|| {
                                if self.sounds.is_empty() {
                                    None
                                } else {
                                    let sound = Arc::clone(
                                        &self.sounds[self.next_sound % self.sounds.len()],
                                    );
                                    self.next_sound += 1;
                                    Some(sound)
                                }
                            }),
                    };
                    if let (Some(sound), Some(audio)) = (fired, self.audio.as_ref()) {
                        audio.play_at(sound, shot.from);
                    }
                    if let Some(field) = self.targets.as_mut() {
                        if let Some((at, destroyed)) = field.shoot(shot.from, shot.to, shot.damage, now) {
                            self.hit_until = now + 150;
                            self.sparks.push(Spark {
                                at,
                                half: 3.0,
                                color: [255, 170, 90, 255],
                                until: now + 150,
                            });
                            if destroyed {
                                self.posts_down += 1;
                                console::print("target down");
                            }
                        }
                        if shot.splash > 0.0 {
                            let before = field.downed();
                            let down = field.blast(shot.to, shot.splash, 100, now);
                            self.posts_down += (field.downed() - before) as u32;
                            if down > 0 {
                                console::print("target down");
                            }
                        }
                    }
                }
            }
        }
        self.sparks.retain(|spark| spark.until > now);
        self.beams.retain(|beam| beam.until > now);
        // A post that has been down long enough stands back up. It takes it a
        // moment to bring its gun up, so a post that just got back up does not
        // shoot at you the instant it is standing.
        if let Some(field) = self.targets.as_mut() {
            if field.recover(now) > 0 {
                self.next_bolt = self.next_bolt.max(now + RECOVERY_GRACE_MS);
            }
        }
        self.update_bolts(now);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Where a model-space point lands on the screen through a fitted matrix.
    fn on_screen(m: &[f32; 16], p: [f32; 3]) -> [f32; 3] {
        [
            m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
            m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
            m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
        ]
    }

    #[test]
    fn a_held_weapon_fits_the_screen_box() {
        // A mesh long on x, as the weapon models are: the long axis runs across
        // the screen and the next longest up it.
        let model = world::ViewModel {
            weapon: 2,
            surfaces: Vec::new(),
            mins: [-40.0, -10.0, -5.0],
            maxs: [40.0, 10.0, 5.0],
        };
        let fitted = Client::view_model_matrix(&model, 0.0);
        let near_left = on_screen(&fitted, [-40.0, -10.0, 0.0]);
        let far_right = on_screen(&fitted, [40.0, 10.0, 0.0]);
        assert!((near_left[0] - 0.05).abs() < 1e-3, "{near_left:?}");
        assert!((far_right[0] - 0.75).abs() < 1e-3, "{far_right:?}");
        assert!((near_left[1] + 0.95).abs() < 1e-3, "{near_left:?}");
        assert!((far_right[1] + 0.25).abs() < 1e-3, "{far_right:?}");
        // Flat, and at the near end, so the map never hides it.
        assert_eq!(near_left[2], 0.0);
        assert_eq!(far_right[2], 0.0);
    }
}

#[cfg(test)]
mod bolt_tests {
    use super::*;

    #[test]
    fn a_fast_bolt_cannot_step_over_the_player() {
        // A bolt crossing right through the eye in one long step.
        let eye = [0.0, 0.0, 0.0];
        let before = [-50.0, 10.0, 0.0];
        let after = [50.0, 10.0, 0.0];
        // Neither end is close, but the path passes ten units away.
        assert!(away(before, eye) > 36.0);
        assert!(away(after, eye) > 36.0);
        assert!((away_from_line(eye, before, after) - 10.0).abs() < 1.0e-4);
    }

    #[test]
    fn a_bolt_that_misses_stays_away() {
        let eye = [0.0, 0.0, 0.0];
        let distance = away_from_line(eye, [-50.0, 100.0, 0.0], [50.0, 100.0, 0.0]);
        assert!((distance - 100.0).abs() < 1.0e-4, "{distance}");
    }
}

#[cfg(test)]
mod bolt_arc_tests {
    use super::*;

    fn fly(from: [f32; 3], to: [f32; 3]) -> [f32; 3] {
        let step = 1.0 / 60.0;
        let mut at = from;
        let mut velocity = bolt_aim(from, to);
        for _ in 0..600 {
            velocity[2] -= BOLT_GRAVITY * step;
            for axis in 0..3 {
                at[axis] += velocity[axis] * step;
            }
            if at[0] >= to[0] {
                break;
            }
        }
        at
    }

    #[test]
    fn an_arcing_bolt_lands_where_it_was_aimed() {
        let to = [500.0, 0.0, 40.0];
        let landed = fly([0.0, 0.0, 24.0], to);
        assert!(
            (landed[2] - to[2]).abs() < 6.0,
            "landed at {landed:?}, aimed at {to:?}"
        );
    }

    #[test]
    fn a_post_leads_a_moving_target_but_only_half_way() {
        let eye = [100.0, 0.0, 24.0];
        let running = [400.0, 0.0, 0.0];
        let aimed = bolt_lead(eye, running, 0.5);
        // Half a second at 400 a second is 200 units, so half is 100.
        assert!((aimed[0] - 200.0).abs() < 0.01, "{aimed:?}");
        // Standing still, the aim is exactly where you are.
        assert_eq!(bolt_lead(eye, [0.0; 3], 0.5), eye);
        // It never leads further than you could have moved.
        let full = [eye[0] + running[0] * 0.5, eye[1], eye[2]];
        assert!(aimed[0] < full[0]);
    }

    #[test]
    fn the_crosshair_opens_up_as_you_move_and_widens_in_the_air() {
        let still = crosshair_gap(0.0, true);
        let walking = crosshair_gap(200.0, true);
        let running = crosshair_gap(400.0, true);
        let jumping = crosshair_gap(400.0, false);
        assert_eq!(still, 3.0);
        assert!(walking > still && running > walking, "{walking} {running}");
        assert!(jumping > running, "{jumping} {running}");
        // Even at full sprint it stays small enough to aim with.
        assert!(running < 12.0, "{running}");
    }

    #[test]
    fn a_bolt_lobs_on_the_way() {
        let from = [0.0, 0.0, 24.0];
        let to = [500.0, 0.0, 40.0];
        let velocity = bolt_aim(from, to);
        // It starts going up, because gravity will bring it back down.
        assert!(velocity[2] > 0.0, "{velocity:?}");
        // And it is in the air for about the time the distance takes.
        let flight = away(from, to) / BOLT_SPEED;
        let peak = velocity[2] * velocity[2] / (2.0 * BOLT_GRAVITY);
        assert!(peak > 4.0, "peak rise {peak}");
        assert!(flight > 0.5 && flight < 1.0, "{flight}");
    }
}
