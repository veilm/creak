use anyhow::{anyhow, Context, Result};
use cairo::{Context as CairoContext, Format, ImageSurface};
use memfd::MemfdOptions;
use memmap2::MmapMut;
use pangocairo::functions as pangocairo;
use shell_words;
use std::env;
use std::fs;
use std::os::unix::io::AsFd;
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry::WlRegistry, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface};
use wayland_client::{globals::{registry_queue_init, GlobalListContents}, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

#[derive(Clone, Copy, Debug, Default)]
struct Margins {
    top: i32,
    right: i32,
    bottom: i32,
    left: i32,
}

#[derive(Clone, Copy, Debug)]
enum Position {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
    Default,
}

#[derive(Debug)]
struct Config {
    font: String,
    width: i32,
    padding: i32,
    border_size: i32,
    border_radius: i32,
    timeout_ms: u64,
    background: [f64; 4],
    text: [f64; 4],
    border: [f64; 4],
    edge: i32,
    default_offset: i32,
}

#[derive(Debug)]
struct Args {
    position: Position,
    message: String,
}

struct State {
    configured: bool,
    closed: bool,
    width: i32,
    height: i32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            configured: false,
            closed: false,
            width: 0,
            height: 0,
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                proxy.ack_configure(serial);
                state.configured = true;
                if width > 0 {
                    state.width = width as i32;
                }
                if height > 0 {
                    state.height = height as i32;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.closed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: wayland_client::protocol::wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: wayland_client::protocol::wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: wayland_client::protocol::wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: wayland_client::protocol::wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegion, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: wayland_client::protocol::wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn main() -> Result<()> {
    let (args, cfg) = parse_args()?;

    let (position, margins) = position_to_anchor(&cfg, args.position);

    let (width, height) = measure_text(&cfg, &args.message)?;
    let width = cfg.width.max(width);
    let height = height.max(cfg.padding * 2 + cfg.border_size * 2 + 1);

    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, mut event_queue) = registry_queue_init(&conn).context("init registry")?;
    let qh = event_queue.handle();

    let compositor: WlCompositor = globals.bind(&qh, 4..=5, ()).context("bind wl_compositor")?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).context("bind wl_shm")?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .context("bind zwlr_layer_shell_v1")?;

    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "creak".to_string(),
        &qh,
        (),
    );

    layer_surface.set_anchor(position);
    layer_surface.set_margin(margins.top, margins.right, margins.bottom, margins.left);
    layer_surface.set_size(width as u32, height as u32);
    layer_surface.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    layer_surface.set_exclusive_zone(0);

    let region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&region));

    surface.commit();
    conn.flush()?;

    let mut state = State {
        configured: false,
        closed: false,
        width,
        height,
    };

    event_queue.roundtrip(&mut state)?;
    if state.width <= 0 || state.height <= 0 {
        state.width = width;
        state.height = height;
    }

    let mut buffer = create_buffer(&shm, &qh, state.width, state.height)?;
    draw_notification(&mut buffer, state.width, state.height, &cfg, &args.message)?;

    surface.attach(Some(&buffer.wl_buffer), 0, 0);
    surface.damage_buffer(0, 0, state.width, state.height);
    surface.commit();
    conn.flush()?;

    let deadline = Instant::now() + Duration::from_millis(cfg.timeout_ms);
    while Instant::now() < deadline && !state.closed {
        event_queue.dispatch_pending(&mut state)?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

fn parse_args() -> Result<(Args, Config)> {
    let mut cfg = default_config();
    let mut tokens = load_config_args()?;
    tokens.extend(env::args().skip(1));
    if env::var("CREAK_DEBUG").is_ok() {
        eprintln!("creak tokens: {:?}", tokens);
    }

    let mut position = Position::Default;
    let mut rest: Vec<String> = Vec::new();
    let mut iter = tokens.into_iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--top-left" {
            position = Position::TopLeft;
        } else if arg == "--top" || arg == "--top-center" {
            position = Position::Top;
        } else if arg == "--top-right" {
            position = Position::TopRight;
        } else if arg == "--left" {
            position = Position::Left;
        } else if arg == "--center" {
            position = Position::Center;
        } else if arg == "--right" {
            position = Position::Right;
        } else if arg == "--bottom-left" {
            position = Position::BottomLeft;
        } else if arg == "--bottom" || arg == "--bottom-center" {
            position = Position::Bottom;
        } else if arg == "--bottom-right" {
            position = Position::BottomRight;
        } else if arg == "--timeout" {
            let val = next_value("--timeout", &mut iter)?;
            cfg.timeout_ms = val.parse()?;
        } else if arg.starts_with("--timeout=") {
            let val = arg.trim_start_matches("--timeout=");
            cfg.timeout_ms = val.parse()?;
        } else if arg == "--width" {
            let val = next_value("--width", &mut iter)?;
            cfg.width = val.parse()?;
        } else if arg.starts_with("--width=") {
            let val = arg.trim_start_matches("--width=");
            cfg.width = val.parse()?;
        } else if arg == "--font" {
            cfg.font = next_value("--font", &mut iter)?;
        } else if arg.starts_with("--font=") {
            cfg.font = arg.trim_start_matches("--font=").to_string();
        } else if arg == "--padding" {
            let val = next_value("--padding", &mut iter)?;
            cfg.padding = val.parse()?;
        } else if arg.starts_with("--padding=") {
            cfg.padding = arg.trim_start_matches("--padding=").parse()?;
        } else if arg == "--border-size" {
            let val = next_value("--border-size", &mut iter)?;
            cfg.border_size = val.parse()?;
        } else if arg.starts_with("--border-size=") {
            cfg.border_size = arg.trim_start_matches("--border-size=").parse()?;
        } else if arg == "--border-radius" {
            let val = next_value("--border-radius", &mut iter)?;
            cfg.border_radius = val.parse()?;
        } else if arg.starts_with("--border-radius=") {
            cfg.border_radius = arg.trim_start_matches("--border-radius=").parse()?;
        } else if arg == "--background" {
            let val = next_value("--background", &mut iter)?;
            cfg.background = parse_hex_color(&val).ok_or_else(|| anyhow!("invalid color for --background"))?;
        } else if arg.starts_with("--background=") {
            let val = arg.trim_start_matches("--background=");
            cfg.background = parse_hex_color(val).ok_or_else(|| anyhow!("invalid color for --background"))?;
        } else if arg == "--text" {
            let val = next_value("--text", &mut iter)?;
            cfg.text = parse_hex_color(&val).ok_or_else(|| anyhow!("invalid color for --text"))?;
        } else if arg.starts_with("--text=") {
            let val = arg.trim_start_matches("--text=");
            cfg.text = parse_hex_color(val).ok_or_else(|| anyhow!("invalid color for --text"))?;
        } else if arg == "--border" {
            let val = next_value("--border", &mut iter)?;
            cfg.border = parse_hex_color(&val).ok_or_else(|| anyhow!("invalid color for --border"))?;
        } else if arg.starts_with("--border=") {
            let val = arg.trim_start_matches("--border=");
            cfg.border = parse_hex_color(val).ok_or_else(|| anyhow!("invalid color for --border"))?;
        } else if arg == "--edge" {
            let val = next_value("--edge", &mut iter)?;
            cfg.edge = val.parse()?;
        } else if arg.starts_with("--edge=") {
            cfg.edge = arg.trim_start_matches("--edge=").parse()?;
        } else if arg == "--default-offset" {
            let val = next_value("--default-offset", &mut iter)?;
            cfg.default_offset = val.parse()?;
        } else if arg.starts_with("--default-offset=") {
            cfg.default_offset = arg.trim_start_matches("--default-offset=").parse()?;
        } else if arg == "--help" || arg == "-h" {
            return Err(anyhow!("usage: creak [--top-left|--top|--top-right|--left|--center|--right|--bottom-left|--bottom|--bottom-right] [--timeout ms] [--width px] [--font font] [--padding px] [--border-size px] [--border-radius px] [--background #RRGGBB[AA]] [--text #RRGGBB[AA]] [--border #RRGGBB[AA]] [--edge px] [--default-offset px] <title> [body...]"));
        } else if arg.starts_with('-') {
            return Err(anyhow!("unknown option: {}", arg));
        } else {
            rest.push(arg);
        }
    }

    if rest.is_empty() {
        return Err(anyhow!("missing message"));
    }

    let message = if rest.len() == 1 {
        rest[0].clone()
    } else {
        let title = &rest[0];
        let body = rest[1..].join(" ");
        format!("{}\n{}", title, body)
    };

    if env::var("CREAK_DEBUG").is_ok() {
        eprintln!("creak config: {:?}", cfg);
    }
    Ok((Args { position, message }, cfg))
}

