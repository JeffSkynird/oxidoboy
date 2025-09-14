use oxido_sdk::*;
use std::sync::OnceLock;
use oxido_sdk::{AnimFrame, Animator};

static ANIM_PLAYER_FRAMES: [AnimFrame; 4] = [
    AnimFrame { tile: 0, millis: 120, fx: false, fy: false },
    AnimFrame { tile: 1, millis: 120, fx: false, fy: false },
    AnimFrame { tile: 2, millis: 120, fx: false, fy: false },
    AnimFrame { tile: 1, millis: 120, fx: false, fy: false },
];
static mut ANIM_PLAYER: Option<Animator> = None;
static mut FACE_LEFT: bool = false; // player orientation


// --- State -----------------------------------------------------------------
static mut FB: [u8; DEFAULT_W * DEFAULT_H * 4] = [0; DEFAULT_W * DEFAULT_H * 4];
static mut INPUT_BITS: u32 = 0;
static mut PREV_INPUT_BITS: u32 = 0;

static mut X: f32 = 10.0;         // player
static mut SCROLL_X: f32 = 0.0;   // map scroll (px)
static mut SCROLL_Y: f32 = 0.0;

const SPEED: f32 = 60.0;       // player px/s
const SCROLL_SPEED: f32 = 40.0; // scroll px/s
const PLAYER_W: i32 = 16;
const PLAYER_H: i32 = 16;

// --- Palette ---------------------------------------------------------------
static PALETTES: OnceLock<Vec<Palette>> = OnceLock::new();
static mut PAL_IDX: usize = 0;
fn palettes() -> &'static Vec<Palette> {
    PALETTES.get_or_init(|| {
        vec![
            Palette([P0, P1, P2, P3]),
            Palette([rgba(15,15,15,255), rgba(80,80,80,255), rgba(160,160,160,255), rgba(240,240,240,255)]),
            Palette([rgba(20,8,0,255),   rgba(120,56,8,255),  rgba(200,120,24,255), rgba(255,208,128,255)]),
            Palette([rgba(0,12,24,255),  rgba(16,64,120,255), rgba(72,140,200,255), rgba(180,220,255,255)]),
            Palette([rgba(0,0,0,255),    rgba(64,64,64,255),  rgba(192,192,192,255),rgba(255,255,255,255)]),
        ]
    })
}
fn current_pal() -> &'static Palette { let list = palettes(); unsafe { &list[PAL_IDX % list.len()] } }

// --- Atlas multi-tile (2x2 tiles 8x8 => 16x16) ------------------------------
fn build_atlas() -> SpriteAtlas {
    let w = 16usize; let h = 16usize; let tile_w = 8usize; let tile_h = 8usize;
    let mut px = vec![0u8; w * h];
    let mut put_tile = |tx: usize, ty: usize, f: &dyn Fn(usize, usize) -> u8| {
        for y in 0..tile_h { for x in 0..tile_w {
            let gx = tx * tile_w + x; let gy = ty * tile_h + y;
            px[gy * w + gx] = f(x, y) & 0b11;
    }}};
    put_tile(0,0,&|x,y| if ((x^y)&1)==0 {1} else {2});              // T0
    put_tile(1,0,&|x,y| if (x+y)%7==0 {3} else {1});                 // T1
    put_tile(0,1,&|x,_| if x%2==0 {2} else {3});                     // T2
    put_tile(1,1,&|x,y| if x==0||y==0||x==7||y==7 {3} else {2});     // T3
    SpriteAtlas::from_indexed(px, w, h, tile_w, tile_h)
}
static ATLAS: OnceLock<SpriteAtlas> = OnceLock::new();
fn atlas() -> &'static SpriteAtlas { ATLAS.get_or_init(build_atlas) }

// --- Tilemap (32x32 tiles) --------------------------------------------------
const MAP_W: usize = 32;
const MAP_H: usize = 32;
fn build_map() -> TileMap {
    let mut tiles = vec![0usize; MAP_W * MAP_H];
    for y in 0..MAP_H { for x in 0..MAP_W {
        let base = if ((x / 4) + (y / 4)) % 2 == 0 { 0 } else { 1 };
        tiles[y * MAP_W + x] = base;
    }}
    for x in 0..MAP_W { tiles[(MAP_H/2) * MAP_W + x] = 2; }
    for x in 0..MAP_W { tiles[x] = 3; tiles[(MAP_H-1)*MAP_W + x] = 3; }
    for y in 0..MAP_H { tiles[y*MAP_W] = 3; tiles[y*MAP_W + (MAP_W-1)] = 3; }
    TileMap::new(MAP_W, MAP_H, 8, 8, tiles)
}
static MAP: OnceLock<TileMap> = OnceLock::new();
fn map() -> &'static TileMap { MAP.get_or_init(build_map) }

