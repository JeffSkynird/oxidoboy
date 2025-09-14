use anyhow::*;
use pixels::{Pixels, SurfaceTexture};
use wasmtime::*;
use winit::{
    dpi::LogicalSize,
    event::*,
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use winit::event::{ElementState, VirtualKeyCode};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

// ===================== Audio (host) ===============================

#[derive(Clone, Copy, Debug, Default)]
struct HostCh {
    // Parameters received from the game
    kind: u32,          // 0=pulse, 1=pulse, 2=noise
    base_freq: f32,     // Hz
    vol: f32,           // 0..1 (base gain)
    duty: f32,          // 0..1 (pulse)
    gate: bool,         // on/off

    // envelope (ms / level 0..1)
    a_ms: f32, d_ms: f32, s_lvl: f32, r_ms: f32,

    // arpeggio (semitones relative) and rate in Hz
    arp_a: i32, arp_b: i32, arp_c: i32, arp_rate_hz: f32,

    // runtime state
    phase: f32,         // 0..1 (pulse)
    noise: u32,         // LFSR
    env_level: f32,     // 0..1
    env_state: u32,     // 0=idle,1=A,2=D,3=S,4=R
    gate_prev: bool,
    arp_phase: f32,     // 0..1 (0..1 → A→B→C)
}

#[derive(Clone, Copy, Default)]
struct WireCh {
    // exact layout sent by the game (13 * 4 bytes)
    kind: u32, base_freq: f32, vol: f32, duty: f32, gate: u32,
    a_ms: f32, d_ms: f32, s_lvl: f32, r_ms: f32,
    arp_a: i32, arp_b: i32, arp_c: i32, arp_rate_hz: f32,
}

struct AudioEngine {
    channels: Arc<Mutex<[HostCh; 4]>>,
    _stream: cpal::Stream,
    sample_rate: f32,
}

impl AudioEngine {
    fn new() -> Option<Self> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let cfg = device.default_output_config().ok()?;
        let sample_rate = cfg.sample_rate().0 as f32;

        let channels = Arc::new(Mutex::new([HostCh::default(); 4]));

        let chs = channels.clone();
        let build = |sf| -> Result<cpal::Stream> {
            let config = cpal::StreamConfig {
                channels: 2,
                sample_rate: cpal::SampleRate(sample_rate as u32),
                buffer_size: cpal::BufferSize::Default,
            };

            match sf {
                cpal::SampleFormat::F32 => {
                    let mut t = 0usize;
                    Ok(device.build_output_stream(
                        &config,
                        move |out: &mut [f32], _| fill_buffer(out, sample_rate, &chs, &mut t),
                        move |e| eprintln!("audio error: {e}"),
                        None,
                    )?)
                }
                cpal::SampleFormat::I16 => {
                    let mut t = 0usize;
                    Ok(device.build_output_stream(
                        &config,
                        move |out: &mut [i16], _| {
                            let mut buf = vec![0.0f32; out.len()];
                            fill_buffer(&mut buf, sample_rate, &chs, &mut t);
                            for (i, s) in buf.iter().enumerate() {
                                out[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                            }
                        },
                        move |e| eprintln!("audio error: {e}"),
                        None,
                    )?)
                }
                cpal::SampleFormat::U16 => {
                    let mut t = 0usize;
                    Ok(device.build_output_stream(
                        &config,
                        move |out: &mut [u16], _| {
                            let mut buf = vec![0.0f32; out.len()];
                            fill_buffer(&mut buf, sample_rate, &chs, &mut t);
                            for (i, s) in buf.iter().enumerate() {
                                out[i] = (((s.clamp(-1.0, 1.0) * 0.5) + 0.5) * u16::MAX as f32) as u16;
                            }
                        },
                        move |e| eprintln!("audio error: {e}"),
                        None,
                    )?)
                }
                _ => bail!("Unsupported audio format"),
            }
        };

        let stream = build(cfg.sample_format()).ok()?;
        stream.play().ok()?;
        Some(Self { channels, _stream: stream, sample_rate })
    }

    fn set_params(&self, src: &[WireCh]) {
        if let std::result::Result::Ok(mut dst) = self.channels.lock() {
            for i in 0..dst.len().min(src.len()) {
                let prev = dst[i];
                let s = src[i];

                // update parameters, keep runtime state
                let mut h = prev;
                h.kind = s.kind;
                h.base_freq = s.base_freq;
                h.vol = s.vol;
                h.duty = s.duty;
                h.gate = s.gate != 0;

                h.a_ms = s.a_ms.max(0.0);
                h.d_ms = s.d_ms.max(0.0);
                h.s_lvl = s.s_lvl.clamp(0.0, 1.0);
                h.r_ms = s.r_ms.max(0.0);

                h.arp_a = s.arp_a;
                h.arp_b = s.arp_b;
                h.arp_c = s.arp_c;
                h.arp_rate_hz = s.arp_rate_hz.max(0.0);

                dst[i] = h;
            }
        }
    }
}

fn hz_for_semitone(base: f32, semi: i32) -> f32 {
    if semi == 0 { return base; }
    base * (2.0f32).powf(semi as f32 / 12.0)
}

fn step_env(ch: &mut HostCh, step: f32) {
    let a = ch.a_ms / 1000.0;
    let d = ch.d_ms / 1000.0;
    let r = ch.r_ms / 1000.0;
    let s = ch.s_lvl;

    // detect edges
    if ch.gate && !ch.gate_prev {
        ch.env_state = 1; // A
        if a <= 0.0 { ch.env_level = 1.0; ch.env_state = 2; }
    } else if !ch.gate && ch.gate_prev {
        ch.env_state = 4; // R
        if r <= 0.0 { ch.env_level = 0.0; ch.env_state = 0; }
    }
    ch.gate_prev = ch.gate;

    match ch.env_state {
        1 => { // Attack (0→1)
            if a > 0.0 { ch.env_level += step / a; } else { ch.env_level = 1.0; }
            if ch.env_level >= 1.0 { ch.env_level = 1.0; ch.env_state = 2; }
        }
        2 => { // Decay (1→S)
            if d > 0.0 {
                let delta = (1.0 - s).max(0.0);
                ch.env_level -= (step / d) * delta;
            } else { ch.env_level = s; }
            if ch.env_level <= s { ch.env_level = s; ch.env_state = 3; }
        }
        3 => { // Sustain
            ch.env_level = s;
            if !ch.gate { ch.env_state = 4; }
        }
        4 => { // Release (→0)
            if r > 0.0 { ch.env_level -= (step / r) * ch.env_level.max(0.0); }
            else { ch.env_level = 0.0; }
            if ch.env_level <= 0.0 { ch.env_level = 0.0; ch.env_state = 0; }
        }
        _ => { ch.env_level = if ch.gate { 1.0 } else { 0.0 }; }
    }
}

fn fill_buffer(out: &mut [f32], sr: f32, channels: &Arc<Mutex<[HostCh; 4]>>, t_counter: &mut usize) {
    // 1) state snapshot
    let mut loc = [HostCh::default(); 4];
    if let std::result::Result::Ok(src) = channels.lock() {
        loc.copy_from_slice(&*src);
    }

    let step = 1.0 / sr;

    for frame in out.chunks_exact_mut(2) {
        let mut mix = 0.0f32;

        for ch in loc.iter_mut() {
            // Envelope
            step_env(ch, step);

            // Arpeggio
            let mut freq = ch.base_freq;
            if ch.arp_rate_hz > 0.0 {
                ch.arp_phase += step * ch.arp_rate_hz;
                if ch.arp_phase >= 1.0 { ch.arp_phase -= 1.0; }
                let seg = (ch.arp_phase * 3.0) as u32 % 3;
                let semi = match seg {
                    0 => ch.arp_a,
                    1 => ch.arp_b,
                    _ => ch.arp_c,
                };
                if semi != 0 { freq = hz_for_semitone(freq, semi); }
            }

            let amp = (ch.vol * ch.env_level).clamp(0.0, 1.0);
            if amp <= 0.0001 { continue; }

            match ch.kind {
                0 | 1 => {
                    ch.phase += freq * step;
                    if ch.phase >= 1.0 { ch.phase -= 1.0; }
                    let s = if ch.phase < ch.duty { 1.0 } else { -1.0 };
                    mix += s * amp;
                }
                2 => { // noise
                    let nsteps = (sr / freq.max(1.0)).max(1.0) as u32;
                    if *t_counter as u32 % nsteps == 0 {
                        let bit = ((ch.noise ^ (ch.noise >> 1)) & 1) as u32;
                        ch.noise = ((ch.noise >> 1) | (bit << 14)) & 0x7FFF;
                        if ch.noise == 0 { ch.noise = 0x4000; }
                    }
                    let s = if (ch.noise & 1) != 0 { 1.0 } else { -1.0 };
                    mix += s * amp;
                }
                _ => {}
            }
        }

        *t_counter = t_counter.wrapping_add(1);
        mix = (mix * 0.25).clamp(-1.0, 1.0); // headroom
        frame[0] = mix;
        frame[1] = mix;
    }

    // 3) return updated state (phase, env, arp…) to engine
    if let std::result::Result::Ok(mut dst) = channels.lock() {
        *dst = loc;
    }
}

// ===================== Runtime (video+input+hotreload) =====================

pub struct Cartridge {
    pub wasm_path: std::path::PathBuf,
    pub w: u32,
    pub h: u32,
}

pub fn run(cart: Cartridge) -> Result<()> {
    const FRAME_TIME: Duration = Duration::from_micros(16_667); // ~60 Hz

    // Event loop
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("OxidoBoy")
        .with_inner_size(LogicalSize::new(cart.w as f64, cart.h as f64))
        .build(&event_loop)?;
    let size = window.inner_size();

    // pixels
    let mut pixels = Pixels::new(
        cart.w,
        cart.h,
        SurfaceTexture::new(size.width, size.height, &window),
    )?;

    // WASM setup
    let engine = Engine::default();

    fn instantiate_all(
        engine: &Engine,
        wasm_path: &std::path::Path,
    ) -> Result<(
        Store<()>,
        Instance,
        Memory,
        TypedFunc<(), ()>,     // init
        TypedFunc<f32, ()>,    // update
        TypedFunc<(), u32>,    // draw_ptr
        TypedFunc<(), u32>,    // draw_len
        TypedFunc<u32, ()>,    // input_set
        Option<TypedFunc<(), u32>>, // audio_state_ptr
        Option<TypedFunc<(), u32>>, // audio_state_len (bytes)
    )> {
        let module = Module::from_file(engine, wasm_path)?;
        let linker = Linker::new(engine);
        let mut store = Store::new(engine, ());
        let instance = linker.instantiate(&mut store, &module)?;

        let memory   = instance.get_memory(&mut store, "memory").context("no memory export")?;
        let init     = instance.get_typed_func::<(), ()>(&mut store, "oxido_init").context("missing oxido_init")?;
        let update   = instance.get_typed_func::<f32, ()>(&mut store, "oxido_update").context("missing oxido_update")?;
        let draw_ptr = instance.get_typed_func::<(), u32>(&mut store, "oxido_draw_ptr").context("missing oxido_draw_ptr")?;
        let draw_len = instance.get_typed_func::<(), u32>(&mut store, "oxido_draw_len").context("missing oxido_draw_len")?;
        let input_set= instance.get_typed_func::<u32, ()>(&mut store, "oxido_input_set").context("missing oxido_input_set")?;

        let audio_ptr = instance.get_typed_func::<(), u32>(&mut store, "oxido_audio_state_ptr").ok();
        let audio_len = instance.get_typed_func::<(), u32>(&mut store, "oxido_audio_state_len").ok();

        Ok((store, instance, memory, init, update, draw_ptr, draw_len, input_set, audio_ptr, audio_len))
    }

    let (mut store, mut _instance, mut memory, mut init, mut update, mut draw_ptr, mut draw_len, mut input_set, mut audio_ptr_fn, mut audio_len_fn)
        = instantiate_all(&engine, &cart.wasm_path)?;
    init.call(&mut store, ())?;

    let mut last_mtime: SystemTime = fs::metadata(&cart.wasm_path)
        .and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    let mut reload_count: u32 = 0;

    // Audio
    let audio_engine = AudioEngine::new();

    // Input
    let mut input_bits: u32 = 0;
    fn bit_from_scancode(sc: u32) -> u32 {
        match sc {
            103 => 1 << 0, 108 => 1 << 1, 105 => 1 << 2, 106 => 1 << 3,
            44  => 1 << 4, 45  => 1 << 5, 28  => 1 << 6, 42 | 54 => 1 << 7,
            _ => 0,
        }
    }

    // Overlay + pacing
    let mut last = Instant::now();
    let mut fps_timer = Instant::now();
    let mut frames: u32 = 0;
    let mut ms_accum: f32 = 0.0;
    let mut next_frame = Instant::now();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_frame);
        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
                WindowEvent::KeyboardInput { input, .. } => {
                    let pressed = input.state == ElementState::Pressed;
                    let mut bit = match input.virtual_keycode {
                        Some(VirtualKeyCode::Up)    => 1 << 0,
                        Some(VirtualKeyCode::Down)  => 1 << 1,
                        Some(VirtualKeyCode::Left)  => 1 << 2,
                        Some(VirtualKeyCode::Right) => 1 << 3,
                        Some(VirtualKeyCode::Z)     => 1 << 4,
                        Some(VirtualKeyCode::X)     => 1 << 5,
                        Some(VirtualKeyCode::Return)=> 1 << 6,
                        Some(VirtualKeyCode::LShift)| Some(VirtualKeyCode::RShift) => 1 << 7,
                        _ => 0,
                    };
                    if bit == 0 { bit = bit_from_scancode(input.scancode); }
                    if bit != 0 {
                        if pressed { input_bits |= bit; } else { input_bits &= !bit; }
                    }
                }
                WindowEvent::Focused(false) => { input_bits = 0; },
                _ => {}
            },

            Event::MainEventsCleared => {
                // dt + FPS
                let now = Instant::now();
                let dt_ms = (now - last).as_secs_f32() * 1000.0;
                last = now;
                frames += 1;
                ms_accum += dt_ms;

                // Hot-reload
                match fs::metadata(&cart.wasm_path) {
                    std::result::Result::Ok(meta) => match meta.modified() {
                        std::result::Result::Ok(mod_time) => {
                            if mod_time > last_mtime {
                                match instantiate_all(&engine, &cart.wasm_path) {
                                    std::result::Result::Ok((s, i, mem, ini, upd, dptr, dlen, iset, ap, al)) => {
                                        store = s; _instance = i; memory = mem;
                                        init = ini; update = upd; draw_ptr = dptr; draw_len = dlen; input_set = iset;
                                        audio_ptr_fn = ap; audio_len_fn = al;
                                        let _ = init.call(&mut store, ());
                                        last_mtime = mod_time;
                                        reload_count += 1;
                                        eprintln!("🔁 OxidoBoy: reloaded {}", cart.wasm_path.display());
                                    }
                                    _ => eprintln!("⚠️  OxidoBoy: reload failed; keeping the previous version"),
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }

                // input + update
                let _ = input_set.call(&mut store, input_bits);
                let _ = update.call(&mut store, dt_ms);

                // video
                let ptr = draw_ptr.call(&mut store, ()).unwrap() as usize;
                let len = draw_len.call(&mut store, ()).unwrap() as usize;
                let data = memory.data(&store);
                let frame = pixels.frame_mut();
                frame.copy_from_slice(&data[ptr..ptr + len]);

                // === Audio: read game state and set parameters ===
                if let (Some(ref ap), Some(ref al), Some(ref eng)) =
                    (audio_ptr_fn.as_ref(), audio_len_fn.as_ref(), audio_engine.as_ref())
                {
                    if let (std::result::Result::Ok(ptr_u32), std::result::Result::Ok(len_u32)) =
                        (ap.call(&mut store, ()), al.call(&mut store, ()))
                    {
                        let ptr = ptr_u32 as usize;
                        let blen = len_u32 as usize;

                        // 4 channels * 13 fields * 4 bytes
                        if blen >= 4 * 13 * 4 {
                            let slice = &memory.data(&store)[ptr..ptr + blen];
                            let mut chans = [WireCh::default(); 4];
                            let mut off = 0usize;
                            for i in 0..4 {
                                let rd_u32 = |s: &[u8], o: &mut usize| { let v = u32::from_le_bytes(s[*o..*o+4].try_into().unwrap()); *o+=4; v };
                                let rd_f32 = |s: &[u8], o: &mut usize| { let v = f32::from_le_bytes(s[*o..*o+4].try_into().unwrap()); *o+=4; v };
                                let rd_i32 = |s: &[u8], o: &mut usize| { let v = i32::from_le_bytes(s[*o..*o+4].try_into().unwrap()); *o+=4; v };

                                chans[i].kind        = rd_u32(slice, &mut off);
                                chans[i].base_freq   = rd_f32(slice, &mut off);
                                chans[i].vol         = rd_f32(slice, &mut off);
                                chans[i].duty        = rd_f32(slice, &mut off);
                                chans[i].gate        = rd_u32(slice, &mut off);

                                chans[i].a_ms        = rd_f32(slice, &mut off);
                                chans[i].d_ms        = rd_f32(slice, &mut off);
                                chans[i].s_lvl       = rd_f32(slice, &mut off);
                                chans[i].r_ms        = rd_f32(slice, &mut off);

                                chans[i].arp_a       = rd_i32(slice, &mut off);
                                chans[i].arp_b       = rd_i32(slice, &mut off);
                                chans[i].arp_c       = rd_i32(slice, &mut off);
                                chans[i].arp_rate_hz = rd_f32(slice, &mut off);
                            }
                            eng.set_params(&chans);
                        }
                    }
                }

                // overlay
                if fps_timer.elapsed().as_secs_f32() >= 1.0 {
                    let fps = frames as f32 / fps_timer.elapsed().as_secs_f32();
                    let avg_ms = if frames > 0 { ms_accum / frames as f32 } else { 0.0 };
                    window.set_title(&format!(
                        "OxidoBoy — {:>4.0} FPS ({:.2} ms)  |  reloads: {}",
                        fps, avg_ms, reload_count
                    ));
                    fps_timer = Instant::now();
                    frames = 0;
                    ms_accum = 0.0;
                }

                window.request_redraw();
                next_frame = Instant::now() + FRAME_TIME;
                *control_flow = ControlFlow::WaitUntil(next_frame);
            }

            Event::RedrawRequested(_) => { let _ = pixels.render(); }
            _ => {}
        }
    });

    #[allow(unreachable_code)]
    Ok(())
}
