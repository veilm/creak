use anyhow::{anyhow, Context, Result};
use cairo::{
    Antialias, Context as CairoContext, FontOptions, Format, HintMetrics, HintStyle, ImageSurface,
};
use memfd::MemfdOptions;
use memmap2::MmapMut;
use pangocairo::functions as pangocairo;
use serde::{Deserialize, Serialize};
use shell_words;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::os::unix::io::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_output::WlOutput, wl_pointer::WlPointer,
    wl_region::WlRegion, wl_registry::WlRegistry, wl_seat::WlSeat, wl_shm::WlShm,
    wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{
    backend::WaylandError,
    globals::{registry_queue_init, GlobalListContents},
    Connection, Dispatch, Proxy, QueueHandle,
};
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
    stack_gap: i32,
    stack: bool,
    output_scale: i32,
    text_antialias: Option<Antialias>,
    text_hint: Option<HintStyle>,
    text_hint_metrics: Option<HintMetrics>,
}

#[derive(Debug)]
struct AlertArgs {
    position: Position,
    message: String,
    name: Option<String>,
    class: Option<String>,
}

#[derive(Debug)]
enum Command {
    Help,
    Show(AlertArgs),
    ListActive,
    ClearByName(String),
    ClearByClass(String),
    ClearById(u64),
}

#[derive(Debug)]
struct Args {
    command: Command,
    state_dir: Option<String>,
}