// ---- Tile collisions (AABB) -------------------------------------------
// ---- Col
fn tile_is_solid(id: usize) -> bool { id == 3 }
fn tile_id_at_world(wx: i32, wy: i32) -> usize {
    let m = map(); let tw = m.tile_w as i32; let th = m.tile_h as i32;
    let tx = (wx.div_euclid(tw)).rem_euclid(m.w as i32) as usize;
    let ty = (wy.div_euclid(th)).rem_euclid(m.h as i32) as usize;
    m.tiles[ty * m.w + tx]
}
fn rect_collides_world(x: i32, y: i32, w: i32, h: i32) -> bool {
    let corners = [(x,y), (x+w-1,y), (x,y+h-1), (x+w-1,y+h-1)];
    for (cx, cy) in corners {
        if tile_is_solid(tile_id_at_world(cx, cy)) { return true; }
    }
    false
}

// ===================== AUDIO (status exported to host) ======================
// Layout must match WireCh on host (13 fields x 4 bytes)
#[repr(C)]
#[derive(Copy, Clone)]
struct AudioCh {
    kind: u32,     // 0=pulse,1=pulse,2=noise
    base_freq: f32,
    vol:  f32,     // 0..1
    duty: f32,     // pulse
    gate: u32,     // 1=on

    // ADSR (ms / nivel 0..1)
    a_ms: f32, d_ms: f32, s_lvl: f32, r_ms: f32,

    // Arpeggio
    arp_a: i32, arp_b: i32, arp_c: i32, arp_rate_hz: f32,
}
static mut AUDIO_STATE: [AudioCh; 4] = [AudioCh{
    kind:0, base_freq:0.0, vol:0.0, duty:0.5, gate:0,
    a_ms: 0.0, d_ms: 0.0, s_lvl: 0.0, r_ms: 0.0,
    arp_a:0, arp_b:0, arp_c:0, arp_rate_hz:0.0
}; 4];

#[no_mangle]
pub extern "C" fn oxido_audio_state_ptr() -> *const u8 {
    unsafe { AUDIO_STATE.as_ptr() as *const u8 }
}
#[no_mangle]
pub extern "C" fn oxido_audio_state_len() -> usize {
    core::mem::size_of::<AudioCh>() * 4
}

// --- ABI --------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn oxido_init() {
    let _ = palettes();
    unsafe {
        // channels setup
        AUDIO_STATE[0] = AudioCh {
            kind:0, base_freq:440.0, vol:0.0, duty:0.5, gate:0,
            a_ms:5.0, d_ms:80.0, s_lvl:0.25, r_ms:120.0,
            arp_a:0, arp_b:7, arp_c:12, arp_rate_hz:18.0
        };
        AUDIO_STATE[1] = AudioCh {
            kind:1, base_freq:660.0, vol:0.0, duty:0.25, gate:0,
            a_ms:1.0, d_ms:40.0, s_lvl:0.20, r_ms:80.0,
            arp_a:0, arp_b:0, arp_c:0, arp_rate_hz:0.0
        };
        AUDIO_STATE[2] = AudioCh {
            kind:2, base_freq:2000.0, vol:0.0, duty:0.0, gate:0,
            a_ms:0.0, d_ms:40.0, s_lvl:0.0, r_ms:60.0,
            arp_a:0, arp_b:0, arp_c:0, arp_rate_hz:0.0
        };
        AUDIO_STATE[3] = AudioCh {
            kind:0, base_freq:330.0, vol:0.0, duty:0.75, gate:0,
            a_ms:8.0, d_ms:100.0, s_lvl:0.30, r_ms:150.0,
            arp_a:-12, arp_b:0, arp_c:7, arp_rate_hz:12.0
        };
        ANIM_PLAYER = Some(Animator::new(&ANIM_PLAYER_FRAMES));
    }
}

