//! Input injection for `touch` / `scroll` (§6.1) through
//! `zwlr_virtual_pointer_v1`, targeted at the virtual output so normalised
//! coordinates (§7) map onto it directly. Runs on its own thread because a
//! Wayland event queue is not `Send`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use opendisplay_proto::control::{Pencil, PencilPhase, Scroll, Touch, TouchPhase};
use tracing::{debug, warn};

use crate::backend::InputSink;
use crate::bounded::{BoundedThread, DROP_JOIN};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

const BTN_LEFT: u32 = 0x110;

/// What the Apple Pencil does to the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum PencilMode {
    /// Contact holds the left button, so the pen draws/selects like a finger.
    #[default]
    Draw,
    /// Contact only moves the pointer, never presses: aim with the pen, click
    /// with the keyboard (what an osu! tablet player does, minus hover).
    Move,
    /// Ignore pen events entirely; fingers keep working.
    Off,
}

static PENCIL_MODE: AtomicU8 = AtomicU8::new(0);
/// Process-wide: the sender is one session, and the injector is built deep in
/// the backend where the CLI options do not reach.
pub fn set_pencil_mode(mode: PencilMode) {
    PENCIL_MODE.store(
        match mode {
            PencilMode::Draw => 0,
            PencilMode::Move => 1,
            PencilMode::Off => 2,
        },
        Ordering::Relaxed,
    );
}

pub fn current_pencil_mode() -> PencilMode {
    pencil_mode()
}

fn pencil_mode() -> PencilMode {
    match PENCIL_MODE.load(Ordering::Relaxed) {
        1 => PencilMode::Move,
        2 => PencilMode::Off,
        _ => PencilMode::Draw,
    }
}

/// The rectangle of the pen's surface that is mapped onto the whole display,
/// in normalised coordinates. Drawing tablets call this the active area: a
/// smaller one means less pen travel for the same cursor movement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PencilArea {
    pub x0: f64,
    pub y0: f64,
    pub w: f64,
    pub h: f64,
}

impl Default for PencilArea {
    fn default() -> Self {
        Self {
            x0: 0.0,
            y0: 0.0,
            w: 1.0,
            h: 1.0,
        }
    }
}

impl PencilArea {
    /// Parse `0.6` (centred square), `60x50` (percent), `0.6x0.5`
    /// (fractions), or `x,y,w,h` (an off-centre rectangle).
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let spec = spec.trim();
        let frac = |s: &str| -> anyhow::Result<f64> {
            let v: f64 = s
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("{s:?} is not a number"))?;
            // A bare number above 1 is read as a percentage.
            let v = if v > 1.0 { v / 100.0 } else { v };
            Ok(v)
        };
        let area = if spec.contains(',') {
            let p: Vec<&str> = spec.split(',').collect();
            if p.len() != 4 {
                anyhow::bail!("expected x,y,w,h, e.g. 0.1,0.1,0.8,0.8");
            }
            PencilArea {
                x0: frac(p[0])?,
                y0: frac(p[1])?,
                w: frac(p[2])?,
                h: frac(p[3])?,
            }
        } else {
            let (w, h) = match spec.split_once(['x', 'X']) {
                Some((w, h)) => (frac(w)?, frac(h)?),
                None => {
                    let v = frac(spec)?;
                    (v, v)
                }
            };
            PencilArea {
                x0: (1.0 - w) / 2.0,
                y0: (1.0 - h) / 2.0,
                w,
                h,
            }
        };
        let ok = area.w > 0.0
            && area.h > 0.0
            && area.w <= 1.0
            && area.h <= 1.0
            && area.x0 >= 0.0
            && area.y0 >= 0.0
            && area.x0 + area.w <= 1.000_001
            && area.y0 + area.h <= 1.000_001;
        if !ok {
            anyhow::bail!("pencil area must fit inside the surface (0..1), got {spec:?}");
        }
        Ok(area)
    }

    /// Pen position → display position, clamped at the area's edges.
    fn mapped(&self, x: f64, y: f64) -> (f64, f64) {
        (
            ((x - self.x0) / self.w).clamp(0.0, 1.0),
            ((y - self.y0) / self.h).clamp(0.0, 1.0),
        )
    }
}

/// How pen positions move the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum PencilMotion {
    /// The cursor follows the pen's position; `--pencil-area` maps a region.
    #[default]
    Absolute,
    /// The cursor stays where it is when the pen touches down and then moves
    /// by the pen's deltas, like a touchpad. `--pencil-sensitivity` scales it.
    Relative,
}