#[derive(Clone, Debug)]
struct StatePaths {
    state_path: String,
    lock_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StackEntry {
    id: u64,
    position: String,
    height: i32,
    gap: i32,
    expires_at: u64,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    summary: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StackState {
    next_id: u64,
    entries: Vec<StackEntry>,
}

impl Default for StackState {
    fn default() -> Self {
        Self {
            next_id: 1,
            entries: Vec::new(),
        }
    }
}

struct StackGuard {
    id: u64,
    position: String,
    state_path: String,
    lock_path: String,
}

static SHOULD_CLOSE: AtomicBool = AtomicBool::new(false);
const HELP_TEXT: &str = r#"creak

Usage:
  creak list active [--style <name|path>] [--state-dir <path>]
  creak clear by name <name> [--style <name|path>] [--state-dir <path>]
  creak clear by class <class> [--style <name|path>] [--state-dir <path>]
  creak clear by id <id> [--style <name|path>] [--state-dir <path>]
  creak [--style <name|path>] [--state-dir <path>] [--name <name>] [--class <class>] [options] <title> [body...]

Alert options:
  --top-left | --top | --top-right
  --left | --center | --right
  --bottom-left | --bottom | --bottom-right
  --timeout <ms>
  --width <px>
  --font <font>
  --padding <px>
  --border-size <px>
  --border-radius <px>
  --background <#RRGGBB[AA]>
  --text <#RRGGBB[AA]>
  --border <#RRGGBB[AA]>
  --edge <px>
  --default-offset <px>
  --stack-gap <px>
  --stack | --no-stack
  --scale <n>
  --text-antialias default|none|gray|subpixel
  --text-hint default|none|slight|medium|full
  --text-hint-metrics default|on|off

Control commands:
  list active                Print active alerts as JSON
  clear by name <name>       SIGTERM + remove matching alerts
  clear by class <class>     SIGTERM + remove matching alerts
  clear by id <id>           SIGTERM + remove matching alert

Common:
  --style <name|path>        Config file: name in $XDG_CONFIG_HOME/creak or file path
  --state-dir <path>         Use a custom state directory
  --help, -h                 Show this help
"#;

impl Drop for StackGuard {
    fn drop(&mut self) {
        if let Ok(_lock) = lock_state(&self.lock_path) {
            if let Ok(mut state) = load_state(&self.state_path) {
                state.entries.retain(|entry| entry.id != self.id);
                let _ = save_state(&self.state_path, &state);
            }
        }
    }
}

struct State {
    configured: bool,
    closed: bool,
    width: i32,
    height: i32,
    scale: i32,
    outputs: HashMap<u32, i32>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            configured: false,
            closed: false,
            width: 0,
            height: 0,
            scale: 1,
            outputs: HashMap::new(),
            seat: None,
            pointer: None,
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
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
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
        state: &mut Self,
        _: &WlSurface,
        event: wayland_client::protocol::wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_surface::Event::Enter { output } = event {
            let id = output.id().protocol_id();
            if let Some(scale) = state.outputs.get(&id) {
                state.scale = (*scale).max(1);
            }
        }
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

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_seat::Event::Capabilities { capabilities } = event {
            if let wayland_client::WEnum::Value(caps) = capabilities {
                if env::var("CREAK_DEBUG").is_ok() {
                    eprintln!("creak seat capabilities: {:?}", caps);
                }
                if caps.contains(wayland_client::protocol::wl_seat::Capability::Pointer) {
                    if state.pointer.is_none() {
                        if env::var("CREAK_DEBUG").is_ok() {
                            eprintln!("creak creating pointer");
                        }
                        state.pointer = Some(seat.get_pointer(qh, ()));
                    }
                } else {
                    state.pointer = None;
                }
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wayland_client::protocol::wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wayland_client::protocol::wl_pointer::Event::Button {
                state: button_state,
                ..
            } => {
                if button_state
                    == wayland_client::WEnum::Value(
                        wayland_client::protocol::wl_pointer::ButtonState::Pressed,
                    )
                {
                    if env::var("CREAK_DEBUG").is_ok() {
                        eprintln!("creak pointer button pressed");
                    }
                    state.closed = true;
                }
            }
            wayland_client::protocol::wl_pointer::Event::Enter { .. } => {
                if env::var("CREAK_DEBUG").is_ok() {
                    eprintln!("creak pointer enter");
                }
            }
            _ => {}
        }
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

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wayland_client::protocol::wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_output::Event::Scale { factor } = event {
            let id = output.id().protocol_id();
            state.outputs.insert(id, factor);
            state.scale = factor.max(1);
        }
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
    let (args, mut cfg) = parse_args()?;
    if matches!(args.command, Command::Help) {
        println!("{}", HELP_TEXT);
        return Ok(());
    }
    let state_paths = state_paths(args.state_dir.as_deref())?;
    match args.command {
        Command::Help => return Ok(()),
        Command::ListActive => {
            let entries = list_active_entries(&state_paths)?;
            println!("{}", serde_json::to_string_pretty(&entries)?);
            return Ok(());
        }
        Command::ClearByName(name) => {
            let count = clear_active_entries(&state_paths, ClearSelector::Name(name))?;
            println!("{}", count);
            return Ok(());
        }
        Command::ClearByClass(class) => {
            let count = clear_active_entries(&state_paths, ClearSelector::Class(class))?;
            println!("{}", count);
            return Ok(());
        }
        Command::ClearById(id) => {
            let count = clear_active_entries(&state_paths, ClearSelector::Id(id))?;
            println!("{}", count);
            return Ok(());
        }
        Command::Show(alert) => {
            run_alert(alert, &mut cfg, &state_paths)?;
        }
    }
    Ok(())
}

fn run_alert(args: AlertArgs, cfg: &mut Config, state_paths: &StatePaths) -> Result<()> {
    install_signal_handlers();
    SHOULD_CLOSE.store(false, Ordering::Relaxed);

    let (width, height) = measure_text(cfg, &args.message)?;
    let width = cfg.width.max(width);
    let height = height.max(cfg.padding * 2 + cfg.border_size * 2 + 1);

    let mut state = State {
        configured: false,
        closed: false,
        width,
        height,
        scale: cfg.output_scale.max(1),
        outputs: HashMap::new(),
        seat: None,
        pointer: None,
    };

    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, mut event_queue) = registry_queue_init(&conn).context("init registry")?;
    let qh = event_queue.handle();

    let compositor: WlCompositor = globals.bind(&qh, 4..=5, ()).context("bind wl_compositor")?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).context("bind wl_shm")?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .context("bind zwlr_layer_shell_v1")?;
    state.seat = globals.bind(&qh, 1..=7, ()).ok();

    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "creak".to_string(),
        &qh,
        (),
    );

    event_queue.roundtrip(&mut state)?;
    if state.scale <= 0 {
        state.scale = 1;
    }

    let (position, base_margins) = position_to_anchor(cfg, args.position);
    let mut stack_offset = 0;
    let mut stack_guard: Option<StackGuard> = None;
    if cfg.stack && cfg.timeout_ms > 0 {
        if let Ok((offset, guard)) = reserve_stack_slot(
            state_paths,
            args.position,
            height,
            cfg.stack_gap,
            cfg.timeout_ms,
            args.name.clone(),
            args.class.clone(),
            message_summary(&args.message),
        ) {
            stack_offset = offset;
            stack_guard = Some(guard);
        }
    }

    let mut margins = apply_stack_offset(base_margins, args.position, stack_offset);

