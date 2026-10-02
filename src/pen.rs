use anyhow::Result;
use evdev::EventType as EvdevEventType;
use evdev::{Device, InputEvent};
use log::info;
use resvg::usvg;
use resvg::usvg::{Options, Tree};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::device::DeviceModel;
use crate::touch::Rect;

// Output dimensions remain the same for both devices
const VIRTUAL_WIDTH: u32 = 768;
const VIRTUAL_HEIGHT: u32 = 1024;

// evdev codes used by the injector
const BTN_TOOL_PEN: u16 = 320;
const BTN_TOOL_RUBBER: u16 = 321;
const BTN_TOUCH: u16 = 330;

/// Default ceiling on injected SYN reports per second. Not a measured safe
/// rate: it only stops unbounded bursts from overrunning xochitl's evdev
/// client buffer (SYN_DROPPED). 0 disables pacing.
pub const DEFAULT_PEN_MAX_RATE: u32 = 4000;
/// Keep waiting this long after the real pen was last in range.
const REAL_PEN_HOLDOFF: Duration = Duration::from_millis(500);
/// Give up on a stroke if the real pen never leaves.
const REAL_PEN_WAIT_LIMIT: Duration = Duration::from_secs(60);
const REAL_PEN_POLL: Duration = Duration::from_millis(20);

pub struct Pen {
    device: Option<Device>,
    device_model: DeviceModel,
    /// True between our own tool-in (BTN_TOOL_PEN/RUBBER=1) and tool-out.
    injecting: bool,
    /// Minimum spacing between injected SYN reports (zero = unpaced).
    report_interval: Duration,
    next_report: Instant,
}

impl Pen {
    pub fn new(no_draw: bool) -> Self {
        Self::with_rate(no_draw, DEFAULT_PEN_MAX_RATE)
    }

    /// `max_rate` caps injected SYN reports per second (0 = unpaced).
    pub fn with_rate(no_draw: bool, max_rate: u32) -> Self {
        let device_model = DeviceModel::detect();
        info!("Pen using device model: {}", device_model.name());

        let pen_input_device = match device_model {
            DeviceModel::Remarkable2 => "/dev/input/event1",
            DeviceModel::RemarkablePaperPro | DeviceModel::RemarkablePaperProMove => "/dev/input/event2",
            DeviceModel::Unknown => "/dev/input/event1", // Default to RM2
        };

        let device = if no_draw { None } else { Some(Device::open(pen_input_device).unwrap()) };

        Self {
            device,
            device_model,
            injecting: false,
            report_interval: Self::report_interval(max_rate),
            next_report: Instant::now(),
        }
    }

