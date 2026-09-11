use std::time::{Duration, Instant};

use gpui::{Context, Task, Timer};

use super::caret::CursorBlinking;

static INTERVAL: Duration = Duration::from_millis(500);
static PAUSE_DELAY: Duration = Duration::from_millis(300);

/// To manage the Input cursor blinking.
///
/// [`CursorBlinking::Blink`] toggles `visible` on a 500ms timer and notifies
/// the view, exactly as it always did.
///
/// The interpolating styles ([`CursorBlinking::needs_animation`]) are **not**
/// driven from here -- a 60Hz timer would repaint the whole window. They stay
/// `visible` and the element reads [`Self::phase`] every animation frame
/// instead. [`CursorBlinking::Solid`] runs no timer at all.
pub(crate) struct BlinkCursor {
    visible: bool,
    paused: bool,
    epoch: usize,
    /// The style the last [`Self::start`] was given, so [`Self::pause`] knows
    /// whether to resume the timer.
    style: CursorBlinking,
    /// When the current cycle began. Typing resets it, so the caret is at
    /// full strength right after a keystroke.
    cycle_start: Instant,

    _task: Task<()>,
}

impl BlinkCursor {
    pub fn new() -> Self {
        Self {
            visible: false,
            paused: false,
            epoch: 0,
            style: CursorBlinking::default(),
            cycle_start: Instant::now(),
            _task: Task::ready(()),
        }
    }

    /// Start the blinking in `style`.
    ///
    /// Only [`CursorBlinking::Blink`] runs the timer; the others are solid
    /// here and shaped by the element.
    pub fn start(&mut self, style: CursorBlinking, cx: &mut Context<Self>) {
        self.style = style;
        self.cycle_start = Instant::now();
        if style == CursorBlinking::Blink {
            self.blink(self.epoch, cx);
        } else {
            // Kill any timer left over from a previous style.
            self.epoch = 0;
            self.visible = true;
            cx.notify();
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.epoch = 0;
        cx.notify();
    }

    /// How far through the blink cycle we are, 0.0 -> 1.0.
    ///
    /// Starts at 0 (solid) after every keystroke, and holds there while
    /// paused so the caret does not fade mid-word.
    pub fn phase(&self) -> f32 {
        if self.paused {
            return 0.;
        }
        // One cycle is on **and** off, so twice the toggle interval.
        let cycle = 2. * INTERVAL.as_secs_f32();
        (self.cycle_start.elapsed().as_secs_f32() / cycle).fract()
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch += 1;
        self.epoch
    }

    fn blink(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if self.paused || epoch != self.epoch {
            self.visible = true;
            return;
        }

        self.visible = !self.visible;
        cx.notify();

        // Schedule the next blink
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            Timer::after(INTERVAL).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.blink(epoch, cx)).ok();
            }
        });
    }

    pub fn visible(&self) -> bool {
        // Keep showing the cursor if paused
        self.paused || self.visible
    }

    /// Pause the blinking, and delay 300ms to resume the blinking.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.paused = true;
        self.visible = true;
        cx.notify();

        // delay 300ms to start the blinking
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            Timer::after(PAUSE_DELAY).await;

            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    this.paused = false;
                    // Every style restarts its cycle solid; only `Blink` needs
                    // the timer put back.
                    this.cycle_start = Instant::now();
                    if this.style == CursorBlinking::Blink {
                        this.blink(epoch, cx);
                    } else {
                        cx.notify();
                    }
                })
                .ok();
            }
        });
    }
}