static PENCIL_MOTION: AtomicU8 = AtomicU8::new(0);

pub fn set_pencil_motion(motion: PencilMotion) {
    PENCIL_MOTION.store(
        match motion {
            PencilMotion::Absolute => 0,
            PencilMotion::Relative => 1,
        },
        Ordering::Relaxed,
    );
}

pub fn current_pencil_motion() -> PencilMotion {
    if PENCIL_MOTION.load(Ordering::Relaxed) == 1 {
        PencilMotion::Relative
    } else {
        PencilMotion::Absolute
    }
}

fn pencil_motion() -> PencilMotion {
    current_pencil_motion()
}

/// How many screen widths a full sweep of the pen surface covers in relative
/// mode. 1.0 means the cursor crosses one screen per full sweep.
static PENCIL_SENSITIVITY: std::sync::Mutex<f64> = std::sync::Mutex::new(1.0);

pub fn set_pencil_sensitivity(value: f64) {
    if let Ok(mut v) = PENCIL_SENSITIVITY.lock() {
        *v = value.clamp(0.05, 10.0);
    }
}

pub fn current_pencil_sensitivity() -> f64 {
    PENCIL_SENSITIVITY.lock().map(|v| *v).unwrap_or(1.0)
}

fn pencil_sensitivity() -> f64 {
    current_pencil_sensitivity()
}

static PENCIL_AREA: std::sync::Mutex<PencilArea> = std::sync::Mutex::new(PencilArea {
    x0: 0.0,
    y0: 0.0,
    w: 1.0,
    h: 1.0,
});

pub fn set_pencil_area(area: PencilArea) {
    if let Ok(mut current) = PENCIL_AREA.lock() {
        *current = area;
    }
}

pub fn current_pencil_area() -> PencilArea {
    PENCIL_AREA.lock().map(|a| *a).unwrap_or_default()
}

fn pencil_area() -> PencilArea {
    current_pencil_area()
}

#[derive(Debug)]
enum InputCmd {
    Touch(Touch),
    Scroll(Scroll),
    /// Apple Pencil: the same absolute pointer as touch, with the button held
    /// for down..up. Wayland's virtual pointer has no pressure or tilt.
    Pencil(Pencil),
    /// Move without pressing (used by the self-test).
    Move {
        x: f64,
        y: f64,
    },
    Stop,
}

struct State {
    seat: Option<wl_seat::WlSeat>,
    outputs: HashMap<u32, (wl_output::WlOutput, String, (i32, i32))>,
    manager: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    manager_version: u32,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, 1, qh, ()))
                }
                "wl_output" if version >= 4 => {
                    let o: wl_output::WlOutput = registry.bind(name, 4, qh, name);
                    state.outputs.insert(name, (o, String::new(), (0, 0)));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    let v = version.min(2);
                    state.manager_version = v;
                    state.manager = Some(registry.bind(name, v, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(global) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => o.1 = name,
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } if flags
                .into_result()
                .is_ok_and(|f| f.contains(wl_output::Mode::Current)) =>
            {
                o.2 = (width, height);
            }
            _ => {}
        }
    }
}

