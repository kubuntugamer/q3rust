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
use crate::audio::{Audio, SAMPLE_RATE};
use crate::bg_misc;
use crate::bg_pmove::{
    WP_BFG, WP_GAUNTLET, WP_GRENADE_LAUNCHER, WP_LIGHTNING,    WP_MACHINEGUN, WP_ROCKET_LAUNCHER, WP_SHOTGUN, WP_PLASMAGUN, WP_RAILGUN,
};
use crate::cmd::CmdSystem;
use crate::console;
use crate::cvar::{Cvars, CVAR_ARCHIVE};
use crate::font::Face;
use crate::items::{self, ItemField};
use crate::prediction::{Input, Prediction, DODGE_PUSH_FAR, DODGE_PUSH_NEAR};
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

    /// Draws one flat triangle in screen pixels, for the shapes a rectangle
    /// cannot make.
    fn triangle(&mut self, points: [[f32; 2]; 3], color: [u8; 4]) -> Result<(), String> {
        let vertex = |point: [f32; 2]| Vertex2D {
            pos: point,
            uv: [0.5, 0.5],
            color,
        };
        let tri = points.map(vertex);
        self.vk.draw_quads(&self.white, &tri)
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
    /// Set once the bolt has come near enough to be stepped out of, so one
    /// graze is rewarded once and not once a frame on the way past.
    grazed: bool,
}

/// A post that is lining up a shot: which post it is, where the bolt will leave
/// from, and when it started and when it goes. While it waits the post glows,
/// so the shot can be seen coming.
struct WindUp {
    index: usize,
    from: [f32; 3],
    started_at: u32,
    fires_at: u32,
    /// How much of the flight time to lead you by: the usual `BOLT_LEAD` for a
    /// post firing alone, nothing at all for the second post of a volley, which
    /// aims where you stand so that stopping is not a safe answer.
    lead: f32,
}

impl WindUp {
    /// How far through the wind-up it is, from nothing at the start to one at
    /// the moment the bolt leaves. It runs off the time this particular post
    /// took to line up, so a close post's glow fills just as quickly as it
    /// fires, however short its warning was.
    fn progress(&self, now: u32) -> f32 {
        let span = self.fires_at.saturating_sub(self.started_at).max(1) as f32;
        let gone = now.saturating_sub(self.started_at) as f32;
        (gone / span).clamp(0.0, 1.0)
    }
}

/// How long the red wash of a fresh hit lasts, in milliseconds.
const HURT_FLASH_MS: u32 = 250;

/// How many posts may have a shot lined up at once, and only from different
/// sides of you. Two is enough to catch you whichever way you step; more than
/// that is a wall of bolts, not a fight.
const MAX_FIRING_POSTS: usize = 2;

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
/// The quiet a respawn buys: for this long no post can even line a shot up, so
/// coming back never means being hit before you can move.
const SPAWN_GRACE_MS: u32 = 1_000;

/// How strongly the shield a respawn buys shows, from 0 to 1: a slow beat that
/// fades out as the grace runs down, so it is hard to miss in the corner of the
/// eye but never looks like damage.
fn shield_glow(left_ms: u32, now: u32) -> f32 {
    if left_ms == 0 {
        return 0.0;
    }
    let left = left_ms.min(SPAWN_GRACE_MS) as f32 / SPAWN_GRACE_MS as f32;
    let beat = (now as f32 / SPAWN_GRACE_MS as f32 * std::f32::consts::TAU * 3.0).sin();
    ((0.5 + 0.3 * beat) * left).clamp(0.0, 1.0)
}

/// The shield note in the status line with `left_ms` of grace to run, or none
/// once it has run out.
fn shield_note(left_ms: u32) -> Option<String> {
    (left_ms > 0).then(|| {
        let seconds = left_ms.min(SPAWN_GRACE_MS) as f32 / SPAWN_GRACE_MS as f32;
        format!("   SHIELD {seconds:.1}")
    })
}

/// How long a post spends lining up before its bolt leaves, so a shot can be
/// seen coming and stepped out of rather than appearing with no warning. This
/// is the warning you get from the far edge of its range.
const BOLT_TELEGRAPH: u32 = 450;

/// How long the bar under the crosshair takes to shrink away after a bolt was
/// stepped out of, and how wide it starts.
const DODGE_FLASH_MS: u32 = 240;
const DODGE_FLASH_WIDTH: f32 = 26.0;
/// The narrowest the dodge bar ever draws while it is up, so the smallest
/// payoff still looks deliberate.
const DODGE_FLASH_FLOOR: f32 = 8.0;

/// How wide the dodge bar is with `left_ms` still to run and a dodge worth
/// `strength` of a full push: widest for a bolt that nearly clipped you, and
/// gone by the time the bar has run out. A small payoff keeps a floor width so
/// it reads as a small reward rather than a drawing glitch.
fn dodge_bar_width(left_ms: u32, strength: f32) -> f32 {
    let left = left_ms.min(DODGE_FLASH_MS) as f32 / DODGE_FLASH_MS as f32;
    let weight = left * strength.clamp(0.0, 1.0);
    if weight <= 0.0 {
        return 0.0;
    }
    DODGE_FLASH_FLOOR.max(DODGE_FLASH_WIDTH * weight)
}

/// How much of a full dodge push `push` is, for the bar that shows it.
fn dodge_strength(push: f32) -> f32 {
    let span = DODGE_PUSH_NEAR - DODGE_PUSH_FAR;
    ((push - DODGE_PUSH_FAR) / span).clamp(0.0, 1.0)
}

/// How long a short line like "post down" stays on screen before it fades out.
const NOTICE_MS: u32 = 1_600;

/// How many notice lines can sit on screen at once, so a burst of things
/// happening does not cover the view.
const NOTICE_LINES: usize = 3;

/// How solid a notice line is: full for most of its life and fading out at the
/// end, so it leaves without a flicker.
fn notice_alpha(left_ms: u32) -> u8 {
    let left = left_ms.min(NOTICE_MS) as f32 / NOTICE_MS as f32;
    (255.0 * (left * 3.0).min(1.0)) as u8
}

/// Adds a line to the stack on screen, dropping the oldest when the stack is
/// full so each new thing says its own line instead of replacing the last.
fn push_notice(notices: &mut Vec<(String, u32)>, text: &str, now: u32) {
    notices.retain(|(_, until)| *until > now);
    notices.push((text.to_string(), now + NOTICE_MS));
    if notices.len() > NOTICE_LINES {
        notices.remove(0);
    }
}

/// The on-screen line for posts going down: one, or how many at once, and how
/// long the wait for it to get back up will be.
fn down_line(count: u32, wait_ms: u32) -> String {
    let seconds = wait_ms.div_ceil(1_000).max(1);
    if count > 1 {
        format!("{count} posts down {seconds}s")
    } else {
        format!("post down {seconds}s")
    }
}

/// The on-screen line for posts standing back up.
fn up_line(count: u32) -> String {
    if count > 1 {
        format!("{count} posts back up")
    } else {
        "post back up".to_string()
    }
}

/// How long before a post that keeps shooting at you gets another line in the
/// console. A post holds its rhythm whether or not the last line was read, so
/// without this the log fills up during a fight.
const BOLT_CALL_REPEAT_MS: u32 = 3_000;

/// Whether a post that started a wind-up is worth a console line: yes for a
/// post you have not heard from lately, no for one still on its rhythm.
fn worth_saying(last: Option<(usize, u32)>, index: usize, now: u32) -> bool {
    match last {
        Some((who, at)) if who == index => now.saturating_sub(at) >= BOLT_CALL_REPEAT_MS,
        _ => true,
    }
}

/// How close a bolt has to pass to count as a hit, and how close it has to
/// pass to count as a shot stepped out of.
const BOLT_HIT_RADIUS: f32 = 36.0;
const BOLT_GRAZE_RADIUS: f32 = 90.0;

/// Whether a bolt that came within `distance` of your head went past rather
/// than into you: near enough to be a dodge, not close enough to hurt.
fn near_miss(distance: f32) -> bool {
    distance > BOLT_HIT_RADIUS && distance < BOLT_GRAZE_RADIUS
}