    layer_surface.set_anchor(position);
    layer_surface.set_margin(margins.top, margins.right, margins.bottom, margins.left);
    layer_surface.set_size(width as u32, height as u32);
    layer_surface.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    layer_surface.set_exclusive_zone(0);

    surface.commit();
    conn.flush()?;

    event_queue.roundtrip(&mut state)?;
    if state.width <= 0 || state.height <= 0 {
        state.width = width;
        state.height = height;
    }

    if cfg.output_scale <= 0 {
        cfg.output_scale = state.scale;
    }
    let scale = cfg.output_scale.max(1);
    let pixel_width = state.width * scale;
    let pixel_height = state.height * scale;
    state.scale = scale;
    surface.set_buffer_scale(state.scale);
    let region = compositor.create_region(&qh, ());
    region.add(0, 0, state.width, state.height);
    surface.set_input_region(Some(&region));

    let mut buffer = create_buffer(&shm, &qh, pixel_width, pixel_height)?;
    draw_notification(
        &mut buffer,
        pixel_width,
        pixel_height,
        state.width,
        state.height,
        cfg,
        &args.message,
    )?;

    surface.attach(Some(&buffer.wl_buffer), 0, 0);
    surface.damage_buffer(0, 0, pixel_width, pixel_height);
    surface.commit();
    conn.flush()?;

    let deadline = Instant::now() + Duration::from_millis(cfg.timeout_ms);
    let mut last_check = Instant::now();
    let mut last_offset = stack_offset;
    while Instant::now() < deadline && !state.closed && !SHOULD_CLOSE.load(Ordering::Relaxed) {
        dispatch_with_timeout(&mut event_queue, &mut state, 10)?;
        conn.flush()?;
        if let Some(guard) = stack_guard.as_ref() {
            if last_check.elapsed() >= Duration::from_millis(100) {
                if let Ok(offset) = stack_offset_for_id(guard) {
                    if offset != last_offset {
                        margins = apply_stack_offset(base_margins, args.position, offset);
                        layer_surface.set_margin(
                            margins.top,
                            margins.right,
                            margins.bottom,
                            margins.left,
                        );
                        surface.commit();
                        let _ = conn.flush();
                        last_offset = offset;
                    }
                }
                last_check = Instant::now();
            }
        }
    }

    drop(stack_guard);
    Ok(())
}

unsafe extern "C" fn handle_signal(_: i32) {
    SHOULD_CLOSE.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, handle_signal as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle_signal as libc::sighandler_t);
    }
}

fn parse_args() -> Result<(Args, Config)> {
    let cfg = default_config();
    let cli_tokens: Vec<String> = env::args().skip(1).collect();
    let (style, mut cli_tokens) = extract_style_arg(cli_tokens)?;
    let mut tokens = load_config_args(style.as_deref())?;
    tokens.append(&mut cli_tokens);
    if env::var("CREAK_DEBUG").is_ok() {
        eprintln!("creak tokens: {:?}", tokens);
    }
    parse_tokens(tokens, cfg)
}

fn extract_style_arg(tokens: Vec<String>) -> Result<(Option<String>, Vec<String>)> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut style: Option<String> = None;
    let mut i = 0usize;
    while i < tokens.len() {
        let arg = &tokens[i];
        if arg == "--style" {
            if i + 1 >= tokens.len() {
                return Err(anyhow!("--style requires a value"));
            }
            style = Some(tokens[i + 1].clone());
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--style=") {
            style = Some(value.to_string());
            i += 1;
            continue;
        }
        out.push(arg.clone());
        i += 1;
    }
    Ok((style, out))
}