fn load_config_args() -> Result<Vec<String>> {
    let xdg_config = env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", env::var("HOME").unwrap_or_default()));
    let path = format!("{}/creak/config", xdg_config);
    if env::var("CREAK_DEBUG").is_ok() {
        eprintln!("creak config path: {}", path);
    }
    let contents = match fs::read_to_string(&path) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };

    let mut args = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts = shell_words::split(line).context("parse config line")?;
        args.extend(parts);
    }
    Ok(args)
}

fn next_value(name: &str, iter: &mut std::iter::Peekable<std::vec::IntoIter<String>>) -> Result<String> {
    iter.next().ok_or_else(|| anyhow!("{} requires a value", name))
}

fn default_config() -> Config {
    Config {
        font: "SimSun 25".to_string(),
        width: 350,
        padding: 10,
        border_size: 5,
        border_radius: 10,
        timeout_ms: 5000,
        background: [0.1, 0.1, 0.1, 1.0],
        text: [1.0, 1.0, 1.0, 1.0],
        border: [1.0, 1.0, 1.0, 1.0],
        edge: 20,
        default_offset: 250,
    }
}

fn parse_hex_color(value: &str) -> Option<[f64; 4]> {
    let hex = value.trim_start_matches('#');
    let (r, g, b, a) = match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            (r, g, b, 255)
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            (r, g, b, a)
        }
        _ => return None,
    };

    Some([
        r as f64 / 255.0,
        g as f64 / 255.0,
        b as f64 / 255.0,
        a as f64 / 255.0,
    ])
}