    fn report_interval(max_rate: u32) -> Duration {
        if max_rate == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(1) / max_rate
        }
    }

    /// Write one batch of events to the pen device.
    ///
    /// Every injected event goes through here so two rules hold everywhere:
    /// a stroke never starts while the user's real pen is in range or
    /// touching (the events would interleave on one device state machine and
    /// corrupt both strokes), and SYN reports never exceed the rate budget.
    fn emit(&mut self, events: &[InputEvent]) -> Result<()> {
        let Some(device) = self.device.as_mut() else {
            return Ok(());
        };
        let tool_in = Self::tool_change(events);
        if tool_in == Some(true) && !self.injecting {
            Self::wait_for_real_pen_out_of_range(device)?;
        }
        if !self.report_interval.is_zero() {
            let now = Instant::now();
            if self.next_report > now {
                let wait = self.next_report - now;
                // Sleep in >=1 ms slices; finer sleeps cost more than they pace.
                if wait >= Duration::from_millis(1) {
                    sleep(wait);
                }
            }
            let reports = events.iter().filter(|event| event.event_type() == EvdevEventType::SYNCHRONIZATION).count() as u32;
            self.next_report = self.next_report.max(now) + self.report_interval * reports;
        }
        device.send_events(events)?;
        if let Some(tool_in) = tool_in {
            self.injecting = tool_in;
        }
        Ok(())
    }

    /// The last pen/rubber tool transition in a batch: Some(true) when the
    /// batch brings our tool into range, Some(false) when it leaves.
    fn tool_change(events: &[InputEvent]) -> Option<bool> {
        events
            .iter()
            .filter(|event| event.event_type() == EvdevEventType::KEY && matches!(event.code(), BTN_TOOL_PEN | BTN_TOOL_RUBBER))
            .last()
            .map(|event| event.value() != 0)
    }

    /// Block until the real pen has been out of range for REAL_PEN_HOLDOFF.
    ///
    /// Injected events update the kernel's key state too, but every injected
    /// stroke ends with the tool keys released, and the digitizer re-reports
    /// BTN_TOOL_PEN/BTN_TOUCH on its next frame while the real pen is near.
    /// So between our strokes a set tool or touch key means the user's pen.
    fn wait_for_real_pen_out_of_range(device: &Device) -> Result<()> {
        let started = Instant::now();
        let mut last_seen: Option<Instant> = None;
        loop {
            let keys = device.get_key_state()?;
            let real_pen_near = [BTN_TOOL_PEN, BTN_TOOL_RUBBER, BTN_TOUCH]
                .iter()
                .any(|&code| keys.contains(evdev::KeyCode::new(code)));
            let now = Instant::now();
            if real_pen_near {
                if last_seen.is_none() {
                    info!("Real pen in range; holding injection");
                }
                last_seen = Some(now);
            }
            match last_seen {
                None => return Ok(()),
                Some(seen) if now.duration_since(seen) >= REAL_PEN_HOLDOFF => {
                    info!("Real pen left range; resuming injection");
                    return Ok(());
                }
                _ => {}
            }
            anyhow::ensure!(
                now.duration_since(started) < REAL_PEN_WAIT_LIMIT,
                "Real pen stayed in range for {:?}; not injecting",
                REAL_PEN_WAIT_LIMIT
            );
            sleep(REAL_PEN_POLL);
        }
    }

    pub fn draw_line_screen(&mut self, p1: (i32, i32), p2: (i32, i32)) -> Result<()> {
        self.draw_line(self.virtual_to_input(p1), self.virtual_to_input(p2))
    }

    pub fn draw_line(&mut self, (x1, y1): (i32, i32), (x2, y2): (i32, i32)) -> Result<()> {
        // trace!("Drawing from ({}, {}) to ({}, {})", x1, y1, x2, y2);

        // We know this is a straight line
        // So figure out the length
        // Then divide it into enough steps to only go 10 units or so
        // Start at x1, y1
        // And then for each step add the right amount to x and y

        let length = ((x2 as f32 - x1 as f32).powf(2.0) + (y2 as f32 - y1 as f32).powf(2.0)).sqrt();
        // 5.0 is the maximum distance between points
        let steps = (length / 5.0).ceil() as i32;

        self.pen_up()?;
        self.pen_down_at((x1, y1))?;

        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let x = (x1 as f32 + (x2 - x1) as f32 * t).round() as i32;
            let y = (y1 as f32 + (y2 - y1) as f32 * t).round() as i32;
            self.goto_xy((x, y))?;
        }

        self.pen_up()?;

        Ok(())
    }

    /// Erase existing ink within `rect` by sweeping horizontal passes across
    /// it using BTN_TOOL_RUBBER strokes (see `draw_line_rubber_screen`).
    /// Confirmed on-device (2026-07-08): selecting the on-screen eraser
    /// toolbar icon and then stroking with the normal BTN_TOOL_PEN signal
    /// does NOT erase anything, no matter which toolbar tool is active --
    /// xochitl only erases in response to the pen's actual hardware
    /// eraser-tip signal (BTN_TOOL_RUBBER), which also means no toolbar
    /// tool switch is needed at all before calling this.
    pub fn erase_rect(&mut self, rect: Rect) -> Result<()> {
        // The injected rubber's effective radius is only a few virtual px,
        // so sweep densely, and add a perpendicular pass to catch ink that
        // slips between rows (verified on-device: 10px single-pass left
        // stripes of survivors).
        const ROW_SPACING: i32 = 4;
        let margin = 10; // erase slightly past the lassoed box's edges
        let x1 = rect.x - margin;
        let x2 = rect.x + rect.w + margin;
        let y_top = rect.y - margin;
        let y_bottom = rect.y + rect.h + margin;

        let mut y = y_top;
        let mut flip = true;
        while y <= y_bottom {
            let (from, to) = if flip { ((x1, y), (x2, y)) } else { ((x2, y), (x1, y)) };
            self.draw_line_rubber_screen(from, to)?;
            flip = !flip;
            y += ROW_SPACING;
        }
        let mut x = x1;
        while x <= x2 {
            let (from, to) = if flip { ((x, y_top), (x, y_bottom)) } else { ((x, y_bottom), (x, y_top)) };
            self.draw_line_rubber_screen(from, to)?;
            flip = !flip;
            x += ROW_SPACING;
        }
        info!("erase_rect: erased {:?}", rect);
        Ok(())
    }

    pub fn draw_bitmap(&mut self, bitmap: &[Vec<bool>]) -> Result<()> {
        self.draw_bitmap_scaled(bitmap, 1)
    }

    /// Draw a bitmap where bitmap dimensions may differ from the virtual coordinate space.
    /// `scale` = bitmap_size / virtual_size — e.g. scale=2 means a 1536×2048 bitmap
    /// that maps to the 768×1024 virtual space at half-unit precision per pixel.
    /// Input coordinates are computed directly from bitmap pixel position for true sub-pixel accuracy,
    /// bypassing integer rounding through the virtual coordinate space.
    pub fn draw_bitmap_scaled(&mut self, bitmap: &[Vec<bool>], scale: u32) -> Result<()> {
        let max_x = self.max_x_value() as f32;
        let max_y = self.max_y_value() as f32;
        let bmp_w = VIRTUAL_WIDTH as f32 * scale as f32;
        let bmp_h = VIRTUAL_HEIGHT as f32 * scale as f32;

        let mut is_pen_down = false;
        for (y, row) in bitmap.iter().enumerate() {
            for (x, &pixel) in row.iter().enumerate() {
                if pixel {
                    let ix = ((x as f32 / bmp_w) * max_x).round() as i32;
                    let iy = ((y as f32 / bmp_h) * max_y).round() as i32;
                    if !is_pen_down {
                        self.pen_down_at((ix, iy))?;
                        is_pen_down = true;
                        sleep(Duration::from_millis(1));
                    }
                    self.goto_xy((ix, iy))?;
                } else if is_pen_down {
                    self.pen_up()?;
                    is_pen_down = false;
                    sleep(Duration::from_millis(1));
                }
            }
            // Lift at end of every row for clean horizontal-run boundaries
            self.pen_up()?;
            is_pen_down = false;
            sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    // fn draw_dot(device: &mut Device, (x, y): (i32, i32)) -> Result<()> {
    //     // trace!("Drawing at ({}, {})", x, y);
    //     goto_xy(device, (x, y))?;
    //     pen_down(device)?;
    //
    //     // Wiggle a little bit
    //     for n in 0..2 {
    //         goto_xy(device, (x + n, y + n))?;
    //     }
    //
    //     pen_up(device)?;
    //
    //     // sleep for 5ms
    //     thread::sleep(time::Duration::from_millis(1));
    //
    //     Ok(())
    // }

    pub fn pen_down(&mut self) -> Result<()> {
        self.emit(&[
            InputEvent::new(EvdevEventType::KEY.0, 320, 1),           // BTN_TOOL_PEN
            InputEvent::new(EvdevEventType::KEY.0, 330, 1),           // BTN_TOUCH
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, 2630),    // ABS_PRESSURE (max pressure)
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 0),       // ABS_DISTANCE
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0), // SYN_REPORT
        ])?;
        Ok(())
    }

    // Press the pen down at a specific input-space coordinate cleanly:
    // First hover at the target position (BTN_TOOL_PEN=1, no touch), then press.
    // This pre-positions the pen so there's no snap artifact from a prior position.
    pub fn pen_down_at(&mut self, (x, y): (i32, i32)) -> Result<()> {
        // Hover far: activate tool at target position from far away (no mark)
        self.emit(&[
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 0, x),        // ABS_X
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 1, y),        // ABS_Y
            InputEvent::new(EvdevEventType::KEY.0, 320, 1),           // BTN_TOOL_PEN
            InputEvent::new(EvdevEventType::KEY.0, 330, 0),           // BTN_TOUCH (not yet)
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, 0),       // ABS_PRESSURE = 0
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 100),     // ABS_DISTANCE far
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0), // SYN_REPORT
        ])?;
        // Press: touch down at same position
        self.emit(&[
            InputEvent::new(EvdevEventType::KEY.0, 330, 1),           // BTN_TOUCH
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, 2630),    // ABS_PRESSURE (max)
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 0),       // ABS_DISTANCE = 0
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0), // SYN_REPORT
        ])?;
        Ok(())
    }

    pub fn pen_up(&mut self) -> Result<()> {
        self.emit(&[
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, 0),       // ABS_PRESSURE
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 100),     // ABS_DISTANCE
            InputEvent::new(EvdevEventType::KEY.0, 330, 0),           // BTN_TOUCH
            InputEvent::new(EvdevEventType::KEY.0, 320, 0),           // BTN_TOOL_PEN
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0), // SYN_REPORT
        ])?;
        Ok(())
    }

    pub fn goto_xy_virtual(&mut self, point: (i32, i32)) -> Result<()> {
        self.goto_xy(self.virtual_to_input(point))
    }

    // --- diagnostic: BTN_TOOL_RUBBER (321) instead of BTN_TOOL_PEN (320) ---
    // Some styluses report a physical eraser-tip flip as a genuinely distinct
    // hardware tool type, separate from the on-screen toolbar tool selection.
    // Used to test whether xochitl's eraser needs this signal rather than
    // (or in addition to) the toolbar eraser tool being selected.
    fn rubber_down_at(&mut self, (x, y): (i32, i32)) -> Result<()> {
        self.emit(&[
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 0, x),
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 1, y),
            InputEvent::new(EvdevEventType::KEY.0, 321, 1), // BTN_TOOL_RUBBER
            InputEvent::new(EvdevEventType::KEY.0, 330, 0), // BTN_TOUCH (not yet)
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, 0),
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 100),
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0),
        ])?;
        self.emit(&[
            InputEvent::new(EvdevEventType::KEY.0, 330, 1), // BTN_TOUCH
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, 2630),
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 0),
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0),
        ])?;
        Ok(())
    }

    fn rubber_up(&mut self) -> Result<()> {
        self.emit(&[
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, 0),
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 100),
            InputEvent::new(EvdevEventType::KEY.0, 330, 0),
            InputEvent::new(EvdevEventType::KEY.0, 321, 0), // BTN_TOOL_RUBBER off
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0),
        ])?;
        Ok(())
    }

    pub fn draw_line_rubber_screen(&mut self, p1: (i32, i32), p2: (i32, i32)) -> Result<()> {
        self.draw_line_rubber(self.virtual_to_input(p1), self.virtual_to_input(p2))
    }

    fn draw_line_rubber(&mut self, (x1, y1): (i32, i32), (x2, y2): (i32, i32)) -> Result<()> {
        let length = ((x2 as f32 - x1 as f32).powi(2) + (y2 as f32 - y1 as f32).powi(2)).sqrt();
        let steps = (length / 5.0).ceil().max(1.0) as i32;
        // Pace like the pen path (see draw_virtual_paths/draw_segment):
        // xochitl drops tool-transition and move events sent as a raw burst,
        // which made erase sweeps register only intermittently.
        self.rubber_down_at((x1, y1))?;
        sleep(Duration::from_millis(3));
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let x = (x1 as f32 + (x2 - x1) as f32 * t).round() as i32;
            let y = (y1 as f32 + (y2 - y1) as f32 * t).round() as i32;
            self.goto_xy((x, y))?;
            if i % 100 == 99 {
                sleep(Duration::from_millis(1));
            }
        }
        sleep(Duration::from_millis(3));
        self.rubber_up()?;
        sleep(Duration::from_millis(3));
        Ok(())
    }

    pub fn goto_xy(&mut self, (x, y): (i32, i32)) -> Result<()> {
        self.emit(&[
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 0, x),        // ABS_X
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 1, y),        // ABS_Y
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0), // SYN_REPORT
        ])?;
        Ok(())
    }

    pub fn max_x_value(&self) -> i32 {
        match self.device_model {
            DeviceModel::Remarkable2 => 15725,
            DeviceModel::RemarkablePaperPro => 11180,
            DeviceModel::RemarkablePaperProMove => 6760,
            DeviceModel::Unknown => 15725, // Default to RM2
        }
    }

    pub fn max_y_value(&self) -> i32 {
        match self.device_model {
            DeviceModel::Remarkable2 => 20966,
            DeviceModel::RemarkablePaperPro => 15340,
            DeviceModel::RemarkablePaperProMove => 11960,
            DeviceModel::Unknown => 20966, // Default to RM2
        }
    }

    pub fn virtual_to_input_pub(&self, point: (i32, i32)) -> (i32, i32) {
        self.virtual_to_input(point)
    }

    // Draw a single segment from prev to (px,py), emitting interpolated goto_xy events.
    // Returns updated step_count.
    fn draw_segment(
        &mut self,
        prev: (f32, f32),
        (px, py): (f32, f32),
        step_count: &mut usize,
        max_step: f32,
    ) -> Result<()> {
        let dist = ((px - prev.0).powi(2) + (py - prev.1).powi(2)).sqrt();
        let steps = ((dist / max_step).ceil() as usize).max(1);
        for s in 1..=steps {
            let t = s as f32 / steps as f32;
            let ix = (prev.0 + (px - prev.0) * t).round() as i32;
            let iy = (prev.1 + (py - prev.1) * t).round() as i32;
            self.goto_xy_virtual((ix, iy))?;
            *step_count += 1;
            if *step_count % 100 == 99 {
                sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    fn draw_polylines(&mut self, polylines: &[svg2polylines::Polyline]) -> Result<()> {
        // Strategy:
        // - Simple polygons (avg segment length > 10 virtual units): split at sharp corners (≥ 25°)
        //   for crisp geometric edges. At each corner, overshoot 3 units in the incoming direction
        //   before lifting to ensure the stroke actually reaches the corner point (prevents gaps).
        // - Complex paths (short avg segment): text glyphs, circles, curves — draw continuously.
        //   avg_segment_len distinguishes these: rect/triangle sides are 100-800 units long;
        //   text glyph segments from svg2polylines at 0.5 tolerance are typically 1-5 units.
        const MAX_STEP: f32 = 1.0; // virtual units per event

        for polyline in polylines {
            if polyline.len() < 2 {
                continue;
            }

            // Compute average segment length to classify path type
            let avg_segment_len: f32 = {
                let mut total = 0.0f32;
                for i in 1..polyline.len() {
                    let dx = (polyline[i].x - polyline[i - 1].x) as f32;
                    let dy = (polyline[i].y - polyline[i - 1].y) as f32;
                    total += (dx * dx + dy * dy).sqrt();
                }
                total / (polyline.len() - 1) as f32
            };

            // Simple polygons have long segments; text/circles have many short segments.
            // Simple geometric shapes (line, triangle, rect) have avg_seg > 100 units.
            // Text glyphs and circles have avg_seg < 20 even at large font sizes.
            // Use 50.0 as threshold to safely distinguish them.
            let corner_threshold_deg: f32 = if avg_segment_len > 50.0 { 25.0 } else { 360.0 };
            info!(
                "polyline: {} pts, avg_seg={:.1}, threshold={}°",
                polyline.len(),
                avg_segment_len,
                corner_threshold_deg as u32
            );

            let mut step_count = 0usize;
            let mut pen_is_down = false;
            let mut prev = (polyline[0].x as f32, polyline[0].y as f32);

            for i in 0..polyline.len() {
                let cur = (polyline[i].x as f32, polyline[i].y as f32);

                if !pen_is_down {
                    let start = self.virtual_to_input((cur.0 as i32, cur.1 as i32));
                    self.pen_up()?;
                    sleep(Duration::from_millis(2));
                    self.pen_down_at(start)?;
                    sleep(Duration::from_millis(2));
                    pen_is_down = true;
                    prev = cur;
                    continue;
                }

                // Check for sharp corner at cur (between prev→cur and cur→next).
                let should_lift = if i + 1 < polyline.len() {
                    let next = (polyline[i + 1].x as f32, polyline[i + 1].y as f32);
                    let d_in = (cur.0 - prev.0, cur.1 - prev.1);
                    let d_out = (next.0 - cur.0, next.1 - cur.1);
                    let len_in = (d_in.0.powi(2) + d_in.1.powi(2)).sqrt();
                    let len_out = (d_out.0.powi(2) + d_out.1.powi(2)).sqrt();
                    if len_in > 0.1 && len_out > 0.1 {
                        let cos_a = (d_in.0 * d_out.0 + d_in.1 * d_out.1) / (len_in * len_out);
                        cos_a.clamp(-1.0, 1.0).acos().to_degrees() > corner_threshold_deg
                    } else {
                        false
                    }
                } else {
                    false
                };

                if should_lift {
                    // Overshoot 2 units past the corner in the incoming direction to ensure
                    // the stroke fully reaches the corner before lifting (prevents gaps).
                    let d_in = (cur.0 - prev.0, cur.1 - prev.1);
                    let len_in = (d_in.0.powi(2) + d_in.1.powi(2)).sqrt();
                    let overshoot = if len_in > 0.1 {
                        let unit = (d_in.0 / len_in, d_in.1 / len_in);
                        (cur.0 + unit.0 * 3.0, cur.1 + unit.1 * 3.0)
                    } else {
                        cur
                    };
                    self.draw_segment(prev, overshoot, &mut step_count, MAX_STEP)?;
                    self.pen_up()?;
                    sleep(Duration::from_millis(2));
                    // Re-press at the exact corner (not the overshoot) for a clean start
                    let start = self.virtual_to_input((cur.0 as i32, cur.1 as i32));
                    self.pen_down_at(start)?;
                    sleep(Duration::from_millis(2));
                } else {
                    self.draw_segment(prev, cur, &mut step_count, MAX_STEP)?;
                }

                prev = cur;
            }

            self.pen_up()?;
            sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    /// Draw a bitmap bidirectionally — alternating L→R and R→L per row.
    /// This cancels the directional bias that causes horizontal leaking.
    pub fn draw_bitmap_bidi(&mut self, bitmap: &[Vec<bool>], scale: u32) -> Result<()> {
        let max_x = self.max_x_value() as f32;
        let max_y = self.max_y_value() as f32;
        let bmp_w = VIRTUAL_WIDTH as f32 * scale as f32;
        let bmp_h = VIRTUAL_HEIGHT as f32 * scale as f32;

        let mut is_pen_down = false;
        for (y, row) in bitmap.iter().enumerate() {
            let iy = ((y as f32 / bmp_h) * max_y).round() as i32;
            let cols = row.len();

            // Collect runs in this row
            let mut runs: Vec<(usize, usize)> = Vec::new();
            let mut run_start: Option<usize> = None;
            for (x, &pixel) in row.iter().enumerate() {
                match (pixel, run_start) {
                    (true, None) => run_start = Some(x),
                    (false, Some(s)) => {
                        runs.push((s, x - 1));
                        run_start = None;
                    }
                    _ => {}
                }
            }
            if let Some(s) = run_start {
                runs.push((s, cols - 1));
            }

            // Even rows: L→R, odd rows: R→L
            let go_left = (y % 2) == 1;
            let ordered_runs: Vec<(usize, usize)> = if go_left {
                runs.iter().rev().cloned().collect()
            } else {
                runs.clone()
            };

            for (x_start, x_end) in ordered_runs {
                // For L→R draw start→end; for R→L draw end→start
                let (draw_from, draw_to) = if go_left {
                    (x_end, x_start)
                } else {
                    (x_start, x_end)
                };

                let ix_from = ((draw_from as f32 / bmp_w) * max_x).round() as i32;
                let ix_to = ((draw_to as f32 / bmp_w) * max_x).round() as i32;

                if is_pen_down {
                    self.pen_up()?;
                    is_pen_down = false;
                    sleep(Duration::from_millis(1));
                }
                self.pen_down_at((ix_from, iy))?;
                is_pen_down = true;
                sleep(Duration::from_millis(1));
                self.goto_xy((ix_to, iy))?;
                self.pen_up()?;
                is_pen_down = false;
                sleep(Duration::from_millis(1));
            }

            // Lift at end of every row
            if is_pen_down {
                self.pen_up()?;
                is_pen_down = false;
            }
            sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// Draw a bitmap column-first — scanning each column top→bottom.
    /// This rotates the directional bias 90° so it appears vertically instead of horizontally.
    pub fn draw_bitmap_col(&mut self, bitmap: &[Vec<bool>], scale: u32) -> Result<()> {
        let max_x = self.max_x_value() as f32;
        let max_y = self.max_y_value() as f32;
        let bmp_w = VIRTUAL_WIDTH as f32 * scale as f32;
        let bmp_h = VIRTUAL_HEIGHT as f32 * scale as f32;

        if bitmap.is_empty() {
            return Ok(());
        }
        let rows = bitmap.len();
        let cols = bitmap[0].len();

        for x in 0..cols {
            let ix = ((x as f32 / bmp_w) * max_x).round() as i32;

            let mut is_pen_down = false;
            let mut run_start: Option<usize> = None;

            for y in 0..rows {
                let pixel = bitmap[y][x];
                let iy = ((y as f32 / bmp_h) * max_y).round() as i32;

                match (pixel, run_start) {
                    (true, None) => {
                        run_start = Some(y);
                        self.pen_down_at((ix, iy))?;
                        is_pen_down = true;
                        sleep(Duration::from_millis(1));
                        self.goto_xy((ix, iy))?;
                    }
                    (true, Some(_)) => {
                        self.goto_xy((ix, iy))?;
                    }
                    (false, Some(_)) => {
                        run_start = None;
                        self.pen_up()?;
                        is_pen_down = false;
                        sleep(Duration::from_millis(1));
                    }
                    (false, None) => {}
                }
            }

            if is_pen_down {
                self.pen_up()?;
                sleep(Duration::from_millis(1));
            }
            sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    /// Draw using alpha values as pen pressure for anti-aliased rendering.
    /// Takes a Vec<Vec<u8>> of alpha values (0-255) and draws each pixel
    /// with pressure proportional to its alpha value.
    pub fn draw_bitmap_alpha_pressure(&mut self, alpha_bitmap: &[Vec<u8>], scale: u32) -> Result<()> {
        let max_x = self.max_x_value() as f32;
        let max_y = self.max_y_value() as f32;
        let bmp_w = VIRTUAL_WIDTH as f32 * scale as f32;
        let bmp_h = VIRTUAL_HEIGHT as f32 * scale as f32;

        for (y, row) in alpha_bitmap.iter().enumerate() {
            let iy = ((y as f32 / bmp_h) * max_y).round() as i32;

            let mut is_pen_down = false;
            let mut last_pressure: i32 = 0;

            for (x, &alpha) in row.iter().enumerate() {
                if alpha <= 15 {
                    if is_pen_down {
                        self.pen_up()?;
                        is_pen_down = false;
                        sleep(Duration::from_millis(1));
                    }
                    continue;
                }

                let ix = ((x as f32 / bmp_w) * max_x).round() as i32;
                let pressure = (alpha as f32 / 255.0 * 2630.0).round() as i32;

                if !is_pen_down || pressure != last_pressure {
                    if is_pen_down {
                        self.pen_up()?;
                        sleep(Duration::from_millis(1));
                    }
                    self.goto_xy_with_pressure((ix, iy), pressure)?;
                    is_pen_down = true;
                    last_pressure = pressure;
                } else {
                    self.goto_xy((ix, iy))?;
                }
            }

            if is_pen_down {
                self.pen_up()?;
                is_pen_down = false;
            }
            sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// Move to position with a specific pressure value (for alpha-pressure rendering).
    fn goto_xy_with_pressure(&mut self, (x, y): (i32, i32), pressure: i32) -> Result<()> {
        // Hover to position first
        self.emit(&[
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 0, x),        // ABS_X
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 1, y),        // ABS_Y
            InputEvent::new(EvdevEventType::KEY.0, 320, 1),           // BTN_TOOL_PEN
            InputEvent::new(EvdevEventType::KEY.0, 330, 0),           // BTN_TOUCH (not yet)
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, 0),       // ABS_PRESSURE = 0
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 100),     // ABS_DISTANCE far
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0), // SYN_REPORT
        ])?;
        // Press with given pressure
        self.emit(&[
            InputEvent::new(EvdevEventType::KEY.0, 330, 1),           // BTN_TOUCH
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 24, pressure), // ABS_PRESSURE
            InputEvent::new(EvdevEventType::ABSOLUTE.0, 25, 0),       // ABS_DISTANCE = 0
            InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0), // SYN_REPORT
        ])?;
        Ok(())
    }

    /// Draw SVG using path tracing via svg2polylines for smooth continuous strokes.
    /// Text is converted to glyph paths by usvg before tracing.
    pub fn draw_svg_paths(&mut self, svg_data: &str) -> Result<()> {
        // Use usvg to parse and flatten text → paths
        let mut opt = Options::default();
        opt.fontdb = Arc::new(crate::util::build_fontdb());
        let tree = Tree::from_str(svg_data, &opt)?;
        let write_opt = usvg::WriteOptions::default(); // preserve_text=false → converts text to paths
        let flattened_svg = tree.to_string(&write_opt);

        let polylines = svg2polylines::parse(&flattened_svg, 0.5, true)
            .map_err(|e| anyhow::anyhow!("svg2polylines error: {}", e))?;
        info!("draw_svg_paths: {} polylines", polylines.len());
        self.draw_polylines(&polylines)
    }

    /// Draw paths given as lists of (x, y) virtual coordinates.
    /// No corner detection — skeleton paths are already single-pixel-wide and smooth.
    fn draw_virtual_paths(&mut self, paths: &[Vec<(f32, f32)>]) -> Result<()> {
        const MAX_STEP: f32 = 1.0;
        let mut step_count = 0usize;

        for path in paths {
            if path.len() < 2 {
                continue;
            }

            let start = self.virtual_to_input((path[0].0 as i32, path[0].1 as i32));
            self.pen_up()?;
            sleep(Duration::from_millis(2));
            self.pen_down_at(start)?;
            sleep(Duration::from_millis(2));

            let mut prev = path[0];
            for &pt in &path[1..] {
                self.draw_segment(prev, pt, &mut step_count, MAX_STEP)?;
                prev = pt;
            }

            self.pen_up()?;
            sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    /// Draw SVG by rasterizing to a bitmap, thinning to a 1-pixel skeleton,
    /// then tracing the skeleton to pen strokes.  Produces single-stroke centerlines
    /// for text and shapes without needing to "fill" outlines.
    pub fn draw_svg_centerline(&mut self, svg_data: &str) -> Result<()> {
        use crate::skeleton;
        use crate::util::svg_to_bitmap;

        // Rasterize at 2× for better thinning quality, then scale coords back
        let scale = 2u32;
        let mut bitmap = svg_to_bitmap(svg_data, VIRTUAL_WIDTH * scale, VIRTUAL_HEIGHT * scale)?;
        info!(
            "draw_svg_centerline: rasterized {}×{} bitmap",
            VIRTUAL_WIDTH * scale,
            VIRTUAL_HEIGHT * scale
        );

        skeleton::thin_zhang_suen(&mut bitmap);
        info!("draw_svg_centerline: thinning done");

        let raw_paths = skeleton::trace_skeleton(&bitmap);
        info!("draw_svg_centerline: {} skeleton paths", raw_paths.len());

        // Scale from 2× pixel space back to virtual 768×1024 and smooth out staircase artifacts
        let inv = 1.0 / scale as f32;
        let paths: Vec<Vec<(f32, f32)>> = raw_paths
            .iter()
            .map(|p| {
                let scaled: Vec<(f32, f32)> = p.iter().map(|&(x, y)| (x * inv, y * inv)).collect();
                skeleton::smooth_path(&scaled, 5)
            })
            .collect();

        self.draw_virtual_paths(&paths)
    }

    /// Draw an ink bitmap (e.g. a generated line-art image, already
    /// thresholded) by thinning it to a 1-pixel skeleton, tracing the
    /// skeleton to paths, and scaling those paths uniformly into `target`
    /// (or into the full canvas with a margin when `target` is None).
    /// The bitmap is consumed (thinned in place).
    pub fn draw_bitmap_centerline(&mut self, bitmap: &mut Vec<Vec<bool>>, target: Option<Rect>) -> Result<()> {
        use crate::skeleton;

        skeleton::thin_zhang_suen(bitmap);
        info!("draw_bitmap_centerline: thinning done");

        let raw_paths = skeleton::trace_skeleton(bitmap);
        info!("draw_bitmap_centerline: {} skeleton paths", raw_paths.len());

        let mut paths: Vec<Vec<(f32, f32)>> = raw_paths.iter().map(|p| skeleton::smooth_path(p, 5)).collect();

        // Drop specks: isolated dots and sub-4px fragments come from residual
        // stipple/noise in the generated image and draw as ugly pen taps.
        paths.retain(|p| {
            let len: f32 = p.windows(2).map(|w| {
                let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
                (dx * dx + dy * dy).sqrt()
            }).sum();
            len >= 4.0
        });
        if paths.is_empty() {
            return Err(anyhow::anyhow!("no drawable strokes after tracing"));
        }

        // Ink bounding box across all paths
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in &paths {
            for &(x, y) in p {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        let (bw, bh) = ((max_x - min_x).max(1.0), (max_y - min_y).max(1.0));

        const MARGIN: f32 = 24.0;
        let (rx, ry, rw, rh) = match target {
            Some(r) => (r.x as f32, r.y as f32, r.w as f32, r.h as f32),
            None => (MARGIN, MARGIN, VIRTUAL_WIDTH as f32 - 2.0 * MARGIN, VIRTUAL_HEIGHT as f32 - 2.0 * MARGIN),
        };
        let scale = (rw / bw).min(rh / bh);
        let tx = rx + (rw - bw * scale) / 2.0 - min_x * scale;
        let ty = ry + (rh - bh * scale) / 2.0 - min_y * scale;
        info!(
            "draw_bitmap_centerline: {} strokes, bbox ({:.0}, {:.0}, {:.0}, {:.0}) -> rect ({:.0}, {:.0}, {:.0}, {:.0}), scale {:.3}",
            paths.len(), min_x, min_y, bw, bh, rx, ry, rw, rh, scale
        );

        let clamp_x = |v: f32| v.clamp(0.0, VIRTUAL_WIDTH as f32 - 1.0);
        let clamp_y = |v: f32| v.clamp(0.0, VIRTUAL_HEIGHT as f32 - 1.0);
        let fitted: Vec<Vec<(f32, f32)>> = paths
            .iter()
            .map(|p| p.iter().map(|&(x, y)| (clamp_x(x * scale + tx), clamp_y(y * scale + ty))).collect())
            .collect();

        self.draw_virtual_paths(&fitted)
    }

    /// Draw SVG using path tracing, passing SVG directly to svg2polylines without usvg preprocessing.
    /// Better for geometric shapes (lines, rects, circles), but text won't render.
    pub fn draw_svg_paths_raw(&mut self, svg_data: &str) -> Result<()> {
        let polylines = svg2polylines::parse(svg_data, 0.5, true)
            .map_err(|e| anyhow::anyhow!("svg2polylines error: {}", e))?;
        info!("draw_svg_paths_raw: {} polylines", polylines.len());
        self.draw_polylines(&polylines)
    }

    fn virtual_to_input(&self, (x, y): (i32, i32)) -> (i32, i32) {
        // Planning happens in user space (screenshots are normalized to it);
        // mirror back to panel space when the UI is rotated 180°
        let (x, y) = crate::util::maybe_rot180_virtual((x, y));
        // Swap and normalize the coordinates
        let x_normalized = x as f32 / VIRTUAL_WIDTH as f32;
        let y_normalized = y as f32 / VIRTUAL_HEIGHT as f32;

        match self.device_model {
            DeviceModel::RemarkablePaperPro | DeviceModel::RemarkablePaperProMove => {
                let x_input = (x_normalized * self.max_x_value() as f32) as i32;
                let y_input = (y_normalized * self.max_y_value() as f32) as i32;
                (x_input, y_input)
            }
            _ => {
                let x_input = ((1.0 - y_normalized) * self.max_y_value() as f32) as i32;
                let y_input = (x_normalized * self.max_x_value() as f32) as i32;
                (x_input, y_input)
            }
        }
    }
}

#[cfg(test)]
mod device_geometry_tests {
    use super::*;
    fn pen_for(device_model: DeviceModel) -> Pen {
        Pen { device: None, device_model, injecting: false, report_interval: Duration::ZERO, next_report: Instant::now() }
    }

    #[test]
    fn move_pen_uses_its_own_digitizer_range() {
        let pen = pen_for(DeviceModel::RemarkablePaperProMove);
        assert_eq!((pen.max_x_value(), pen.max_y_value()), (6760, 11960));
        assert_eq!(pen.virtual_to_input((384, 512)), (3380, 5980));
        let pro = pen_for(DeviceModel::RemarkablePaperPro);
        assert_eq!((pro.max_x_value(), pro.max_y_value()), (11180, 15340));
    }

    #[test]
    fn only_tool_transitions_open_or_close_an_injected_stroke() {
        let key = |code, value| InputEvent::new(EvdevEventType::KEY.0, code, value);
        let syn = InputEvent::new(EvdevEventType::SYNCHRONIZATION.0, 0, 0);
        let x = InputEvent::new(EvdevEventType::ABSOLUTE.0, 0, 10);
        assert_eq!(Pen::tool_change(&[x, key(BTN_TOOL_PEN, 1), key(BTN_TOUCH, 0), syn]), Some(true));
        assert_eq!(Pen::tool_change(&[key(BTN_TOUCH, 0), key(BTN_TOOL_RUBBER, 0), syn]), Some(false));
        // Touch alone (the press after a hover) is not a tool transition.
        assert_eq!(Pen::tool_change(&[key(BTN_TOUCH, 1), syn]), None);
        assert_eq!(Pen::tool_change(&[x, syn]), None);
    }

    #[test]
    fn rate_limit_converts_reports_per_second_to_spacing() {
        assert_eq!(Pen::report_interval(0), Duration::ZERO);
        assert_eq!(Pen::report_interval(4000), Duration::from_micros(250));
    }
}