macro_rules! noop_dispatch {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
noop_dispatch!(
    wl_seat::WlSeat,
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1
);

pub struct InputInjector {
    tx: mpsc::Sender<InputCmd>,
    thread: BoundedThread,
}

impl InputInjector {
    /// Start the injector thread for the output named `output_name`;
    /// `scale` is the output scale, used to turn video pixels into logical pixels
    /// for scroll deltas.
    pub fn start(output_name: &str, scale: f64) -> Result<InputInjector> {
        let (tx, rx) = mpsc::channel::<InputCmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let name = output_name.to_string();
        let thread = BoundedThread::spawn("input", move || {
            let r = injector_thread(&name, scale, rx, &ready_tx);
            if let Err(e) = r {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        ready_rx.recv().context("input thread died")??;
        Ok(InputInjector { tx, thread })
    }

    fn send(&self, cmd: InputCmd) {
        let _ = self.tx.send(cmd);
    }

    /// Move the pointer without pressing; `x`/`y` normalised to the output.
    pub fn move_to(&self, x: f64, y: f64) {
        self.send(InputCmd::Move { x, y });
    }
}

impl InputSink for InputInjector {
    fn touch(&mut self, t: &Touch) {
        self.send(InputCmd::Touch(t.clone()));
    }

    fn scroll(&mut self, s: &Scroll) {
        self.send(InputCmd::Scroll(s.clone()));
    }

    fn pencil(&mut self, p: &Pencil) {
        self.send(InputCmd::Pencil(p.clone()));
    }
}

impl Drop for InputInjector {
    /// Waits (bounded) for the thread to release a held button and destroy
    /// the pointer: the compositor keeps the seat's button count, and the
    /// process may exit right after this.
    fn drop(&mut self) {
        let _ = self.tx.send(InputCmd::Stop);
        self.thread.join_within(DROP_JOIN);
    }
}

fn injector_thread(
    output_name: &str,
    scale: f64,
    rx: mpsc::Receiver<InputCmd>,
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland")?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State {
        seat: None,
        outputs: HashMap::new(),
        manager: None,
        manager_version: 0,
    };
    queue.roundtrip(&mut st)?;
    queue.roundtrip(&mut st)?;
    let manager = st
        .manager
        .clone()
        .context("compositor lacks zwlr_virtual_pointer_manager_v1")?;
    let (output, _, (w, h)) = st
        .outputs
        .values()
        .find(|(_, n, _)| n == output_name)
        .cloned()
        .with_context(|| format!("no output {output_name}"))?;
    if w <= 0 || h <= 0 {
        bail!("output {output_name} has no mode yet");
    }
    let pointer = if st.manager_version >= 2 {
        manager.create_virtual_pointer_with_output(st.seat.as_ref(), Some(&output), &qh, ())
    } else {
        warn!(
            "virtual pointer manager v1: cannot bind to the output; absolute motion uses the whole layout"
        );
        manager.create_virtual_pointer(st.seat.as_ref(), &qh, ())
    };
    let _ = ready.send(Ok(()));
    let start = Instant::now();
    let time = |start: Instant| start.elapsed().as_millis() as u32;
    // Extents are the output's physical size; x/y are normalised (§7).
    let (xe, ye) = (w as u32, h as u32);
    let to_abs = |x: f64, y: f64| {
        (
            (x.clamp(0.0, 1.0) * xe as f64).round() as u32,
            (y.clamp(0.0, 1.0) * ye as f64).round() as u32,
        )
    };
    let mut pressed = false;
    let mut pressed_at: Option<Duration> = None;
    let mut last_pen: Option<(f64, f64)> = None;
    for cmd in rx {
        match cmd {
            InputCmd::Stop => break,
            InputCmd::Move { x, y } => {
                let (ax, ay) = to_abs(x, y);
                pointer.motion_absolute(time(start), ax, ay, xe, ye);
                pointer.frame();
            }
            InputCmd::Touch(t) => {
                let (ax, ay) = to_abs(t.x, t.y);
                pointer.motion_absolute(time(start), ax, ay, xe, ye);
                pointer.frame();
                match t.phase {
                    TouchPhase::Began => {
                        if !pressed {
                            pointer.button(time(start), BTN_LEFT, wl_pointer::ButtonState::Pressed);
                            pointer.frame();
                            pressed = true;
                            pressed_at = Some(start.elapsed());
                        }
                    }
                    TouchPhase::Moved => {}
                    TouchPhase::Ended | TouchPhase::Cancelled => {
                        if pressed {
                            pointer.button(
                                time(start),
                                BTN_LEFT,
                                wl_pointer::ButtonState::Released,
                            );
                            pointer.frame();
                            pressed = false;
                            pressed_at = None;
                        }
                    }
                }
            }
            InputCmd::Pencil(p) => {
                let mode = pencil_mode();
                if mode != PencilMode::Off {
                    match pencil_motion() {
                        PencilMotion::Absolute => {
                            let (x, y) = pencil_area().mapped(p.x, p.y);
                            let (ax, ay) = to_abs(x, y);
                            pointer.motion_absolute(time(start), ax, ay, xe, ye);
                            pointer.frame();
                        }
                        PencilMotion::Relative => {
                            // Anchored on contact: the cursor does not jump to
                            // the pen, it only moves once the pen does.
                            if p.phase != PencilPhase::Down {
                                if let Some((lx, ly)) = last_pen {
                                    let s = pencil_sensitivity();
                                    let (lw, lh) = (xe as f64 / scale, ye as f64 / scale);
                                    let dx = (p.x - lx) * lw * s;
                                    let dy = (p.y - ly) * lh * s;
                                    if dx != 0.0 || dy != 0.0 {
                                        pointer.motion(time(start), dx, dy);
                                        pointer.frame();
                                    }
                                }
                            }
                            if p.phase == PencilPhase::Up {
                                last_pen = None;
                            } else {
                                last_pen = Some((p.x, p.y));
                            }
                        }
                    }
                }
                // `Move`: aim only. A pen player clicks with the keyboard, so
                // the pointer must never be left holding a button. `Off`:
                // nothing at all. `Draw`: contact is a held left button.
                if mode == PencilMode::Draw {
                    match p.phase {
                        PencilPhase::Down => {
                            if !pressed {
                                pointer.button(
                                    time(start),
                                    BTN_LEFT,
                                    wl_pointer::ButtonState::Pressed,
                                );
                                pointer.frame();
                                pressed = true;
                                pressed_at = Some(start.elapsed());
                            }
                        }
                        // A hover move just positions the pointer; no button.
                        PencilPhase::Move | PencilPhase::Hover => {}
                        PencilPhase::Up => {
                            if pressed {
                                // A contact shorter than a frame is easy for a
                                // game polling at 60 Hz to miss; hold one frame.
                                if let Some(at) = pressed_at {
                                    let held = start.elapsed().saturating_sub(at);
                                    if held < Duration::from_millis(16) {
                                        std::thread::sleep(
                                            Duration::from_millis(16).saturating_sub(held),
                                        );
                                    }
                                }
                                pointer.button(
                                    time(start),
                                    BTN_LEFT,
                                    wl_pointer::ButtonState::Released,
                                );
                                pointer.frame();
                                pressed = false;
                                pressed_at = None;
                            }
                        }
                    }
                }
            }
            InputCmd::Scroll(s) => {
                // §6.1: deltas in video pixels with natural-scrolling sign
                // (content follows the fingers). wl_pointer's positive vertical
                // axis scrolls content up, so the sign flips; units are logical pixels.
                pointer.axis_source(wl_pointer::AxisSource::Finger);
                if s.dy != 0.0 {
                    pointer.axis(time(start), wl_pointer::Axis::VerticalScroll, -s.dy / scale);
                }
                if s.dx != 0.0 {
                    pointer.axis(
                        time(start),
                        wl_pointer::Axis::HorizontalScroll,
                        -s.dx / scale,
                    );
                }
                pointer.frame();
            }
        }
        conn.flush()?;
        queue.dispatch_pending(&mut st)?;
    }
    if pressed {
        pointer.button(time(start), BTN_LEFT, wl_pointer::ButtonState::Released);
        pointer.frame();
    }
    pointer.destroy();
    let _ = queue.roundtrip(&mut st);
    debug!("input thread done");
    Ok(())
}

#[cfg(test)]
mod pencil_tests {
    use super::PencilArea;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn the_default_area_is_the_whole_surface() {
        let a = PencilArea::default();
        assert_eq!(a.mapped(0.0, 0.0), (0.0, 0.0));
        assert_eq!(a.mapped(0.25, 0.75), (0.25, 0.75));
    }

    #[test]
    fn a_smaller_area_magnifies_movement() {
        let a = PencilArea::parse("0.5").unwrap();
        assert!(close(a.x0, 0.25) && close(a.h, 0.5));
        for (pen, want) in [(0.25, 0.0), (0.5, 0.5), (0.75, 1.0), (0.1, 0.0)] {
            let (x, y) = a.mapped(pen, pen);
            assert!(close(x, want) && close(y, want), "{pen} -> {x},{y}");
        }
        // Outside the area clamps to the edge.
        let (x, y) = a.mapped(0.1, 0.9);
        assert!(close(x, 0.0) && close(y, 1.0));
    }

    #[test]
    fn percent_and_off_centre_forms_parse() {
        let a = PencilArea::parse("60x50").unwrap();
        assert!(close(a.w, 0.6) && close(a.h, 0.5));
        assert!(close(a.x0, 0.2) && close(a.y0, 0.25));
        let b = PencilArea::parse("0.1, 0.2, 0.5, 0.5").unwrap();
        assert!(close(b.x0, 0.1) && close(b.y0, 0.2));
        let (x, y) = b.mapped(0.1, 0.2);
        assert!(close(x, 0.0) && close(y, 0.0));
        let (x, y) = b.mapped(0.6, 0.7);
        assert!(close(x, 1.0) && close(y, 1.0));
    }

    #[test]
    fn areas_outside_the_surface_are_rejected() {
        assert!(PencilArea::parse("0.1,0.1,0.95,0.8").is_err());
        assert!(PencilArea::parse("0").is_err());
        assert!(PencilArea::parse("wide").is_err());
        assert!(PencilArea::parse("0.1,0.2,0.3").is_err());
    }
}