fn parse_tokens(tokens: Vec<String>, mut cfg: Config) -> Result<(Args, Config)> {
    let mut position = Position::Default;
    let mut alert_name: Option<String> = None;
    let mut alert_class: Option<String> = None;
    let mut state_dir: Option<String> = None;
    let mut command: Option<Command> = None;
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
            cfg.background =
                parse_hex_color(&val).ok_or_else(|| anyhow!("invalid color for --background"))?;
        } else if arg.starts_with("--background=") {
            let val = arg.trim_start_matches("--background=");
            cfg.background =
                parse_hex_color(val).ok_or_else(|| anyhow!("invalid color for --background"))?;
        } else if arg == "--text" {
            let val = next_value("--text", &mut iter)?;
            cfg.text = parse_hex_color(&val).ok_or_else(|| anyhow!("invalid color for --text"))?;
        } else if arg.starts_with("--text=") {
            let val = arg.trim_start_matches("--text=");
            cfg.text = parse_hex_color(val).ok_or_else(|| anyhow!("invalid color for --text"))?;
        } else if arg == "--border" {
            let val = next_value("--border", &mut iter)?;
            cfg.border =
                parse_hex_color(&val).ok_or_else(|| anyhow!("invalid color for --border"))?;
        } else if arg.starts_with("--border=") {
            let val = arg.trim_start_matches("--border=");
            cfg.border =
                parse_hex_color(val).ok_or_else(|| anyhow!("invalid color for --border"))?;
        } else if arg == "--edge" {
            let val = next_value("--edge", &mut iter)?;
            cfg.edge = val.parse()?;
        } else if arg.starts_with("--edge=") {
            cfg.edge = arg.trim_start_matches("--edge=").parse()?;
        } else if arg == "--scale" {
            let val = next_value("--scale", &mut iter)?;
            cfg.output_scale = val.parse()?;
        } else if arg.starts_with("--scale=") {
            cfg.output_scale = arg.trim_start_matches("--scale=").parse()?;
        } else if arg == "--text-antialias" {
            let val = next_value("--text-antialias", &mut iter)?;
            cfg.text_antialias = parse_antialias(&val)?;
        } else if arg.starts_with("--text-antialias=") {
            let val = arg.trim_start_matches("--text-antialias=");
            cfg.text_antialias = parse_antialias(val)?;
        } else if arg == "--text-hint" {
            let val = next_value("--text-hint", &mut iter)?;
            cfg.text_hint = parse_hint_style(&val)?;
        } else if arg.starts_with("--text-hint=") {
            let val = arg.trim_start_matches("--text-hint=");
            cfg.text_hint = parse_hint_style(val)?;
        } else if arg == "--text-hint-metrics" {
            let val = next_value("--text-hint-metrics", &mut iter)?;
            cfg.text_hint_metrics = parse_hint_metrics(&val)?;
        } else if arg.starts_with("--text-hint-metrics=") {
            let val = arg.trim_start_matches("--text-hint-metrics=");
            cfg.text_hint_metrics = parse_hint_metrics(val)?;
        } else if arg == "--default-offset" {
            let val = next_value("--default-offset", &mut iter)?;
            cfg.default_offset = val.parse()?;
        } else if arg.starts_with("--default-offset=") {
            cfg.default_offset = arg.trim_start_matches("--default-offset=").parse()?;
        } else if arg == "--stack-gap" {
            let val = next_value("--stack-gap", &mut iter)?;
            cfg.stack_gap = val.parse()?;
        } else if arg.starts_with("--stack-gap=") {
            cfg.stack_gap = arg.trim_start_matches("--stack-gap=").parse()?;
        } else if arg == "--stack" {
            cfg.stack = true;
        } else if arg == "--no-stack" {
            cfg.stack = false;
        } else if arg == "--name" {
            alert_name = Some(next_value("--name", &mut iter)?);
        } else if arg.starts_with("--name=") {
            alert_name = Some(arg.trim_start_matches("--name=").to_string());
        } else if arg == "--class" {
            alert_class = Some(next_value("--class", &mut iter)?);
        } else if arg.starts_with("--class=") {
            alert_class = Some(arg.trim_start_matches("--class=").to_string());
        } else if arg == "--state-dir" {
            state_dir = Some(next_value("--state-dir", &mut iter)?);
        } else if arg.starts_with("--state-dir=") {
            state_dir = Some(arg.trim_start_matches("--state-dir=").to_string());
        } else if arg == "--list-active" {
            command = Some(Command::ListActive);
        } else if arg == "--clear-by-name" {
            let name = next_value("--clear-by-name", &mut iter)?;
            command = Some(Command::ClearByName(name));
        } else if arg.starts_with("--clear-by-name=") {
            command = Some(Command::ClearByName(
                arg.trim_start_matches("--clear-by-name=").to_string(),
            ));
        } else if arg == "--clear-by-class" {
            let class = next_value("--clear-by-class", &mut iter)?;
            command = Some(Command::ClearByClass(class));
        } else if arg.starts_with("--clear-by-class=") {
            command = Some(Command::ClearByClass(
                arg.trim_start_matches("--clear-by-class=").to_string(),
            ));
        } else if arg == "--clear-by-id" {
            let id = next_value("--clear-by-id", &mut iter)?;
            command = Some(Command::ClearById(id.parse()?));
        } else if arg.starts_with("--clear-by-id=") {
            let id = arg.trim_start_matches("--clear-by-id=");
            command = Some(Command::ClearById(id.parse()?));
        } else if arg == "list" {
            let sub = next_value("list", &mut iter)?;
            if sub != "active" {
                return Err(anyhow!("usage: creak list active"));
            }
            command = Some(Command::ListActive);
        } else if arg == "clear" {
            command = Some(parse_clear_command(&mut iter)?);
        } else if arg == "--help" || arg == "-h" {
            command = Some(Command::Help);
        } else if arg.starts_with('-') {
            return Err(anyhow!("unknown option: {}", arg));
        } else {
            rest.push(arg);
        }
    }

    let command = if let Some(command) = command {
        if !rest.is_empty() {
            return Err(anyhow!(
                "unexpected positional arguments for control command"
            ));
        }
        command
    } else {
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
        Command::Show(AlertArgs {
            position,
            message,
            name: alert_name,
            class: alert_class,
        })
    };

    if env::var("CREAK_DEBUG").is_ok() {
        eprintln!("creak config: {:?}", cfg);
    }
    Ok((Args { command, state_dir }, cfg))
}