/// Whether a bolt walked from `from` to `to` has gone by `point`: its far end
/// is moving away from it. Until then a close pass is a shot still coming, and
/// paying for it would reward the player for the bolt that is about to hit
/// them.
fn gone_past(point: [f32; 3], from: [f32; 3], to: [f32; 3]) -> bool {
    let line = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    if line[0] == 0.0 && line[1] == 0.0 && line[2] == 0.0 {
        return false;
    }
    (to[0] - point[0]) * line[0]
        + (to[1] - point[1]) * line[1]
        + (to[2] - point[2]) * line[2]
        >= 0.0
}

/// Whether a bolt walked from `from` to `to` reaches the player: it has to pass
/// within reach of the eye, and the player has to still be up. A bolt that lands
/// after you fall finds a corpse, which takes no hurt and does not grunt again.
fn hits_player(up: bool, eye: [f32; 3], from: [f32; 3], to: [f32; 3]) -> bool {
    up && away_from_line(eye, from, to) < BOLT_HIT_RADIUS
}

/// What a bolt's step is worth as a dodge: how far it passed you, once it has
/// gone by and if that was close enough to feel. `None` while it is still on
/// its way in, when it went wide, and when it has already paid. The miss is
/// measured on the whole step, so a bolt that crosses close between two frames
/// still counts.
fn grazed_by(point: [f32; 3], from: [f32; 3], to: [f32; 3], already: bool) -> Option<f32> {
    if already || !gone_past(point, from, to) {
        return None;
    }
    let miss = away_from_line(point, from, to);
    near_miss(miss).then_some(miss)
}

/// What a dodge at `distance` is worth: full pay for a bolt that nearly
/// clipped you, tapering to the least for one that went past with room.
fn dodge_push(distance: f32) -> f32 {
    let span = BOLT_GRAZE_RADIUS - BOLT_HIT_RADIUS;
    let closeness = ((BOLT_GRAZE_RADIUS - distance) / span).clamp(0.0, 1.0);
    DODGE_PUSH_FAR + (DODGE_PUSH_NEAR - DODGE_PUSH_FAR) * closeness
}

/// How long a post takes when you are right on top of it. Standing close enough
/// to smell the gun means almost no warning, so closing in has to be a decision
/// rather than a free way to bully a post that cannot aim fast enough.
const BOLT_TELEGRAPH_NEAR: u32 = 220;

/// How long a post `distance` away takes to line a shot up: the full warning at
/// the edge of its range, cut down steadily as you close, so the closer you
/// stay in its sight the less time you get to step out of it.
fn telegraph_for(distance: f32) -> u32 {
    let closed = (distance / BOLT_RANGE).clamp(0.0, 1.0);
    let span = (BOLT_TELEGRAPH - BOLT_TELEGRAPH_NEAR) as f32;
    BOLT_TELEGRAPH_NEAR + (span * closed) as u32
}

/// How much longer the posts wait after firing a pair than after a lone shot,
/// so a bracket is something they commit to rather than their default.
const VOLLEY_INTERVAL_SHARE: f32 = 1.6;

/// How long before the posts may line up another shot, once `posts` of them
/// have just fired together.
fn interval_after(posts: usize) -> u32 {
    if posts > 1 {
        (BOLT_INTERVAL as f32 * VOLLEY_INTERVAL_SHARE) as u32
    } else {
        BOLT_INTERVAL
    }
}

/// How long the second post of a volley takes, as a share of a lone post's
/// warning. Its bolt goes where you are rather than where you will be, so a
/// step already beats it; giving it the full warning would make the pair feel
/// like twice the threat when it is only worth more than one bolt.
const VOLLEY_SECOND_TELEGRAPH: f32 = 0.75;

/// How long the post filling `slot` of a shot takes to line it up. Shorter for
/// the second of a volley, but never so short that the bolt is a surprise.
fn telegraph_for_slot(distance: f32, slot: usize) -> u32 {
    let warning = telegraph_for(distance);
    if slot == 0 {
        return warning;
    }
    ((warning as f32 * VOLLEY_SECOND_TELEGRAPH) as u32).max(BOLT_TELEGRAPH_NEAR / 2)
}

/// How much of your movement a post allows for when it fires. A post that
/// aimed exactly where you are would never touch a moving target; one that
/// aimed perfectly ahead would be unbeatable, so it takes half the lead.
const BOLT_LEAD: f32 = 0.5;

/// Where a post aims to catch you: ahead of where you are, by the time the
/// bolt takes to arrive.
fn bolt_lead(eye: [f32; 3], velocity: [f32; 3], flight: f32, lead: f32) -> [f32; 3] {
    [
        eye[0] + velocity[0] * flight * lead,
        eye[1] + velocity[1] * flight * lead,
        eye[2] + velocity[2] * flight * lead,
    ]
}

/// What becomes of a post's lined-up shot this frame: it fires, it keeps
/// lining up, or it is dropped because the post is no longer standing.
#[derive(Debug, PartialEq)]
enum Shot {
    Fire,
    Wait,
    Drop,
}

/// A post that has just been knocked down loses the shot it was lining up
/// straight away, so its glow and its mark on your ring go with it instead of
/// hanging in the air for a bolt that will never come. A shot lined up at a
/// player who has died is let go of the same way: a body on the floor cannot be
/// hurt, and a post that kept shooting at it would spend its rhythm on nothing
/// until the respawn.
fn shot_state(wind: &WindUp, alive: bool, player_up: bool, now: u32) -> Shot {
    if !alive || !player_up {
        Shot::Drop
    } else if now < wind.fires_at {
        Shot::Wait
    } else {
        Shot::Fire
    }
}

/// How long the chirp that says a second post has joined a volley lasts.
const VOLLEY_CHIRP_MS: u32 = 140;

/// A short two-note rising chirp for the second post of a volley. It is made
/// here rather than taken from the paks, so it sounds unlike anything a weapon
/// makes and can never be mistaken for a shot.
fn volley_chirp() -> Arc<Vec<f32>> {
    let total = (SAMPLE_RATE as u32 * VOLLEY_CHIRP_MS / 1_000) as usize;
    let half = total / 2;
    let mut samples = Vec::with_capacity(total);
    for i in 0..total {
        let progress = i as f32 / total as f32;
        // Two steps up, fading in and out so neither end clicks.
        let freq = if i < half { 640.0 } else { 900.0 };
        let envelope = (progress * std::f32::consts::PI).sin();
        let t = i as f32 / SAMPLE_RATE as f32;
        samples.push(0.35 * envelope * (std::f32::consts::TAU * freq * t).sin());
    }
    Arc::new(samples)
}

/// How much of the flight time the post filling `slot` of a volley leads you
/// by. The nearest leads the shot; a second post firing with it aims where you
/// are right now, so stepping one way to dodge the first walks into the second
/// and standing still eats both.
fn lead_for(slot: usize) -> f32 {
    if slot == 0 {
        BOLT_LEAD
    } else {
        0.0
    }
}

/// Hands the leader's place to the post that was firing second once the leader
/// has been shot down: the shorter warning it was given was paid for by a
/// second bolt that is no longer coming, so it now warns for the time a lone
/// post would and leads your run. Counted from when it started lining up, so
/// its glow keeps filling instead of starting over.
fn take_the_lead(wind: &mut WindUp, eye: [f32; 3]) {
    let distance = away(wind.from, eye);
    wind.lead = BOLT_LEAD;
    wind.fires_at = wind.started_at + telegraph_for(distance);
}

/// Whether two posts, seen from `eye`, stand far enough apart to catch you
/// between them: a cos of the angle between them below this is roughly fifty
/// degrees or more. Two posts nearly in line are just two bolts from the same
/// side, which one step still escapes.
const VOLLEY_SPREAD_COS: f32 = 0.64;