#[no_mangle]
pub extern "C" fn oxido_update(dt_ms: f32) {
    let dt = dt_ms / 1000.0;
    unsafe {
        // scroll Y
        if INPUT_BITS & key_bit(Key::Up)   != 0 { SCROLL_Y -= SCROLL_SPEED * dt; }
        if INPUT_BITS & key_bit(Key::Down) != 0 { SCROLL_Y += SCROLL_SPEED * dt; }

        // move player in X with collision
        let mut new_x = X;
        if INPUT_BITS & key_bit(Key::Right) != 0 { new_x += SPEED * dt; }
        if INPUT_BITS & key_bit(Key::Left)  != 0 { new_x -= SPEED * dt; }

        let world_y = (60.0 + SCROLL_Y).floor() as i32;
        let world_x_new = (new_x + SCROLL_X).floor() as i32;

        if !rect_collides_world(world_x_new, world_y, PLAYER_W, PLAYER_H) {
            X = new_x;
        }

        // Orientqation according to input
        if INPUT_BITS & key_bit(Key::Left)  != 0 { FACE_LEFT = true; }
        if INPUT_BITS & key_bit(Key::Right) != 0 { FACE_LEFT = false; }

        // --- Animation: also runs with Up/Down ---
        let moving_h = (INPUT_BITS & (key_bit(Key::Left) | key_bit(Key::Right))) != 0;
        let moving_v = (INPUT_BITS & (key_bit(Key::Up)   | key_bit(Key::Down)))  != 0;
        let moving = moving_h || moving_v;

        if let Some(ref mut a) = ANIM_PLAYER {
            a.playing = moving;
            if moving {
                a.tick(dt_ms);      // advances frames when there is movement on any axis
            } else {
                // to return to the first frame when it stops:
                // a.reset();
            }
        }

        // Camera X
        let map_px_w = (map().w * map().tile_w) as i32;
        let view_w = DEFAULT_W as i32;
        let target_cam_x = (X as i32 + PLAYER_W/2) - (view_w/2);
        let cam_x = target_cam_x.clamp(0, (map_px_w - view_w).max(0));
        SCROLL_X = cam_x as f32;

        // Pallettes (edges)
        let start = key_bit(Key::Start);
        let select = key_bit(Key::Select);
        let pressed = |mask: u32| (INPUT_BITS & mask) != 0 && (PREV_INPUT_BITS & mask) == 0;
        if pressed(start)  { let len=palettes().len(); PAL_IDX = (PAL_IDX + 1) % len; }
        if pressed(select) { let len=palettes().len(); PAL_IDX = (PAL_IDX + len - 1) % len; }

        // ====== AUDIO DEMO ======
        // Z (A): bip width ADSR + triad arpeggio (0,7,12)
        let a_down = (INPUT_BITS & key_bit(Key::A)) != 0;
        AUDIO_STATE[0].vol  = 0.35;
        AUDIO_STATE[0].gate = if a_down { 1 } else { 0 };

        // X (B): noise short “click”
        let b_down = (INPUT_BITS & key_bit(Key::B)) != 0;
        AUDIO_STATE[2].base_freq = 2200.0;
        AUDIO_STATE[2].vol  = 0.25;
        AUDIO_STATE[2].gate = if b_down { 1 } else { 0 };

        PREV_INPUT_BITS = INPUT_BITS;
    }
}

// --- Rendering ----------------------------------------------------------------
#[no_mangle]
pub extern "C" fn oxido_draw_ptr() -> *const u8 {
    unsafe {
        let mut f = Frame { data: &mut FB, w: DEFAULT_W, h: DEFAULT_H };
        let pal = current_pal();

        // Background and player
        map().draw(&mut f, atlas(), pal, SCROLL_X as i32, SCROLL_Y as i32, false);

        // Player (sprite 8x8 centered in hitbox 16x16)
        let (fx, fy, tile) = if let Some(ref a) = ANIM_PLAYER {
            let fr = a.current();
            (fr.fx ^ unsafe { FACE_LEFT }, fr.fy, fr.tile)
        } else { (false, false, 0) };

        let xi = X as i32;
        let yi = 60;
        // sprite 8x8 centered in hitbox 16x16
        let sprite_w = 8;
        let sprite_h = 8;
        let ox = (PLAYER_W - sprite_w) / 2; // 4
        let oy = (PLAYER_H - sprite_h) / 2; // 4

        // HITBOX (debug)
        //f.rect(xi, yi, PLAYER_W, PLAYER_H, pal.color(3));

        atlas().blit(&mut f, xi + ox, yi + oy, tile, pal, fx, fy, true);

        // HUD
        f.rect(1, 1, 158, 14, pal.color(1));
        f.text5x7(4, 4, &format!("PAL {}  Z=ADSR+ARP  X=NOISE", unsafe { PAL_IDX }), pal.color(3));

        FB.as_ptr()
    }
}

#[no_mangle] pub extern "C" fn oxido_draw_len() -> usize { DEFAULT_W * DEFAULT_H * 4 }
#[no_mangle] pub extern "C" fn oxido_input_set(bits: u32) { unsafe { INPUT_BITS = bits; } }