fn parse_clear_command(
    iter: &mut std::iter::Peekable<std::vec::IntoIter<String>>,
) -> Result<Command> {
    let by = next_value("clear", iter)?;
    if by != "by" {
        return Err(anyhow!("usage: creak clear by <name|class|id> <value>"));
    }
    let key = next_value("clear by", iter)?;
    let value = next_value("clear by <key>", iter)?;
    match key.as_str() {
        "name" => Ok(Command::ClearByName(value)),
        "class" => Ok(Command::ClearByClass(value)),
        "id" => Ok(Command::ClearById(value.parse()?)),
        _ => Err(anyhow!("usage: creak clear by <name|class|id> <value>")),
    }
}

fn dispatch_with_timeout(
    event_queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    timeout_ms: i32,
) -> Result<()> {
    if let Some(guard) = event_queue.prepare_read() {
        let fd = guard.connection_fd().as_raw_fd();
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pollfd as *mut libc::pollfd, 1, timeout_ms) };
        if rc > 0 && (pollfd.revents & libc::POLLIN) != 0 {
            if let Err(err) = guard.read() {
                match err {
                    WaylandError::Io(io_err) if io_err.kind() == ErrorKind::WouldBlock => {}
                    other => return Err(anyhow!("wayland read error: {:?}", other)),
                }
            }
        }
    }
    event_queue.dispatch_pending(state)?;
    Ok(())
}

fn load_config_args(style: Option<&str>) -> Result<Vec<String>> {
    let xdg_config = env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| format!("{}/.config", env::var("HOME").unwrap_or_default()));
    let path = config_path_for_style(&xdg_config, style);
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

fn config_path_for_style(xdg_config_home: &str, style: Option<&str>) -> String {
    let default_dir = format!("{}/creak", xdg_config_home);
    match style {
        Some(value) if value.contains('/') => value.to_string(),
        Some(value) => format!("{}/{}", default_dir, value),
        None => format!("{}/config", default_dir),
    }
}