fn position_to_anchor(cfg: &Config, position: Position) -> (zwlr_layer_surface_v1::Anchor, Margins) {
    let edge = cfg.edge;
    let default_offset = cfg.default_offset;

    match position {
        Position::TopLeft => (
            zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left,
            Margins {
                top: edge,
                left: edge,
                ..Margins::default()
            },
        ),
        Position::Top => (
            zwlr_layer_surface_v1::Anchor::Top,
            Margins {
                top: edge,
                ..Margins::default()
            },
        ),
        Position::TopRight => (
            zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Right,
            Margins {
                top: edge,
                right: edge,
                ..Margins::default()
            },
        ),
        Position::Left => (
            zwlr_layer_surface_v1::Anchor::Left,
            Margins {
                left: edge,
                ..Margins::default()
            },
        ),
        Position::Center => (
            zwlr_layer_surface_v1::Anchor::empty(),
            Margins::default(),
        ),
        Position::Right => (
            zwlr_layer_surface_v1::Anchor::Right,
            Margins {
                right: edge,
                ..Margins::default()
            },
        ),
        Position::BottomLeft => (
            zwlr_layer_surface_v1::Anchor::Bottom | zwlr_layer_surface_v1::Anchor::Left,
            Margins {
                bottom: edge,
                left: edge,
                ..Margins::default()
            },
        ),
        Position::Bottom => (
            zwlr_layer_surface_v1::Anchor::Bottom,
            Margins {
                bottom: edge,
                ..Margins::default()
            },
        ),
        Position::BottomRight => (
            zwlr_layer_surface_v1::Anchor::Bottom | zwlr_layer_surface_v1::Anchor::Right,
            Margins {
                bottom: edge,
                right: edge,
                ..Margins::default()
            },
        ),
        Position::Default => (
            zwlr_layer_surface_v1::Anchor::Top,
            Margins {
                top: default_offset,
                ..Margins::default()
            },
        ),
    }
}

fn measure_text(cfg: &Config, text: &str) -> Result<(i32, i32)> {
    let surface = ImageSurface::create(Format::ARgb32, cfg.width.max(1), 1)?;
    let cr = CairoContext::new(&surface)?;
    let layout = pangocairo::create_layout(&cr);
    layout.set_text(text);

    let font_desc = pango::FontDescription::from_string(&cfg.font);
    layout.set_font_description(Some(&font_desc));
    layout.set_width(cfg.width * pango::SCALE);
    layout.set_alignment(pango::Alignment::Center);
    layout.set_wrap(pango::WrapMode::WordChar);

    let (text_width, text_height) = layout.pixel_size();
    let height = text_height + cfg.padding * 2 + cfg.border_size * 2;
    Ok((text_width, height))
}

