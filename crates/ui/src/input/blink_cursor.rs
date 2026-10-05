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
    /// Started and not stopped since: the input has the focus, so there is a
    /// caret on screen to blink. See [`Self::pause`].
    active: bool,
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
            active: false,
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
        self.active = true;
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
        self.active = false;
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
    ///
    /// **Only while it is running.** Moving the caret pauses the blink, and the
    /// caret is moved by more than typing: setting the text puts it back at
    /// the start. Resuming after the pause used to start the 500ms timer
    /// whether or not anything had been blinking -- so an editor that had
    /// just been given its text, or one that was edited from outside while
    /// another pane had the focus, repainted the window twice a second for a
    /// caret nobody could see, until it was focused and blurred once
    /// (dopamine #930). With "reduce motion" on as well: the style it resumed
    /// in is the one the last `start` was given, and there had been none.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};

    /// Nothing is blinking until the input is focused, so there is nothing to
    /// pause -- and nothing to resume, which is what started a timer for a
    /// caret that was not on screen (dopamine #930).
    #[gpui::test]
    fn pausing_a_cursor_that_is_not_running_does_nothing(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());
        cursor.update(cx, |cursor, cx| {
            cursor.pause(cx);
            assert!(!cursor.paused);
            assert_eq!(cursor.epoch, 0, "a resume was scheduled");
        });

        // Running: the pause takes, and schedules the resume.
        cursor.update(cx, |cursor, cx| {
            cursor.start(CursorBlinking::Blink, cx);
            let before = cursor.epoch;
            cursor.pause(cx);
            assert!(cursor.paused);
            assert!(cursor.visible());
            assert!(cursor.epoch > before);
        });

        // Stopped (the input lost the focus): back to nothing.
        cursor.update(cx, |cursor, cx| {
            cursor.stop(cx);
            cursor.paused = false;
            cursor.pause(cx);
            assert!(!cursor.paused);
            assert_eq!(cursor.epoch, 0);
        });
    }

    /// A solid caret runs no timer, focused or not.
    #[gpui::test]
    fn a_solid_caret_schedules_nothing(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());
        cursor.update(cx, |cursor, cx| {
            cursor.start(CursorBlinking::Solid, cx);
            assert_eq!(cursor.epoch, 0);
            assert!(cursor.visible());
        });
    }
}