fn next_value(
    name: &str,
    iter: &mut std::iter::Peekable<std::vec::IntoIter<String>>,
) -> Result<String> {
    iter.next()
        .ok_or_else(|| anyhow!("{} requires a value", name))
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
        stack_gap: 10,
        stack: true,
        output_scale: 0,
        text_antialias: None,
        text_hint: None,
        text_hint_metrics: None,
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

fn parse_antialias(value: &str) -> Result<Option<Antialias>> {
    match value {
        "default" => Ok(None),
        "none" => Ok(Some(Antialias::None)),
        "gray" => Ok(Some(Antialias::Gray)),
        "subpixel" => Ok(Some(Antialias::Subpixel)),
        _ => Err(anyhow!("invalid --text-antialias: {}", value)),
    }
}

fn parse_hint_style(value: &str) -> Result<Option<HintStyle>> {
    match value {
        "default" => Ok(None),
        "none" => Ok(Some(HintStyle::None)),
        "slight" => Ok(Some(HintStyle::Slight)),
        "medium" => Ok(Some(HintStyle::Medium)),
        "full" => Ok(Some(HintStyle::Full)),
        _ => Err(anyhow!("invalid --text-hint: {}", value)),
    }
}

fn parse_hint_metrics(value: &str) -> Result<Option<HintMetrics>> {
    match value {
        "default" => Ok(None),
        "on" => Ok(Some(HintMetrics::On)),
        "off" => Ok(Some(HintMetrics::Off)),
        _ => Err(anyhow!("invalid --text-hint-metrics: {}", value)),
    }
}

fn position_to_anchor(
    cfg: &Config,
    position: Position,
) -> (zwlr_layer_surface_v1::Anchor, Margins) {
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
        Position::Center => (zwlr_layer_surface_v1::Anchor::empty(), Margins::default()),
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

fn position_key(position: Position) -> &'static str {
    match position {
        Position::TopLeft => "top-left",
        Position::Top => "top",
        Position::TopRight => "top-right",
        Position::Left => "left",
        Position::Center => "center",
        Position::Right => "right",
        Position::BottomLeft => "bottom-left",
        Position::Bottom => "bottom",
        Position::BottomRight => "bottom-right",
        Position::Default => "default",
    }
}

fn apply_stack_offset(mut margins: Margins, position: Position, offset: i32) -> Margins {
    match position {
        Position::Bottom | Position::BottomLeft | Position::BottomRight => {
            margins.bottom += offset;
        }
        _ => {
            margins.top += offset;
        }
    }
    margins
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

fn state_paths(state_dir: Option<&str>) -> Result<StatePaths> {
    let dir = match state_dir {
        Some(dir) => dir.to_string(),
        None => {
            let xdg_state = env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
                format!("{}/.local/state", env::var("HOME").unwrap_or_default())
            });
            format!("{}/creak", xdg_state)
        }
    };
    fs::create_dir_all(&dir)?;
    Ok(StatePaths {
        state_path: format!("{}/stack.json", dir),
        lock_path: format!("{}/stack.lock", dir),
    })
}

fn lock_state(lock_path: &str) -> Result<fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(anyhow!("failed to lock stack state"));
    }
    Ok(file)
}

fn load_state(path: &str) -> Result<StackState> {
    match fs::read_to_string(path) {
        Ok(data) => {
            if data.trim().is_empty() {
                return Ok(StackState::default());
            }
            match serde_json::from_str(&data) {
                Ok(state) => Ok(state),
                Err(err) => {
                    if env::var("CREAK_DEBUG").is_ok() {
                        eprintln!("creak stack state parse failed: {}", err);
                    }
                    Ok(StackState::default())
                }
            }
        }
        Err(_) => Ok(StackState::default()),
    }
}

fn save_state(path: &str, state: &StackState) -> Result<()> {
    let tmp = format!("{}.tmp", path);
    let data = serde_json::to_vec(state)?;
    fs::write(&tmp, data)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn message_summary(message: &str) -> String {
    let mut summary = message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if summary.len() > 120 {
        summary.truncate(120);
    }
    summary
}

fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    let code = std::io::Error::last_os_error().raw_os_error();
    code == Some(libc::EPERM)
}

fn prune_entries(state: &mut StackState, now: u64) {
    state.entries.retain(|entry| {
        let not_expired = entry.expires_at == 0 || entry.expires_at > now;
        not_expired && process_alive(entry.pid)
    });
}

fn list_active_entries(paths: &StatePaths) -> Result<Vec<StackEntry>> {
    let _lock = lock_state(&paths.lock_path)?;
    let mut state = load_state(&paths.state_path)?;
    let now = now_millis();
    let before = state.entries.len();
    prune_entries(&mut state, now);
    if state.entries.len() != before {
        save_state(&paths.state_path, &state)?;
    }
    Ok(state.entries)
}

enum ClearSelector {
    Id(u64),
    Name(String),
    Class(String),
}

fn clear_matches(entry: &StackEntry, selector: &ClearSelector) -> bool {
    match selector {
        ClearSelector::Id(id) => entry.id == *id,
        ClearSelector::Name(name) => entry.name.as_deref() == Some(name.as_str()),
        ClearSelector::Class(class) => entry.class.as_deref() == Some(class.as_str()),
    }
}

fn send_sigterm(pid: u32) -> Result<()> {
    if pid == 0 {
        return Ok(());
    }
    let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if rc == 0 {
        return Ok(());
    }
    let code = std::io::Error::last_os_error().raw_os_error();
    if code == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(anyhow!("failed to SIGTERM pid {}: {:?}", pid, code))
}

fn clear_active_entries(paths: &StatePaths, selector: ClearSelector) -> Result<usize> {
    let _lock = lock_state(&paths.lock_path)?;
    let mut state = load_state(&paths.state_path)?;
    let now = now_millis();
    prune_entries(&mut state, now);

    let mut removed = 0usize;
    let mut keep = Vec::with_capacity(state.entries.len());
    for entry in state.entries.into_iter() {
        if clear_matches(&entry, &selector) {
            send_sigterm(entry.pid)?;
            removed += 1;
            continue;
        }
        keep.push(entry);
    }
    state.entries = keep;
    save_state(&paths.state_path, &state)?;
    Ok(removed)
}