struct Buffer {
    _mmap: MmapMut,
    wl_buffer: wayland_client::protocol::wl_buffer::WlBuffer,
    stride: i32,
}

fn create_buffer(shm: &WlShm, qh: &QueueHandle<State>, width: i32, height: i32) -> Result<Buffer> {
    let stride = width * 4;
    let size = stride * height;

    let memfd = MemfdOptions::default().create("creak")?;
    memfd.as_file().set_len(size as u64)?;

    let mmap = unsafe { MmapMut::map_mut(memfd.as_file())? };

    let pool = shm.create_pool(memfd.as_file().as_fd(), size, qh, ());
    let wl_buffer = pool.create_buffer(
        0,
        width,
        height,
        stride,
        wayland_client::protocol::wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();

    Ok(Buffer {
        _mmap: mmap,
        wl_buffer,
        stride,
    })
}

fn draw_notification(buffer: &mut Buffer, width: i32, height: i32, cfg: &Config, text: &str) -> Result<()> {
    let data = buffer._mmap.as_mut();
    for b in data.iter_mut() {
        *b = 0;
    }

    let surface = unsafe {
        ImageSurface::create_for_data_unsafe(
            data.as_mut_ptr(),
            Format::ARgb32,
            width,
            height,
            buffer.stride,
        )?
    };

    let cr = CairoContext::new(&surface)?;

    let radius = cfg.border_radius as f64;
    let border = cfg.border_size as f64;

    let x = border / 2.0;
    let y = border / 2.0;
    let w = width as f64 - border;
    let h = height as f64 - border;

    rounded_rect(&cr, x, y, w, h, radius);
    cr.set_source_rgba(cfg.background[0], cfg.background[1], cfg.background[2], cfg.background[3]);
    cr.fill_preserve()?;

    if cfg.border_size > 0 {
        cr.set_line_width(border);
        cr.set_source_rgba(cfg.border[0], cfg.border[1], cfg.border[2], cfg.border[3]);
        cr.stroke()?;
    } else {
        cr.new_path();
    }

    let layout = pangocairo::create_layout(&cr);
    layout.set_text(text);
    let font_desc = pango::FontDescription::from_string(&cfg.font);
    layout.set_font_description(Some(&font_desc));
    layout.set_width((width - 2 * (cfg.padding + cfg.border_size)) * pango::SCALE);
    layout.set_alignment(pango::Alignment::Center);
    layout.set_wrap(pango::WrapMode::WordChar);

    cr.set_source_rgba(cfg.text[0], cfg.text[1], cfg.text[2], cfg.text[3]);
    cr.move_to(
        (cfg.padding + cfg.border_size) as f64,
        (cfg.padding + cfg.border_size) as f64,
    );
    pangocairo::show_layout(&cr, &layout);

    surface.flush();
    if env::var("CREAK_DEBUG").is_ok() {
        if data.len() >= 4 {
            eprintln!(
                "creak pixel0 argb bytes: {:02x} {:02x} {:02x} {:02x}",
                data[0], data[1], data[2], data[3]
            );
        }
        let px = 10i32;
        let py = 10i32;
        let offset = (py * buffer.stride + px * 4) as usize;
        if data.len() >= offset + 4 {
            eprintln!(
                "creak pixel10,10 argb bytes: {:02x} {:02x} {:02x} {:02x}",
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3]
            );
        }
    }
    Ok(())
}

fn rounded_rect(cr: &CairoContext, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -90.0_f64.to_radians(), 0.0_f64.to_radians());
    cr.arc(x + w - r, y + h - r, r, 0.0_f64.to_radians(), 90.0_f64.to_radians());
    cr.arc(x + r, y + h - r, r, 90.0_f64.to_radians(), 180.0_f64.to_radians());
    cr.arc(x + r, y + r, r, 180.0_f64.to_radians(), 270.0_f64.to_radians());
    cr.close_path();
}