/// Whether the posts at `a` and `b` bracket `eye`, so that stepping away from
/// one walks towards the other.
fn brackets(eye: [f32; 3], a: [f32; 3], b: [f32; 3]) -> bool {
    let to_a = [a[0] - eye[0], a[1] - eye[1]];
    let to_b = [b[0] - eye[0], b[1] - eye[1]];
    let len_a = (to_a[0] * to_a[0] + to_a[1] * to_a[1]).sqrt();
    let len_b = (to_b[0] * to_b[0] + to_b[1] * to_b[1]).sqrt();
    if len_a < 1.0 || len_b < 1.0 {
        return false;
    }
    let cos = (to_a[0] * to_b[0] + to_a[1] * to_b[1]) / (len_a * len_b);
    cos <= VOLLEY_SPREAD_COS
}

/// Which posts should line up a shot right now, nearest first: those still
/// standing, with a clear line to `eye`, in range, on a side of you nothing
/// already lined up is on, and not already lining one up. Up to `limit` of them
/// may fire together, so two posts with you in the open between them can catch
/// you whichever way you step.
fn volley_candidates(
    field: &TargetField,
    eye: [f32; 3],
    already: &[WindUp],
    limit: usize,
    sees: impl Fn([f32; 3], [f32; 3]) -> bool,
) -> Vec<(usize, [f32; 3], f32)> {
    let mut found: Vec<(usize, [f32; 3], f32)> = field
        .targets()
        .iter()
        .enumerate()
        .filter(|(_, target)| target.alive())
        .filter(|(index, _)| !already.iter().any(|wind| wind.index == *index))
        .filter(|(_, target)| {
            sees(
                [
                    target.origin[0],
                    target.origin[1],
                    target.origin[2] + 24.0,
                ],
                eye,
            )
        })
        .map(|(index, target)| (index, target.origin, away(target.origin, eye)))
        .filter(|(_, _, distance)| *distance < BOLT_RANGE)
        .collect();    found.sort_by(|a, b| {
        a.2.partial_cmp(&b.2)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Take them nearest first, but only while each new post stands on a
    // different side of you from everything already lined up, so a volley is a
    // bracket and not two bolts from the same direction.
    let mut chosen: Vec<(usize, [f32; 3], f32)> = Vec::new();
    for candidate in found {
        if chosen.len() >= limit {
            break;
        }
        let spread = chosen
            .iter()
            .map(|(_, from, _)| *from)
            .chain(already.iter().map(|wind| wind.from))
            .all(|from| brackets(eye, from, candidate.1));
        if spread {
            chosen.push(candidate);
        }
    }
    chosen
}

/// Where a post lies relative to where you face, in degrees: zero dead ahead,
/// positive to your left (yaw grows left, the same way the camera turns), and
/// past ±90 when it is behind you.
fn bearing(eye: [f32; 3], yaw: f32, at: [f32; 3]) -> f32 {
    let (dx, dy) = (at[0] - eye[0], at[1] - eye[1]);
    if dx == 0.0 && dy == 0.0 {
        return 0.0;
    }
    let facing = dy.atan2(dx).to_degrees();
    (facing - yaw + 180.0).rem_euclid(360.0) - 180.0
}

/// A point `radius` pixels from `centre` in the direction of a post `bearing`
/// degrees off your view: straight up for a post dead ahead, round to the left
/// for one on your left. A post behind you sits at the side of the ring rather
/// than wrapping round the bottom, so it still says which way to turn.
fn ring_point(centre: [f32; 2], bearing: f32, radius: f32) -> [f32; 2] {
    let (sin, cos) = bearing.clamp(-90.0, 90.0).to_radians().sin_cos();
    [centre[0] - sin * radius, centre[1] - cos * radius]
}

/// Which way to say a post round `bearing` degrees off your view lies, in the
/// words a player would use: ahead, off to one side, or behind you.
fn side_words(bearing: f32) -> String {
    let off = bearing.abs();
    let side = if bearing > 0.0 { "left" } else { "right" };
    if off < 15.0 {
        "ahead".to_string()
    } else if off < 60.0 {
        format!("front {side}")
    } else if off < 120.0 {
        side.to_string()
    } else {
        format!("behind {side}")
    }
}

/// The three corners of the arrow that points out from the crosshair at the
/// post lining a shot up.
fn threat_arrow(centre: [f32; 2], bearing: f32, radius: f32) -> [[f32; 2]; 3] {
    let (sin, cos) = bearing.clamp(-90.0, 90.0).to_radians().sin_cos();
    // Outward along the bearing, and across it so the head has a base.
    let out = [-sin, -cos];
    let across = [cos, -sin];
    let base = ring_point(centre, bearing, radius);
    let half = 5.0;
    [
        [
            centre[0] + out[0] * (radius + 8.0),
            centre[1] + out[1] * (radius + 8.0),
        ],
        [
            base[0] + across[0] * half,
            base[1] + across[1] * half,
        ],
        [
            base[0] - across[0] * half,
            base[1] - across[1] * half,
        ],
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

/// How finely a bolt's step is walked when it looks for a wall: fine enough
/// that the thin brushes the maps are built from cannot be stepped clean over
/// when a slow frame moves the bolt a long way.
const BOLT_STEP: f32 = 4.0;

/// The first point along a bolt's step that `solid` says is inside the map, or
/// `None` when the whole step is clear. A bolt covering a long step in a slow
/// frame is walked in pieces, so it stops at a thin wall instead of appearing
/// on the far side of it.
fn first_solid(
    from: [f32; 3],
    to: [f32; 3],
    solid: impl Fn([f32; 3]) -> bool,
) -> Option<[f32; 3]> {
    let pieces = (away(from, to) / BOLT_STEP).ceil().max(1.0) as u32;
    for piece in 1..=pieces {
        let along = piece as f32 / pieces as f32;
        let point = [
            from[0] + (to[0] - from[0]) * along,
            from[1] + (to[1] - from[1]) * along,
            from[2] + (to[2] - from[2]) * along,
        ];
        if solid(point) {
            return Some(point);
        }
    }
    None
}

/// Where a shot ends once the practice posts are counted: the map's trace stops
/// at walls only, so a post catches the round first. The mark it leaves, the
/// sound it makes and a rail's beam all end on the post rather than carrying on
/// to the wall behind it. Also reports what the shot did to a post, if it
/// reached one.
fn shot_end(
    shot: &crate::prediction::Shot,
    field: &mut TargetField,
    now: u32,
) -> ([f32; 3], Option<([f32; 3], bool)>) {
    let struck = field.shoot(shot.from, shot.to, shot.damage, now);
    (struck.map_or(shot.to, |(at, _)| at), struck)
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
    /// The posts lining shots up right now, at most `MAX_FIRING_POSTS` of them.
    /// The post that is lining one up right now, if any.
    wind_ups: Vec<WindUp>,
    /// The last post the console named a shot for, and when, so a post that
    /// keeps firing does not fill the log.
    bolt_call: Option<(usize, u32)>,
    /// Short lines and when each should have gone: what just happened to a
    /// post, said on screen as well as in the console.
    notices: Vec<(String, u32)>,
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
    /// The chirp for a second post joining a volley, made rather than loaded.
    volley_chirp: Arc<Vec<f32>>,
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
    /// When the quiet a respawn buys runs out, so the status line can say it is
    /// on.
    spawn_grace_until: u32,
    /// When the short bar saying "you stepped out of that" should have gone,
    /// and how much of a full push the dodge was worth.
    dodge_until: u32,
    dodge_strength: f32,
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
            wind_ups: Vec::new(),
            bolt_call: None,
            notices: Vec::new(),
            bolts_moved_at: 0,
            sounds,
            shotgun_sound,
            rocket_sound,
            explosion_sound,
            volley_chirp: volley_chirp(),
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
            spawn_grace_until: 0,
            dodge_until: 0,
            dodge_strength: 1.0,
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

            // Q snaps back to the weapon held before this one, the way a
            // last-weapon bind does.
            if key == Keycode::Q {
                if !repeat {
                    if let Some(prediction) = self.prediction.as_mut() {
                        prediction.switch_to_last();
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
                // A window that loses focus never hears the keys come back up,
                // so everything held is let go instead of being left pressed.
                Event::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                } => self.input.release_all(),
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
                Event::MouseWheel { y, precise_y, .. } => {
                    if !console::lock().is_open() {
                        // A notch of the wheel is one step; a trackpad's
                        // fractional scroll is rounded so a soft flick does not
                        // fly through the whole kit.
                        let lines = if y != 0 { y } else { precise_y as i32 };
                        if lines != 0 {
                            if let Some(prediction) = self.prediction.as_mut() {
                                prediction.cycle_weapon(lines.signum());
                            }
                        }
                    }
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

        // Every post that is lining up a shot glows at its gun, growing and
        // brightening until its bolt leaves.
        let glow_now = self.millis();
        for wind in &self.wind_ups {
            let progress = wind.progress(glow_now);
            let color = [
                255,
                (110.0 + 145.0 * progress) as u8,
                (50.0 + 130.0 * progress) as u8,
                255,
            ];
            let at = [wind.from[0], wind.from[1], wind.from[2] + 24.0];
            let glow = items::cube(at, 2.0 + 4.0 * progress, color);
            self.renderer.vk.draw_stage(
                &self.renderer.white,
                &glow,
                &mvp,
                [1.0, 1.0, 0.0, 0.0],
                BlendMode::Add,
                false,
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
            "HEALTH {health}   ARMOR {armor}   DOWN {}",
            self.posts_down
        );
        let now = self.millis();
        // The shield a respawn buys says so while it lasts, so the quiet is
        // explained rather than looking like the posts missed their chance.
        if let Some(note) = shield_note(self.spawn_grace_until.saturating_sub(now)) {
            status.push_str(&note);
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
        // A bar under the crosshair shrinks away as it shows the step of speed
        // a bolt that just missed paid for, so the reward is seen, not guessed.
        if now < self.dodge_until {
            let bar = dodge_bar_width(self.dodge_until - now, self.dodge_strength);
            self.renderer.fill(
                mid_x - bar * 0.5,
                mid_y + 16.0,
                bar,
                2.0,
                [150, 225, 255, 220],
            )?;
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

        // A post lining a shot up shows on a ring around the crosshair, on the
        // side the post is on and brighter as the shot gets closer, so a shot
        // from off screen can be found and turned into before it fires. A
        // volley shows a mark for each post, so both can be found.
        if !prediction.dead() {
            for wind in &self.wind_ups {
                let progress = wind.progress(now);
                let heading = bearing(prediction.origin(), prediction.yaw(), wind.from);
                let arrow = threat_arrow([mid_x, mid_y], heading, 34.0);
                let color = [255, (210.0 - 90.0 * progress) as u8, 70, 230];
                self.renderer.triangle(arrow, color)?;
            }
        }

        // The shield a respawn buys pulses at the edges of the frame while it
        // lasts, so coming back is something seen rather than guessed at.
        let grace_left = self.spawn_grace_until.saturating_sub(now);
        if grace_left > 0 {
            let alpha = (shield_glow(grace_left, now) * 255.0) as u8;
            let edge = 6.0;
            for (x, y, w, h) in [
                (0.0, 0.0, width as f32, edge),
                (0.0, height as f32 - edge, width as f32, edge),
                (0.0, edge, edge, height as f32 - edge * 2.0),
                (width as f32 - edge, edge, edge, height as f32 - edge * 2.0),
            ] {
                self.renderer
                    .fill(x, y, w, h, [120, 220, 255, alpha])?;
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

        // What is in hand sits in its own panel at the bottom right: the
        // weapon's name over its clip, so the numbers are where the eye already
        // goes for a weapon and not mixed in with health.
        let panel_width = 170.0;
        let big_scale = scale * 1.7;
        let big_height = self.renderer.face().line_height(big_scale);
        let panel_height = line_height + big_height + 16.0;
        let panel_x = width as f32 - 8.0 - panel_width;
        let panel_y = height as f32 - 8.0 - panel_height;
        self.renderer.fill(
            panel_x,
            panel_y,
            panel_width,
            panel_height,
            [0, 0, 0, 150],
        )?;
        let name = weapon.to_uppercase();
        let name_width = self.renderer.face().width(&name, scale);
        self.renderer.text(
            &name,
            panel_x + panel_width - 8.0 - name_width,
            panel_y + 6.0,
            scale,
            [200, 200, 200, 255],
        )?;
        let clip = format!("{ammo}");
        let clip_width = self.renderer.face().width(&clip, big_scale);
        let clip_colour = if ammo > 0 {
            [245, 245, 245, 255]
        } else {
            [225, 90, 80, 255]
        };
        self.renderer.text(
            &clip,
            panel_x + panel_width - 8.0 - clip_width,
            panel_y + 6.0 + line_height,
            big_scale,
            clip_colour,
        )?;
        // A charged weapon fills a violet bar along the bottom of the panel,
        // since nothing else on screen says when it is ready to let go.
        let charging = prediction.charge(now);
        if charging > 0.0 {
            self.renderer.fill(
                panel_x + 2.0,
                panel_y + panel_height - 6.0,
                (panel_width - 4.0) * charging.clamp(0.0, 1.0),
                4.0,
                [170, 110, 235, 255],
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

        // Short lines in the middle of the frame say when posts go down or come
        // back up, so a fight reads without opening the console. Each new line
        // joins the stack instead of replacing the one before it.
        self.notices.retain(|(_, until)| *until > now);
        for (row, (text, until)) in self.notices.iter().enumerate() {
            let text_width = self.renderer.face().width(text, scale);
            self.renderer.text(
                text,
                (width as f32 - text_width) * 0.5,
                height as f32 * 0.32 + row as f32 * line_height,
                scale,
                [235, 200, 110, notice_alpha(until.saturating_sub(now))],
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

        // Any post that has finished lining up takes its shot.
        let lining_up = std::mem::take(&mut self.wind_ups);
        let mut still_lining_up = Vec::with_capacity(lining_up.len());
        // Set when the post leading a volley goes down before it fires, so the
        // post that was firing with it is left shooting alone.
        let mut leader_fell = false;
        for (slot, wind) in lining_up.into_iter().enumerate() {
            let alive = self
                .targets
                .as_ref()
                .map(|field| field.targets()[wind.index].alive())
                .unwrap_or(false);
            match shot_state(&wind, alive, prediction.health() > 0, now) {
                // Still lining up: keep the glow going.
                Shot::Wait => still_lining_up.push(wind),
                Shot::Drop => {
                    if slot == 0 {
                        leader_fell = true;
                    }
                }
                Shot::Fire => {
                    let flight = away(wind.from, eye) / BOLT_SPEED;
                    let aim = bolt_lead(eye, prediction.velocity(), flight, wind.lead);
                    self.bolts.push(Bolt {
                        at: wind.from,
                        velocity: bolt_aim(wind.from, aim),
                        until: now + 6_000,
                        grazed: false,
                    });
                }
            }
        }
        // With the leader gone, whatever was firing second is the whole shot
        // now, so it stops aiming where you stand and takes a lone post's
        // warning rather than a pair's.
        if leader_fell {
            for wind in still_lining_up.iter_mut().filter(|wind| wind.lead == 0.0) {
                take_the_lead(wind, eye);
            }
        }
        self.wind_ups = still_lining_up;

        if self.wind_ups.len() < MAX_FIRING_POSTS
            && now >= self.next_bolt
            && prediction.health() > 0
        {
            if let Some(field) = self.targets.as_ref() {
                // A post only fires when the map leaves it a clear line.
                // Posts already lining up are the nearer ones, so anything
                // joining them takes the second place in the volley.
                let already = self.wind_ups.len();
                let room = MAX_FIRING_POSTS - already;
                let chosen =
                    volley_candidates(field, eye, &self.wind_ups, room, |chest, eye| {
                        prediction.sees(chest, eye)
                    });
                if !chosen.is_empty() {
                    // The wait for the next shot starts now, so a post whose
                    // shot is cancelled still keeps its rhythm. A pair buys
                    // you a longer breather than one bolt does.
                    self.next_bolt = now + interval_after(already + chosen.len());
                }
                for (position, (index, from, distance)) in chosen.into_iter().enumerate() {
                    let slot = already + position;
                    // Say which post has you and which way to look, since a
                    // post off screen is easy to miss otherwise. A post that
                    // keeps firing only gets a line now and then, and a volley
                    // names the one to turn to first.
                    if slot == 0 && worth_saying(self.bolt_call, index, now) {
                        self.bolt_call = Some((index, now));
                        let heading = bearing(prediction.origin(), prediction.yaw(), from);
                        console::print(&format!(
                            "post lining up a shot: {} ({distance:.0} away)",
                            side_words(heading)
                        ));
                    }
                    // A second post joining the shot is heard as well as seen,
                    // so the split is noticed even with the post off screen.
                    if slot == 1 {
                        if let Some(audio) = self.audio.as_ref() {
                            audio.play_at(self.volley_chirp.clone(), from);
                        }
                    }
                    let lead = lead_for(slot);
                    self.wind_ups.push(WindUp {
                        index,
                        from,
                        started_at: now,
                        fires_at: now + telegraph_for_slot(distance, slot),
                        lead,
                    });
                }
            }
        }

        let mut damage = 0;
        let mut strikes = Vec::new();
        // A player who has already been dropped is not a target, so the bolts
        // fired at where they fell pass through the corpse rather than hitting
        // it over and over while it waits to respawn.
        let up = prediction.health() > 0;
        // How close the nearest bolt of this frame came as it went past.
        let mut dodged: Option<f32> = None;
        self.bolts.retain_mut(|bolt| {
            let before = bolt.at;
            bolt.velocity[2] -= BOLT_GRAVITY * step;
            for axis in 0..3 {
                bolt.at[axis] += bolt.velocity[axis] * step;
            }
            // A bolt that reaches the floor or a wall stops there. The whole
            // step is walked, so a frame long enough to move a bolt across a
            // room cannot carry it through a wall on the way.
            if let Some(at) = first_solid(before, bolt.at, |point| prediction.solid(point)) {
                strikes.push(at);
                return false;
            }
            if hits_player(up, eye, before, bolt.at) {
                damage += BOLT_DAMAGE;
                strikes.push(bolt.at);
                return false;
            }
            // A shot that has gone past close enough to feel is worth a little
            // speed, so stepping out of one is paid back. One still on its way
            // in is not: paying then would speed you up for the bolt about to
            // hit you.
            if let Some(distance) = grazed_by(eye, before, bolt.at, bolt.grazed) {
                bolt.grazed = true;
                dodged = Some(dodged.map_or(distance, |best: f32| best.min(distance)));
            }
            bolt.until > now
        });
        if let Some(distance) = dodged {
            if prediction.health() > 0 {
                let push = dodge_push(distance);
                // A dodge is only paid to a moving player, so standing still
                // must not light up the bar for speed that was never given.
                if prediction.dodge(push) {
                    self.dodge_strength = dodge_strength(push);
                    self.dodge_until = now + DODGE_FLASH_MS;
                }
            }
        }
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
            // The posts are the client's own, so the prediction has to be told
            // where they stand before it moves this frame's rounds.
            if let Some(field) = self.targets.as_ref() {
                prediction.set_bodies(&field.standing_bodies());
            }
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
                self.wind_ups.clear();
                self.bolt_call = None;
                self.next_bolt = now + SPAWN_GRACE_MS;
                self.spawn_grace_until = now + SPAWN_GRACE_MS;
                // The fight starts over: a post put down again in the new life
                // lies there the plain six seconds, not the longer time it
                // earned before.
                if let Some(field) = self.targets.as_mut() {
                    field.forget_down_counts();
                }
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
                    // The map's trace knows only its own walls, so a practice
                    // post in the way stops the round before them. Wherever it
                    // stopped, the mark and the sound belong there.
                    let (stop, struck) = match self.targets.as_mut() {
                        Some(field) => shot_end(&shot, field, now),
                        None => (shot.to, None),
                    };
                    if shot.hit || struck.is_some() {
                        self.sparks.push(Spark {
                            at: stop,
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
                            audio.play_at(sound, stop);
                        }
                    }
                    if shot.splash > 0.0 {
                        self.sparks.push(Spark {
                            at: stop,
                            half: 12.0,
                            color: [255, 190, 120, 255],
                            until: now + 200,
                        });
                        if let (Some(sound), Some(audio)) =
                            (self.explosion_sound.clone(), self.audio.as_ref())
                        {
                            audio.play_at(sound, stop);
                        }
                    }
                    // Each weapon fires with its own sound.
                    let weapon = prediction.weapon();
                    if weapon == WP_RAILGUN || weapon == WP_LIGHTNING {
                        self.beams.push(Beam {
                            from: shot.from,
                            to: stop,
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
                    if let Some((at, destroyed)) = struck {
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
                            let wait = self
                                .targets
                                .as_ref()
                                .map_or(0, |field| field.wait_at(now));
                            push_notice(&mut self.notices, &down_line(1, wait), now);
                        }
                    }
                    if let Some(field) = self.targets.as_mut() {
                        if shot.splash > 0.0 {
                            let before = field.downed();
                            let down = field.blast(stop, shot.splash, 100, now);
                            self.posts_down += (field.downed() - before) as u32;
                            if down > 0 {
                                console::print("target down");
                                push_notice(
                                    &mut self.notices,
                                    &down_line(down as u32, field.wait_at(now)),
                                    now,
                                );
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
            let back = field.recover(now);
            if back > 0 {
                self.next_bolt = self.next_bolt.max(now + RECOVERY_GRACE_MS);
                push_notice(&mut self.notices, &up_line(back as u32), now);
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

    #[test]
    fn a_bolt_does_not_hit_a_player_already_down() {
        let eye = [0.0, 0.0, 0.0];
        let (before, after) = ([-50.0, 0.0, 0.0], [50.0, 0.0, 0.0]);
        assert!(hits_player(true, eye, before, after), "it passes through the eye");
        // The same bolt reaches a player who is already down without being a
        // hit, so a corpse takes no hurt and does not grunt again.
        assert!(!hits_player(false, eye, before, after));
        // Close still has to mean close.
        assert!(!hits_player(true, eye, [-50.0, 100.0, 0.0], [50.0, 100.0, 0.0]));
    }
}

#[cfg(test)]
mod bolt_arc_tests {
    use super::*;
    /// A round leaving the weapon, rather than the state of a post's shot,
    /// which this module's own `Shot` already means.
    use crate::prediction::Shot as Round;

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
    fn a_post_glows_before_it_fires() {
        let wind = WindUp {
            index: 0,
            from: [0.0, 0.0, 24.0],
            started_at: 0,
            fires_at: 450,
            lead: BOLT_LEAD,
        };
        assert_eq!(wind.progress(0), 0.0, "dark the moment it starts lining up");
        assert!((wind.progress(225) - 0.5).abs() < 0.01);
        assert_eq!(wind.progress(450), 1.0, "full glow as the bolt leaves");
        assert_eq!(wind.progress(900), 1.0, "and no further");
        // Long enough to see and step out of, short enough to stay a threat.
        assert!(BOLT_TELEGRAPH >= 350 && BOLT_TELEGRAPH < BOLT_INTERVAL);
    }

    #[test]
    fn a_post_with_you_close_lines_up_faster() {
        assert_eq!(
            telegraph_for(0.0),
            BOLT_TELEGRAPH_NEAR,
            "point blank gives the shortest warning"
        );
        assert_eq!(
            telegraph_for(BOLT_RANGE * 3.0),
            BOLT_TELEGRAPH,
            "and nothing beyond the far edge gets any longer"
        );
        let mut last = telegraph_for(0.0);
        for step in 1..=20 {
            let far = telegraph_for(BOLT_RANGE * step as f32 / 20.0);
            assert!(far >= last, "the further off you are, the more warning");
            last = far;
        }
        assert!(
            BOLT_TELEGRAPH_NEAR * 2 < BOLT_TELEGRAPH,
            "close has to be noticeably faster than far to be worth the risk"
        );
        // Even point blank there is a glow, just a brief one.
        assert!(BOLT_TELEGRAPH_NEAR >= 150);
    }

    #[test]
    fn a_post_is_found_by_which_side_it_is_on() {
        let eye = [0.0, 0.0, 24.0];
        // Yaw 0 faces +X, and yaw grows to the left, so +Y is your left.
        assert!((bearing(eye, 0.0, [100.0, 0.0, 24.0])).abs() < 0.01);
        assert!((bearing(eye, 0.0, [0.0, 100.0, 24.0]) - 90.0).abs() < 0.01);
        assert!((bearing(eye, 0.0, [0.0, -100.0, 24.0]) + 90.0).abs() < 0.01);
        let behind = bearing(eye, 0.0, [-100.0, 0.0, 24.0]).abs();
        assert!((behind - 180.0).abs() < 0.01, "behind reads as {behind}");
        // Turning with the post keeps it dead ahead.
        assert!(bearing(eye, 90.0, [0.0, 100.0, 24.0]).abs() < 0.01);
        // And it wraps rather than jumping at north.
        assert!(bearing(eye, 350.0, [100.0, 20.0, 24.0]) > 0.0);
    }

    #[test]
    fn each_notice_gets_its_own_line_and_the_oldest_falls_off() {
        let mut notices = Vec::new();
        push_notice(&mut notices, "post down", 1_000);
        push_notice(&mut notices, "post back up", 1_000);
        assert_eq!(notices.len(), 2, "a second thing does not replace the first");
        assert_eq!((notices[0].0.as_str(), notices[0].1), ("post down", 1_000 + NOTICE_MS));

        for n in 0..5 {
            push_notice(&mut notices, &down_line(1, 8_000), 1_000 + n);
        }
        assert_eq!(notices.len(), NOTICE_LINES, "the stack stays small");
        assert_eq!(
            notices.last().unwrap(),
            &("post down 8s".to_string(), 1_004 + NOTICE_MS)
        );

        // Lines that have run out are cleared before the new one joins.
        push_notice(&mut notices, "post down", 1_004 + NOTICE_MS + 1);
        assert_eq!(notices.len(), 1, "{notices:?}");
    }

    #[test]
    fn a_burst_of_posts_says_how_many() {
        assert_eq!(down_line(1, 6_000), "post down 6s");
        assert_eq!(down_line(2, 8_000), "2 posts down 8s");
        assert_eq!(up_line(1), "post back up");
        assert_eq!(up_line(3), "3 posts back up");
    }

    #[test]
    fn the_wait_is_rounded_up_to_whole_seconds() {
        assert_eq!(down_line(1, 6_001), "post down 7s");
        assert_eq!(down_line(1, 1), "post down 1s", "never a zero second wait");
        assert_eq!(down_line(1, 12_000), "post down 12s");
    }

    #[test]
    fn the_dodge_bar_shrinks_away() {
        assert_eq!(dodge_bar_width(DODGE_FLASH_MS, 1.0), DODGE_FLASH_WIDTH);
        assert!((dodge_bar_width(DODGE_FLASH_MS / 2, 1.0) - DODGE_FLASH_WIDTH * 0.5).abs() < 0.01);
        assert_eq!(dodge_bar_width(0, 1.0), 0.0, "gone when the time is up");
        assert_eq!(
            dodge_bar_width(DODGE_FLASH_MS * 2, 1.0),
            DODGE_FLASH_WIDTH,
            "a stale clock never makes it wider than it starts"
        );
    }

    #[test]
    fn the_shield_pulses_and_fades_as_it_runs_out() {
        assert_eq!(shield_glow(0, 500), 0.0, "nothing to show once it is over");
        let full = shield_glow(SPAWN_GRACE_MS, 500);
        assert!(full > 0.2, "it is visible at the start: {full}");
        assert!(full <= 1.0);
        assert!(
            shield_glow(SPAWN_GRACE_MS / 4, 500) < full,
            "it fades as the grace runs down"
        );
        // It beats rather than holding steady: a quarter of one of its three
        // beats per second later it is at its brightest.
        let a = shield_glow(SPAWN_GRACE_MS, 0);
        let b = shield_glow(SPAWN_GRACE_MS, SPAWN_GRACE_MS / 12);
        assert!(b > a + 0.2, "{a} {b}");
        assert_eq!(
            shield_glow(SPAWN_GRACE_MS * 5, 0),
            shield_glow(SPAWN_GRACE_MS, 0),
            "a stale clock never glows past full"
        );
    }

    #[test]
    fn the_status_line_shows_the_shield_a_respawn_buys() {
        assert_eq!(shield_note(0), None, "no shield once it has run out");
        assert_eq!(shield_note(SPAWN_GRACE_MS).as_deref(), Some("   SHIELD 1.0"));
        assert_eq!(shield_note(SPAWN_GRACE_MS / 2).as_deref(), Some("   SHIELD 0.5"));
        assert_eq!(
            shield_note(SPAWN_GRACE_MS * 3).as_deref(),
            Some("   SHIELD 1.0"),
            "it never reads past the grace it gives"
        );
    }

    #[test]
    fn a_bigger_dodge_shows_a_wider_bar() {
        assert_eq!(dodge_strength(DODGE_PUSH_NEAR), 1.0);
        assert_eq!(dodge_strength(DODGE_PUSH_FAR), 0.0);
        assert!((dodge_strength((DODGE_PUSH_NEAR + DODGE_PUSH_FAR) * 0.5) - 0.5).abs() < 0.01);

        // The bar keeps that shape at every point of its shrink.
        let close = dodge_bar_width(DODGE_FLASH_MS, dodge_strength(DODGE_PUSH_NEAR));
        let far = dodge_bar_width(DODGE_FLASH_MS, dodge_strength(DODGE_PUSH_FAR + 0.01));
        assert!(close > far, "{close} {far}");
        assert_eq!(far, DODGE_FLASH_FLOOR, "a barely-paid dodge keeps a floor width");
        assert!(far > DODGE_FLASH_WIDTH * 0.25, "and that floor is a bar, not a hair");
        assert_eq!(
            dodge_bar_width(1, 1.0),
            DODGE_FLASH_FLOOR,
            "even on its last frame it is visible"
        );
        assert_eq!(dodge_bar_width(0, 1.0), 0.0, "but it does go");
    }

    #[test]
    fn a_notice_fades_out_at_the_end() {
        assert_eq!(notice_alpha(NOTICE_MS), 255, "full while it is fresh");
        assert_eq!(notice_alpha(NOTICE_MS / 2), 255);
        let fading = notice_alpha(NOTICE_MS / 6);
        assert!(fading < 255 && fading > 0, "{fading}");
        assert_eq!(notice_alpha(0), 0, "gone by the time it runs out");
    }

    #[test]
    fn a_closer_bolt_pays_more_for_the_dodge() {
        let near = dodge_push(BOLT_HIT_RADIUS + 0.01);
        let middle = dodge_push((BOLT_HIT_RADIUS + BOLT_GRAZE_RADIUS) * 0.5);
        let far = dodge_push(BOLT_GRAZE_RADIUS - 0.01);
        assert!((near - DODGE_PUSH_NEAR).abs() < 0.1, "{near}");
        assert!((far - DODGE_PUSH_FAR).abs() < 0.1, "{far}");
        assert!(near > middle && middle > far, "{near} {middle} {far}");

        // Whatever happens with the clock, the pay stays inside the range.
        assert_eq!(dodge_push(0.0), DODGE_PUSH_NEAR);
        assert_eq!(dodge_push(1_000.0), DODGE_PUSH_FAR);
    }

    #[test]
    fn a_bolt_that_just_misses_counts_as_a_dodge() {
        assert!(!near_miss(0.0), "inside you is a hit, not a dodge");
        assert!(!near_miss(BOLT_HIT_RADIUS), "a graze on the hit edge");
        assert!(near_miss(70.0), "past your shoulder counts");
        assert!(!near_miss(BOLT_GRAZE_RADIUS), "a wide shot is no dodge");
        assert!(!near_miss(400.0));
    }

    #[test]
    fn a_bolt_still_on_its_way_in_is_not_yet_a_dodge() {
        // Coming at your face from 70 out: close enough to feel, but it has
        // not gone by, and paying here would reward the bolt that hits you.
        let eye = [0.0; 3];
        assert_eq!(
            grazed_by(eye, [80.0, 0.0, 0.0], [70.0, 0.0, 0.0], false),
            None
        );

        // Level with you, then past: that is the dodge.
        assert_eq!(
            grazed_by(eye, [10.0, 50.0, 0.0], [-10.0, 50.0, 0.0], false),
            Some(50.0)
        );
    }

    #[test]
    fn a_dodge_is_paid_once_and_only_for_a_close_pass() {
        let eye = [0.0; 3];
        // Wide enough to be no dodge at all.
        assert_eq!(
            grazed_by(eye, [10.0, 200.0, 0.0], [-10.0, 200.0, 0.0], false),
            None
        );
        // A bolt that did not move cannot have gone by.
        assert_eq!(
            grazed_by(eye, [50.0, 50.0, 0.0], [50.0, 50.0, 0.0], false),
            None
        );
        // And a bolt that already paid does not pay twice.
        assert_eq!(
            grazed_by(eye, [10.0, 50.0, 0.0], [-10.0, 50.0, 0.0], true),
            None
        );
    }

    #[test]
    fn a_bolt_covering_a_long_step_still_stops_at_a_thin_wall() {
        // A wall between the two positions of one slow frame: testing only
        // where the bolt ended up would put it on the far side of it.
        let wall = |point: [f32; 3]| (48.0..52.0).contains(&point[0]);
        let at = first_solid([0.0, 0.0, 0.0], [70.0, 0.0, 0.0], wall)
            .expect("the wall is between the two");
        assert!((48.0..52.0).contains(&at[0]), "stopped at {at:?}");

        assert!(
            first_solid([0.0, 0.0, 0.0], [40.0, 0.0, 0.0], wall).is_none(),
            "a step that stops short of it is clear"
        );
        assert!(
            first_solid([60.0, 0.0, 0.0], [100.0, 0.0, 0.0], wall).is_none(),
            "and so is one that starts past it"
        );
        assert!(
            first_solid([10.0, 0.0, 0.0], [10.0, 0.0, 0.0], wall).is_none(),
            "a bolt that did not move is in the open"
        );
        assert!(
            first_solid([10.0, 0.0, 0.0], [10.0, 0.0, 0.0], |_| true).is_some(),
            "though it still stops if it is already in a wall"
        );
    }

    #[test]
    fn a_post_keeps_quiet_between_shots() {
        // Nothing said yet: the first shot is always worth a line.
        assert!(worth_saying(None, 2, 1_000));
        // The same post again a moment later says nothing.
        assert!(!worth_saying(Some((2, 1_000)), 2, 1_400));
        assert!(!worth_saying(Some((2, 1_000)), 2, 1_000 + BOLT_CALL_REPEAT_MS - 1));
        // Once it has been quiet long enough, it speaks up again.
        assert!(worth_saying(Some((2, 1_000)), 2, 1_000 + BOLT_CALL_REPEAT_MS));
        // A different post is news straight away.
        assert!(worth_saying(Some((2, 1_000)), 5, 1_100));
    }

    #[test]
    fn a_shot_is_announced_with_which_way_to_look() {
        assert_eq!(side_words(0.0), "ahead");
        assert_eq!(side_words(-8.0), "ahead");
        assert_eq!(side_words(35.0), "front left");
        assert_eq!(side_words(-35.0), "front right");
        assert_eq!(side_words(90.0), "left");
        assert_eq!(side_words(-95.0), "right");
        assert_eq!(side_words(170.0), "behind left");
        assert_eq!(side_words(-170.0), "behind right");
    }

    #[test]
    fn the_threat_arrow_points_at_the_post() {
        let centre = [400.0, 300.0];
        let ahead = threat_arrow(centre, 0.0, 34.0);
        assert!(ahead[0][1] < centre[1] - 40.0, "straight ahead points up");
        assert!((ahead[0][0] - centre[0]).abs() < 0.01, "and stays centred");
        // Base of the head nearer the crosshair than the tip.
        assert!(ahead[1][1] > ahead[0][1] && ahead[2][1] > ahead[0][1]);

        let left = threat_arrow(centre, 90.0, 34.0);
        assert!(left[0][0] < centre[0] - 40.0, "your left points left");
        let right = threat_arrow(centre, -90.0, 34.0);
        assert!(right[0][0] > centre[0] + 40.0, "your right points right");

        // A post behind you stops at the side of the ring rather than wrapping
        // under the crosshair, where it would point at your own feet.
        let behind = threat_arrow(centre, 180.0, 34.0);
        assert!(behind[0][0] < centre[0], "{behind:?}");
        assert!((behind[0][1] - centre[1]).abs() < 0.01, "{behind:?}");
        // Everything stays close to the ring, clear of the crosshair itself.
        for corner in behind.iter().chain(left.iter()).chain(ahead.iter()) {
            let dx = corner[0] - centre[0];
            let dy = corner[1] - centre[1];
            let distance = (dx * dx + dy * dy).sqrt();
            assert!(distance > 28.0 && distance < 46.0, "{corner:?} at {distance}");
        }
    }

    #[test]
    fn a_short_wind_up_still_fills_its_glow() {
        let wind = WindUp {
            index: 0,
            from: [0.0, 0.0, 24.0],
            started_at: 1_000,
            fires_at: 1_000 + BOLT_TELEGRAPH_NEAR,
            lead: BOLT_LEAD,
        };
        assert_eq!(wind.progress(1_000), 0.0);
        assert!((wind.progress(1_000 + BOLT_TELEGRAPH_NEAR / 2) - 0.5).abs() < 0.02);
        assert_eq!(wind.progress(1_000 + BOLT_TELEGRAPH_NEAR), 1.0);
    }

    #[test]
    fn a_pair_costs_the_posts_more_of_a_wait_than_one_bolt() {
        assert_eq!(interval_after(0), BOLT_INTERVAL);
        assert_eq!(interval_after(1), BOLT_INTERVAL, "a lone shot keeps its rhythm");
        let pair = interval_after(2);
        assert!(pair > BOLT_INTERVAL, "surviving the bracket buys a longer breather");
        assert!(pair <= BOLT_INTERVAL * 2, "but not a ceasefire");
        assert_eq!(pair, interval_after(3), "no further penalty past a pair");
    }

    #[test]
    fn the_second_post_of_a_volley_warns_for_less_time() {
        for distance in [0.0, BOLT_RANGE * 0.5, BOLT_RANGE * 4.0] {
            assert_eq!(
                telegraph_for_slot(distance, 0),
                telegraph_for(distance),
                "a lone post is unchanged"
            );
            let second = telegraph_for_slot(distance, 1);
            assert!(
                second < telegraph_for(distance),
                "the pair is not twice the warning of one post"
            );
            assert!(
                second >= BOLT_TELEGRAPH_NEAR / 2,
                "still a warning, not a jump scare"
            );
        }
        assert!(
            telegraph_for_slot(BOLT_RANGE * 4.0, 1) > telegraph_for_slot(0.0, 1),
            "the second post still takes longer from further off"
        );
    }

    #[test]
    fn the_volley_chirp_is_short_quiet_and_rises() {
        let chirp = volley_chirp();
        let expected = (SAMPLE_RATE as u32 * VOLLEY_CHIRP_MS / 1_000) as usize;
        assert_eq!(chirp.len(), expected);
        assert!(expected as u32 > SAMPLE_RATE as u32 / 20, "long enough to hear");
        assert!(
            chirp[0].abs() < 0.01 && chirp[chirp.len() - 1].abs() < 0.01,
            "it starts and ends at silence, so it does not click"
        );
        assert!(chirp.iter().all(|s| s.abs() <= 0.4), "quiet enough to sit under a fight");
        // The second half is the higher note.
        let crossings = |slice: &[f32]| {
            slice
                .windows(2)
                .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
                .count()
        };
        let half = chirp.len() / 2;
        assert!(crossings(&chirp[half..]) > crossings(&chirp[..half]));
    }

    #[test]
    fn the_post_left_alone_by_a_fallen_leader_takes_the_full_warning() {
        let eye = [0.0, 0.0, 24.0];
        let from = [300.0, 0.0, 24.0];
        let short = telegraph_for_slot(away(from, eye), 1);
        let mut wind = WindUp {
            index: 1,
            from,
            started_at: 1_000,
            fires_at: 1_000 + short,
            lead: 0.0,
        };
        assert_eq!(wind.progress(1_000 + short), 1.0, "it was about to fire");

        take_the_lead(&mut wind, eye);
        assert_eq!(wind.lead, BOLT_LEAD, "the lone shot now leads the run");
        assert_eq!(
            wind.fires_at,
            1_000 + telegraph_for(away(from, eye)),
            "and warns for the time a lone post would"
        );
        assert!(
            wind.fires_at > 1_000 + short,
            "the pair's short warning is given back"
        );
        assert!(
            wind.progress(1_000 + short) < 1.0,
            "the glow keeps filling instead of starting over"
        );
    }

    #[test]
    fn a_post_shot_down_while_aiming_loses_its_shot() {
        let wind = WindUp {
            index: 2,
            from: [200.0, 0.0, 24.0],
            started_at: 0,
            fires_at: 450,
            lead: BOLT_LEAD,
        };
        assert_eq!(
            shot_state(&wind, false, true, 10),
            Shot::Drop,
            "down before it fires"
        );
        assert_eq!(
            shot_state(&wind, false, true, 450),
            Shot::Drop,
            "and still dropped when its turn comes"
        );
        assert_eq!(shot_state(&wind, true, true, 10), Shot::Wait);
        assert_eq!(shot_state(&wind, true, true, 450), Shot::Fire);
        assert_eq!(
            shot_state(&wind, true, true, 900),
            Shot::Fire,
            "late is still a shot"
        );
    }

    #[test]
    fn a_post_lets_go_of_a_shot_at_a_body_on_the_floor() {
        let wind = WindUp {
            index: 1,
            from: [200.0, 0.0, 24.0],
            started_at: 0,
            fires_at: 450,
            lead: BOLT_LEAD,
        };
        // The post is still standing, so only the player being down decides it.
        assert_eq!(shot_state(&wind, true, false, 200), Shot::Drop);
        assert_eq!(
            shot_state(&wind, true, false, 450),
            Shot::Drop,
            "a bolt is not spent on a corpse"
        );
    }

    #[test]
    fn posts_in_the_open_on_both_sides_line_up_together() {
        assert_eq!(MAX_FIRING_POSTS, 2, "a fight, not a wall of bolts");
        // One ahead and one to the side, plus a third on the first post's side.
        let field = TargetField::new(&[[100.0, 0.0, 0.0], [0.0, 150.0, 0.0], [900.0, 0.0, 0.0]]);
        let chosen = volley_candidates(&field, [0.0; 3], &[], MAX_FIRING_POSTS, |_, _| true);
        assert_eq!(chosen.len(), 2, "the two nearest that bracket you");
        assert_eq!(chosen[0].0, 0, "nearest first, so it fires first");
        assert_eq!(chosen[1].0, 1, "from a different side");
        assert!((chosen[0].2 - 100.0).abs() < 0.01);
        assert_eq!(lead_for(0), BOLT_LEAD, "the first post leads the shot");
        assert_eq!(lead_for(1), 0.0, "the second aims where you stand");
    }

    #[test]
    fn a_post_behind_a_wall_or_already_firing_is_left_out() {
        let field = TargetField::new(&[[100.0, 0.0, 0.0], [0.0, 150.0, 0.0]]);
        // The second post is behind something as far as this test is concerned.
        let blocked = volley_candidates(&field, [0.0; 3], &[], MAX_FIRING_POSTS, |chest, _| {
            chest[0] > 50.0
        });
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].0, 0);

        // With the nearest already lining up, the other takes the free slot.
        let busy = [WindUp {
            index: 0,
            from: [100.0, 0.0, 0.0],
            started_at: 0,
            fires_at: 450,
            lead: BOLT_LEAD,
        }];
        let next = volley_candidates(&field, [0.0; 3], &busy, MAX_FIRING_POSTS - 1, |_, _| true);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].0, 1, "no post takes two shots at once");
        assert!(
            volley_candidates(&field, [0.0; 3], &busy, 0, |_, _| true).is_empty(),
            "no room means no volley"
        );
    }

    #[test]
    fn two_posts_in_a_line_only_get_one_shot_between_them() {
        // Both straight ahead: one step sideways still leaves both bolts behind,
        // so there is nothing to bracket and only the near post fires.
        let field = TargetField::new(&[[100.0, 0.0, 0.0], [300.0, 0.0, 0.0], [900.0, 0.0, 0.0]]);
        let chosen = volley_candidates(&field, [0.0; 3], &[], MAX_FIRING_POSTS, |_, _| true);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].0, 0);

        let eye = [0.0, 0.0, 0.0];
        assert!(!brackets(eye, [100.0, 0.0, 0.0], [300.0, 0.0, 0.0]), "in line");
        assert!(brackets(eye, [100.0, 0.0, 0.0], [0.0, 150.0, 0.0]), "at right angles");
        assert!(brackets(eye, [100.0, 0.0, 0.0], [-100.0, 0.0, 0.0]), "opposite sides");
        assert!(
            !brackets(eye, [100.0, 0.0, 0.0], [200.0, 90.0, 0.0]),
            "barely twenty degrees apart"
        );
        assert!(!brackets(eye, eye, [100.0, 0.0, 0.0]), "a post on top of you brackets nothing");
    }

    #[test]
    fn a_shot_that_stops_on_a_post_ends_there() {
        // The map knows only its walls, so its own trace runs on to the far
        // one; the post in the way is what the round reaches first.
        let mut field = TargetField::new(&[[240.0, 0.0, 24.0]]);
        let shot = Round {
            from: [0.0, 0.0, 40.0],
            to: [600.0, 0.0, 40.0],
            hit: true,
            splash: 0.0,
            damage: 34,
        };
        let (stop, struck) = shot_end(&shot, &mut field, 1_000);
        assert!(
            (stop[0] - 224.0).abs() < 0.001,
            "the mark and the beam end on the post's near side: {stop:?}"
        );
        assert!(struck.is_some(), "and the post takes the round");
        assert_eq!(field.targets()[0].health, 66);

        // Nothing in the way: the shot still ends where the map said.
        let clear = Round {
            from: [0.0, 200.0, 40.0],
            to: [600.0, 200.0, 40.0],
            hit: true,
            splash: 0.0,
            damage: 34,
        };
        let (stop, struck) = shot_end(&clear, &mut field, 1_000);
        assert_eq!(stop, clear.to);
        assert!(struck.is_none());
    }

    #[test]
    fn a_post_out_of_reach_is_left_out() {
        let far = TargetField::new(&[[BOLT_RANGE + 50.0, 0.0, 0.0]]);
        assert!(volley_candidates(&far, [0.0; 3], &[], MAX_FIRING_POSTS, |_, _| true).is_empty());
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
        let aimed = bolt_lead(eye, running, 0.5, BOLT_LEAD);
        // Half a second at 400 a second is 200 units, so half is 100.
        assert!((aimed[0] - 200.0).abs() < 0.01, "{aimed:?}");
        // Standing still, the aim is exactly where you are.
        assert_eq!(bolt_lead(eye, [0.0; 3], 0.5, BOLT_LEAD), eye);
        // Without lead it aims at where you stand, wall or no wall.
        assert_eq!(bolt_lead(eye, running, 0.5, 0.0), eye);
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