fn reserve_stack_slot(
    paths: &StatePaths,
    position: Position,
    height: i32,
    gap: i32,
    timeout_ms: u64,
    name: Option<String>,
    class: Option<String>,
    summary: String,
) -> Result<(i32, StackGuard)> {
    let _lock = lock_state(&paths.lock_path)?;
    let mut state = load_state(&paths.state_path)?;
    let now = now_millis();
    prune_entries(&mut state, now);

    let key = position_key(position);
    let mut offset = 0;
    for entry in state.entries.iter().filter(|entry| entry.position == key) {
        offset += entry.height + entry.gap;
    }

    let id = state.next_id;
    state.next_id += 1;
    let expires_at = now.saturating_add(timeout_ms);
    state.entries.push(StackEntry {
        id,
        position: key.to_string(),
        height,
        gap,
        expires_at,
        created_at: now,
        pid: std::process::id(),
        name,
        class,
        summary,
    });
    save_state(&paths.state_path, &state)?;

    Ok((
        offset,
        StackGuard {
            id,
            position: key.to_string(),
            state_path: paths.state_path.clone(),
            lock_path: paths.lock_path.clone(),
        },
    ))
}

fn stack_offset_for_id(guard: &StackGuard) -> Result<i32> {
    let _lock = lock_state(&guard.lock_path)?;
    let state = load_state(&guard.state_path)?;
    let mut offset = 0;
    for entry in state.entries.iter() {
        if entry.position != guard.position {
            continue;
        }
        if entry.id == guard.id {
            break;
        }
        offset += entry.height + entry.gap;
    }
    Ok(offset)
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

fn draw_notification(
    buffer: &mut Buffer,
    pixel_width: i32,
    pixel_height: i32,
    logical_width: i32,
    logical_height: i32,
    cfg: &Config,
    text: &str,
) -> Result<()> {
    let data = buffer._mmap.as_mut();
    for b in data.iter_mut() {
        *b = 0;
    }

    let surface = unsafe {
        ImageSurface::create_for_data_unsafe(
            data.as_mut_ptr(),
            Format::ARgb32,
            pixel_width,
            pixel_height,
            buffer.stride,
        )?
    };

    let cr = CairoContext::new(&surface)?;
    let scale = cfg.output_scale.max(1) as f64;
    cr.scale(scale, scale);

    let radius = cfg.border_radius as f64;
    let border = cfg.border_size as f64;

    let x = border / 2.0;
    let y = border / 2.0;
    let w = logical_width as f64 - border;
    let h = logical_height as f64 - border;

    rounded_rect(&cr, x, y, w, h, radius);
    cr.set_source_rgba(
        cfg.background[0],
        cfg.background[1],
        cfg.background[2],
        cfg.background[3],
    );
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
    layout.set_width((logical_width - 2 * (cfg.padding + cfg.border_size)) * pango::SCALE);
    layout.set_alignment(pango::Alignment::Center);
    layout.set_wrap(pango::WrapMode::WordChar);

    if cfg.text_antialias.is_some() || cfg.text_hint.is_some() || cfg.text_hint_metrics.is_some() {
        if let Ok(mut opts) = FontOptions::new() {
            if let Some(aa) = cfg.text_antialias {
                opts.set_antialias(aa);
            }
            if let Some(hint) = cfg.text_hint {
                opts.set_hint_style(hint);
            }
            if let Some(metrics) = cfg.text_hint_metrics {
                opts.set_hint_metrics(metrics);
            }
            cr.set_font_options(&opts);
            let context = layout.context();
            pangocairo::context_set_font_options(&context, Some(&opts));
        }
    }

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
    cr.arc(
        x + w - r,
        y + r,
        r,
        -90.0_f64.to_radians(),
        0.0_f64.to_radians(),
    );
    cr.arc(
        x + w - r,
        y + h - r,
        r,
        0.0_f64.to_radians(),
        90.0_f64.to_radians(),
    );
    cr.arc(
        x + r,
        y + h - r,
        r,
        90.0_f64.to_radians(),
        180.0_f64.to_radians(),
    );
    cr.arc(
        x + r,
        y + r,
        r,
        180.0_f64.to_radians(),
        270.0_f64.to_radians(),
    );
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn make_temp_state_dir() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let seq = TEST_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let uniq = format!("creak-test-{}-{}-{}", std::process::id(), nanos, seq);
        let dir = env::temp_dir().join(uniq);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir.to_string_lossy().into_owned()
    }

    fn test_paths() -> StatePaths {
        let dir = make_temp_state_dir();
        state_paths(Some(&dir)).expect("state paths")
    }

    #[test]
    fn parse_list_active_command() {
        let tokens = vec![
            "list".to_string(),
            "active".to_string(),
            "--state-dir".to_string(),
            "/tmp/creak-test".to_string(),
        ];
        let (args, _) = parse_tokens(tokens, default_config()).expect("parse tokens");
        match args.command {
            Command::ListActive => {}
            _ => panic!("expected list active command"),
        }
        assert_eq!(args.state_dir.as_deref(), Some("/tmp/creak-test"));
    }

    #[test]
    fn extract_style_arg_splits_cli_tokens() {
        let tokens = vec![
            "--style".to_string(),
            "hi".to_string(),
            "--timeout".to_string(),
            "10".to_string(),
            "hello".to_string(),
        ];
        let (style, rest) = extract_style_arg(tokens).expect("extract style");
        assert_eq!(style.as_deref(), Some("hi"));
        assert_eq!(rest, vec!["--timeout", "10", "hello"]);
    }

    #[test]
    fn config_path_for_style_resolves_name_and_path() {
        let xdg = "/tmp/xdg";
        assert_eq!(
            config_path_for_style(xdg, None),
            "/tmp/xdg/creak/config".to_string()
        );
        assert_eq!(
            config_path_for_style(xdg, Some("hi")),
            "/tmp/xdg/creak/hi".to_string()
        );
        assert_eq!(
            config_path_for_style(xdg, Some("/tmp/custom-style")),
            "/tmp/custom-style".to_string()
        );
    }

    #[test]
    fn clear_by_name_removes_matching_entries() {
        let paths = test_paths();
        let state = StackState {
            next_id: 3,
            entries: vec![
                StackEntry {
                    id: 1,
                    position: "top".to_string(),
                    height: 10,
                    gap: 2,
                    expires_at: now_millis() + 60_000,
                    created_at: now_millis(),
                    pid: 0,
                    name: Some("water".to_string()),
                    class: Some("reminder".to_string()),
                    summary: "hydrate".to_string(),
                },
                StackEntry {
                    id: 2,
                    position: "top".to_string(),
                    height: 10,
                    gap: 2,
                    expires_at: now_millis() + 60_000,
                    created_at: now_millis(),
                    pid: 0,
                    name: Some("other".to_string()),
                    class: Some("reminder".to_string()),
                    summary: "other".to_string(),
                },
            ],
        };
        save_state(&paths.state_path, &state).expect("save");

        let removed =
            clear_active_entries(&paths, ClearSelector::Name("water".to_string())).expect("clear");
        assert_eq!(removed, 1);
        let updated = load_state(&paths.state_path).expect("reload");
        assert_eq!(updated.entries.len(), 1);
        assert_eq!(updated.entries[0].id, 2);
    }

    #[test]
    fn list_active_prunes_expired_and_dead_entries() {
        let paths = test_paths();
        let now = now_millis();
        let state = StackState {
            next_id: 4,
            entries: vec![
                StackEntry {
                    id: 1,
                    position: "top".to_string(),
                    height: 10,
                    gap: 2,
                    expires_at: now + 60_000,
                    created_at: now,
                    pid: 0,
                    name: Some("alive".to_string()),
                    class: Some("class".to_string()),
                    summary: "alive".to_string(),
                },
                StackEntry {
                    id: 2,
                    position: "top".to_string(),
                    height: 10,
                    gap: 2,
                    expires_at: now.saturating_sub(1),
                    created_at: now,
                    pid: 0,
                    name: Some("expired".to_string()),
                    class: Some("class".to_string()),
                    summary: "expired".to_string(),
                },
                StackEntry {
                    id: 3,
                    position: "top".to_string(),
                    height: 10,
                    gap: 2,
                    expires_at: now + 60_000,
                    created_at: now,
                    pid: 999_999,
                    name: Some("dead-pid".to_string()),
                    class: Some("class".to_string()),
                    summary: "dead".to_string(),
                },
            ],
        };
        save_state(&paths.state_path, &state).expect("save");

        let entries = list_active_entries(&paths).expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 1);
    }
}
