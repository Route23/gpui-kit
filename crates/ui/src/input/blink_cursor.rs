use std::time::{Duration, Instant};

#[cfg(not(test))]
use gpui::Timer;
use gpui::{App, AsyncApp, Context, Task};

use super::caret::{self, CursorBlinking};

static INTERVAL: Duration = Duration::from_millis(500);
static PAUSE_DELAY: Duration = Duration::from_millis(300);

/// Wait for `duration`, on the timer the blink has always run on.
#[cfg(not(test))]
async fn sleep(duration: Duration, _: &AsyncApp) {
    Timer::after(duration).await;
}

/// In this crate's tests the wait is on the executor's clock instead, which a
/// test can move: what a second of blinking costs is counted without sleeping
/// through it.
#[cfg(test)]
async fn sleep(duration: Duration, cx: &AsyncApp) {
    cx.background_executor().timer(duration).await;
}

/// The time, on the clock [`sleep`] waits on.
fn now(cx: &App) -> Instant {
    cx.background_executor().now()
}

/// To manage the Input cursor blinking.
///
/// [`CursorBlinking::Blink`] toggles `visible` on a 500ms timer and notifies
/// the view, exactly as it always did. [`CursorBlinking::Solid`] runs no timer
/// at all.
///
/// The interpolating styles ([`CursorBlinking::needs_animation`]) stay
/// `visible`, and the element shapes the caret from [`Self::phase`] whenever
/// it paints it. What gets it painted is a timer here too, one step of the
/// fade at a time, for as long as the element keeps asking: see
/// [`Self::request_frame`].
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
    /// The timer of a fade's next step, while one is waiting. See
    /// [`Self::request_frame`].
    frame: Option<Task<()>>,

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
            frame: None,
            _task: Task::ready(()),
        }
    }

    /// Start the blinking in `style`.
    ///
    /// Only [`CursorBlinking::Blink`] runs its timer from here. The others are
    /// solid here and shaped by the element: a fade gets its first step from
    /// here, and each one after that from the element painting the one before
    /// ([`Self::request_frame`]).
    pub fn start(&mut self, style: CursorBlinking, cx: &mut Context<Self>) {
        self.active = true;
        self.style = style;
        self.cycle_start = now(cx);
        // The cycle starts over: a step of the last one is no longer due.
        self.frame = None;
        if style == CursorBlinking::Blink {
            self.blink(self.epoch, cx);
        } else {
            // Kill any timer left over from a previous style.
            self.epoch = 0;
            self.visible = true;
            cx.notify();
            // The first step of a fade is waited for from here, not from the
            // frame this notify is meant to bring. An input hears that it has
            // the focus while the window is being drawn, and a notify from in
            // there does not get the window drawn again: left to that frame,
            // a caret that comes back with the window or the focus would stay
            // solid until something else drew it.
            self.request_frame(cx);
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.active = false;
        self.epoch = 0;
        self.frame = None;
        cx.notify();
    }

    /// How far through the blink cycle we are, 0.0 -> 1.0.
    ///
    /// Starts at 0 (solid) after every keystroke, and holds there while
    /// paused so the caret does not fade mid-word. 0 as well while the cursor
    /// is not running: the frame an input is drawn in before it hears that it
    /// has the focus does not show the caret part way through a fade.
    pub fn phase(&self, cx: &App) -> f32 {
        let Some(elapsed) = self.elapsed(cx) else {
            return 0.;
        };
        // One cycle is on **and** off, so twice the toggle interval.
        let cycle = (2 * INTERVAL).as_nanos();
        (elapsed.as_nanos() % cycle) as f32 / cycle as f32
    }

    /// How long the cycle has been going. `None` while there is none: the
    /// cursor is paused, or not running at all -- the caret is solid then.
    fn elapsed(&self, cx: &App) -> Option<Duration> {
        (self.active && !self.paused).then(|| now(cx).saturating_duration_since(self.cycle_start))
    }

    /// Have the caret painted again when the next step of its fade is due.
    ///
    /// The element calls this each time it paints a caret that can be seen,
    /// and the notify the timer ends in gets it painted again -- a loop for
    /// exactly as long as there is a fade to watch. Not while the blink is
    /// paused (typing: the caret is solid, and resuming notifies by itself),
    /// not after the input lost the focus, and not for a caret that is
    /// scrolled out of view: for those, the step already on its timer is the
    /// last.
    ///
    /// The element used to ask for an animation frame on every frame it drew
    /// instead. That notifies the view being drawn at the rate of the
    /// display: in an application that caches none of its views, the whole
    /// window 60 to 144 times a second for as long as the editor had the
    /// focus, typing or not (dopamine #902).
    pub fn request_frame(&mut self, cx: &mut Context<Self>) {
        if self.frame.is_some() || !self.style.needs_animation() {
            return;
        }
        let Some(elapsed) = self.elapsed(cx) else {
            return;
        };

        let wait = caret::next_fade_frame(elapsed);
        self.frame = Some(cx.spawn(async move |this, cx| {
            sleep(wait, cx).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    this.frame = None;
                    cx.notify();
                })
                .ok();
            }
        }));
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
            sleep(INTERVAL, cx).await;
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
        // Solid until it resumes: no step of a fade is due.
        self.frame = None;
        cx.notify();

        // delay 300ms to start the blinking
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            sleep(PAUSE_DELAY, cx).await;

            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    this.paused = false;
                    // Every style restarts its cycle solid; only `Blink` needs
                    // the timer put back.
                    this.cycle_start = now(cx);
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
    use gpui::{AppContext as _, Entity, Subscription, TestAppContext};
    use std::{cell::Cell, rc::Rc};

    const SECOND: Duration = Duration::from_secs(1);
    const FADES: [CursorBlinking; 3] = [
        CursorBlinking::Smooth,
        CursorBlinking::Phase,
        CursorBlinking::Expand,
    ];

    /// A cursor, and how often it has notified: each time is a frame.
    fn cursor(cx: &mut TestAppContext) -> (Entity<BlinkCursor>, Rc<Cell<usize>>, Subscription) {
        let cursor = cx.new(|_| BlinkCursor::new());
        let notified = Rc::new(Cell::new(0));
        let watch = cx.update(|cx| {
            let notified = notified.clone();
            cx.observe(&cursor, move |_, _| notified.set(notified.get() + 1))
        });
        (cursor, notified, watch)
    }

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

    /// The default, as it always was: on for 500ms, off for 500ms, solid
    /// while keys go down and for 300ms after -- and then off first.
    #[gpui::test]
    fn the_default_blink_is_on_and_off_every_500ms(cx: &mut TestAppContext) {
        let (cursor, notified, _watch) = cursor(cx);
        let visible = |cx: &mut TestAppContext| cursor.read_with(cx, |cursor, _| cursor.visible());
        let ms = Duration::from_millis;

        cursor.update(cx, |cursor, cx| cursor.start(CursorBlinking::Blink, cx));
        cx.run_until_parked();
        assert!(visible(cx));
        notified.set(0);

        for on in [false, true, false, true] {
            cx.executor().advance_clock(ms(499));
            assert_eq!(visible(cx), !on);
            cx.executor().advance_clock(ms(1));
            assert_eq!(visible(cx), on);
        }
        assert_eq!(notified.get(), 4, "two frames a second");

        // A key: solid at once, and for the 300ms of the pause...
        cx.executor().advance_clock(ms(200));
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        cx.run_until_parked();
        assert!(visible(cx));
        notified.set(0);
        cx.executor().advance_clock(ms(299));
        assert!(visible(cx));
        assert_eq!(notified.get(), 0);
        // ...then it blinks again, from off.
        cx.executor().advance_clock(ms(1));
        assert!(!visible(cx));
        cx.executor().advance_clock(ms(500));
        assert!(visible(cx));
        assert_eq!(notified.get(), 2);

        // It never waits for a step of a fade.
        cursor.update(cx, |cursor, cx| {
            cursor.request_frame(cx);
            assert!(cursor.frame.is_none());
        });
    }

    /// The fades read their place in the cycle off the clock: one second
    /// there and back, starting solid.
    #[gpui::test]
    fn the_phase_follows_the_clock(cx: &mut TestAppContext) {
        let (cursor, _, _watch) = cursor(cx);
        let phase = |cx: &mut TestAppContext| cursor.read_with(cx, |cursor, cx| cursor.phase(cx));
        let ms = Duration::from_millis;

        // Not running: solid, however long it has been.
        cx.executor().advance_clock(ms(1234));
        assert_eq!(phase(cx), 0.);

        cursor.update(cx, |cursor, cx| cursor.start(CursorBlinking::Smooth, cx));
        assert_eq!(phase(cx), 0.);
        cx.executor().advance_clock(ms(250));
        assert_eq!(phase(cx), 0.25);
        cx.executor().advance_clock(ms(250));
        assert_eq!(phase(cx), 0.5);
        cx.executor().advance_clock(ms(500));
        assert_eq!(phase(cx), 0.);
        // Days later it is still exact -- seconds in an `f32` are not.
        cx.executor().advance_clock(SECOND * 400_000 + ms(125));
        assert_eq!(phase(cx), 0.125);

        // Paused: solid, and the cycle starts over when it resumes.
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        cx.executor().advance_clock(PAUSE_DELAY - ms(1));
        assert_eq!(phase(cx), 0.);
        cx.executor().advance_clock(ms(1));
        assert_eq!(phase(cx), 0.);
        cx.executor().advance_clock(ms(100));
        assert_eq!(phase(cx), 0.1);

        // Stopped: solid again.
        cursor.update(cx, |cursor, cx| cursor.stop(cx));
        assert_eq!(phase(cx), 0.);
    }

    /// One timer, however often the caret is painted before it fires -- and
    /// no next one unless the caret is painted again. The first step is the
    /// exception: starting asks for it, whether or not a frame follows.
    #[gpui::test]
    fn a_fade_waits_for_one_step_at_a_time(cx: &mut TestAppContext) {
        let ns = Duration::from_nanos;
        for blinking in FADES {
            let (cursor, notified, _watch) = cursor(cx);
            cursor.update(cx, |cursor, cx| {
                cursor.start(blinking, cx);
                assert!(cursor.frame.is_some(), "{blinking:?}: no first step");
            });
            cx.run_until_parked();
            notified.set(0);

            for _ in 0..5 {
                cursor.update(cx, |cursor, cx| cursor.request_frame(cx));
            }
            cx.executor().advance_clock(caret::FADE_TICK - ns(1));
            assert_eq!(notified.get(), 0, "{blinking:?}");
            cx.executor().advance_clock(ns(1));
            assert_eq!(notified.get(), 1, "{blinking:?}");

            // Painted 10ms after that step was due, the next one is still a
            // tick after it -- not a tick after the paint.
            let late = Duration::from_millis(10);
            cx.executor().advance_clock(late);
            cursor.update(cx, |cursor, cx| cursor.request_frame(cx));
            cx.executor().advance_clock(caret::FADE_TICK - late - ns(1));
            assert_eq!(notified.get(), 1, "{blinking:?}");
            cx.executor().advance_clock(ns(1));
            assert_eq!(notified.get(), 2, "{blinking:?}");

            // Nobody painted it since: there is no third.
            cx.executor().advance_clock(SECOND);
            assert_eq!(notified.get(), 2, "{blinking:?}");
        }
    }

    /// Painted every time it notifies -- which is what the element does -- a
    /// fade is 30 frames a second. That is the whole of what it costs.
    #[gpui::test]
    fn a_fade_that_is_watched_is_thirty_frames_a_second(cx: &mut TestAppContext) {
        for blinking in FADES {
            let (cursor, notified, _watch) = cursor(cx);
            // The element: paint, and ask for the next step.
            let _element = cx.update(|cx| {
                cx.observe(&cursor, |cursor, cx| {
                    cursor.update(cx, |cursor, cx| cursor.request_frame(cx))
                })
            });
            cursor.update(cx, |cursor, cx| cursor.start(blinking, cx));
            cx.run_until_parked();
            notified.set(0);

            cx.executor().advance_clock(SECOND);
            assert_eq!(notified.get(), 30, "{blinking:?}");
            cx.executor().advance_clock(SECOND * 60);
            assert_eq!(notified.get(), 30 * 61, "{blinking:?}, a minute later");
        }
    }

    /// While keys go down the caret is solid, and no step is waited for: the
    /// one that was is dropped, and none is taken until the blink resumes.
    #[gpui::test]
    fn a_paused_fade_waits_for_nothing(cx: &mut TestAppContext) {
        for blinking in FADES {
            let (cursor, notified, _watch) = cursor(cx);
            cursor.update(cx, |cursor, cx| {
                cursor.start(blinking, cx);
                cursor.request_frame(cx);
                assert!(cursor.frame.is_some());

                cursor.pause(cx);
                assert!(
                    cursor.frame.is_none(),
                    "{blinking:?}: still waiting for a step"
                );
                cursor.request_frame(cx);
                assert!(
                    cursor.frame.is_none(),
                    "{blinking:?}: took a step while paused"
                );
            });
            cx.run_until_parked();
            notified.set(0);

            cx.executor()
                .advance_clock(PAUSE_DELAY - Duration::from_millis(1));
            assert_eq!(notified.get(), 0, "{blinking:?}");
            // Resuming is one frame by itself: the element paints, and asks.
            cx.executor().advance_clock(Duration::from_millis(1));
            assert_eq!(notified.get(), 1, "{blinking:?}");
            cursor.update(cx, |cursor, cx| {
                cursor.request_frame(cx);
                assert!(cursor.frame.is_some(), "{blinking:?}");
            });
        }
    }

    /// No step for a cursor that is not running, nor for one that has
    /// stopped, nor for the styles that do not fade.
    #[gpui::test]
    fn only_a_running_fade_waits_for_a_step(cx: &mut TestAppContext) {
        let (cursor, notified, _watch) = cursor(cx);
        cursor.update(cx, |cursor, cx| {
            // Never started.
            cursor.request_frame(cx);
            assert!(cursor.frame.is_none());

            for blinking in [CursorBlinking::Blink, CursorBlinking::Solid] {
                cursor.start(blinking, cx);
                let epoch = cursor.epoch;
                cursor.request_frame(cx);
                assert!(cursor.frame.is_none(), "{blinking:?}");
                assert_eq!(cursor.epoch, epoch, "{blinking:?}");
            }

            for blinking in FADES {
                cursor.start(blinking, cx);
                cursor.request_frame(cx);
                assert!(cursor.frame.is_some(), "{blinking:?}");
                // Lost the focus: the step it was waiting for goes with it.
                cursor.stop(cx);
                assert!(cursor.frame.is_none(), "{blinking:?}");
                cursor.request_frame(cx);
                assert!(cursor.frame.is_none(), "{blinking:?}");
            }
        });
        cx.run_until_parked();
        notified.set(0);
        cx.executor().advance_clock(SECOND);
        assert_eq!(notified.get(), 0);
    }

    /// 30 steps a second is a step of a tenth at the most: of the opacity for
    /// the fades, of the height for `Expand`.
    #[test]
    fn the_biggest_step_of_a_fade_is_a_tenth() {
        let cycle = 2 * INTERVAL;
        let steps = (cycle.as_nanos() / caret::FADE_TICK.as_nanos()) as u32;
        assert_eq!(steps, 30);
        let at = |blinking, step: u32| {
            let phase = (caret::FADE_TICK * step).as_secs_f32() / cycle.as_secs_f32();
            caret::appearance(blinking, phase.fract())
        };
        for blinking in FADES {
            let mut biggest = 0f32;
            for step in 0..steps {
                let (a, b) = (at(blinking, step), at(blinking, step + 1));
                biggest = biggest.max((a.0 - b.0).abs()).max((a.1 - b.1).abs());
            }
            assert!(biggest > 0.05, "{blinking:?}: {biggest}");
            assert!(biggest <= 0.1 + 1e-4, "{blinking:?}: {biggest}");
        }
    }

    /// What the caret costs an editor in a window: how often the window is
    /// drawn for it (dopamine #902).
    mod frames {
        use super::*;
        use crate::{
            Root,
            input::{CaretAnimation, Input, InputState, element::display_frames},
            scroll::ScrollbarShow,
        };
        use gpui::{
            IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext, Window, div,
            px, size,
        };
        use std::cell::RefCell;

        const ALL: [CursorBlinking; 5] = [
            CursorBlinking::Blink,
            CursorBlinking::Smooth,
            CursorBlinking::Phase,
            CursorBlinking::Expand,
            CursorBlinking::Solid,
        ];

        /// The view the editor sits in. Nothing in the window is cached, so
        /// every frame renders it -- which is what gets counted.
        struct Host {
            input: Entity<InputState>,
            frames: Rc<Cell<usize>>,
        }

        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.frames.set(self.frames.get() + 1);
                div().size_full().child(Input::new(&self.input).h_full())
            }
        }

        /// An editor with the focus, in the window in front, on a display that
        /// refreshes `hz` times a second.
        struct Editor<'a> {
            cx: &'a mut VisualTestContext,
            input: Entity<InputState>,
            frames: Rc<Cell<usize>>,
            hz: u32,
            /// How many times the display has refreshed, and when the last
            /// one was.
            refreshes: u32,
            at: Duration,
        }

        impl<'a> Editor<'a> {
            fn open(blinking: CursorBlinking, hz: u32, cx: &'a mut TestAppContext) -> Self {
                Self::open_with(blinking, CaretAnimation::Off, hz, cx)
            }

            fn open_with(
                blinking: CursorBlinking,
                slide: CaretAnimation,
                hz: u32,
                cx: &'a mut TestAppContext,
            ) -> Self {
                if !cx.has_global::<crate::Theme>() {
                    cx.update(crate::init);
                }
                // Whatever the window before this one still had asked for.
                display_frames::take();

                let frames = Rc::new(Cell::new(0));
                let slot = Rc::new(RefCell::new(None));
                let (_, cx) = cx.add_window_view({
                    let (frames, slot) = (frames.clone(), slot.clone());
                    move |window, cx| {
                        let input = cx.new(|cx| {
                            InputState::new(window, cx)
                                .code_editor("text")
                                .cursor_blinking(blinking)
                                .caret_animation(slide)
                                // Its fade after a scroll is a timer of its
                                // own, on the wall clock: not counted here.
                                .scrollbar_style(
                                    Some(ScrollbarShow::Always),
                                    true,
                                    true,
                                    0,
                                    false,
                                    false,
                                )
                                .default_value("let caret = blink();\n".repeat(120))
                        });
                        *slot.borrow_mut() = Some(input.clone());
                        let host = cx.new(|_| Host { input, frames });
                        Root::new(host, window, cx)
                    }
                });
                let input = slot.take().unwrap();

                // A dozen rows: a frame is cheap, and the caret can be
                // scrolled out of view.
                cx.simulate_resize(size(px(640.), px(280.)));
                cx.update(|window, _| window.activate_window());
                cx.run_until_parked();
                input.update_in(cx, |input, window, cx| input.focus(window, cx));
                cx.run_until_parked();

                Self {
                    cx,
                    input,
                    frames,
                    hz,
                    refreshes: 0,
                    at: Duration::ZERO,
                }
            }

            /// Let `duration` go by, one frame of the display after another,
            /// and count the frames the window drew.
            ///
            /// Timers come due in between. On a frame, gpui notifies the views
            /// that asked for it and draws the window if that left it dirty --
            /// a test window draws as soon as it is dirty, so here the notify
            /// is all there is to do.
            fn pass(&mut self, duration: Duration) -> usize {
                let before = self.frames.get();
                let end = self.at + duration;
                loop {
                    let due = SECOND * (self.refreshes + 1) / self.hz;
                    if due > end {
                        break;
                    }
                    self.cx.executor().advance_clock(due - self.at);
                    self.at = due;
                    self.refreshes += 1;
                    for view in display_frames::take() {
                        self.cx.update(|_, cx| cx.notify(view));
                    }
                }
                self.cx.executor().advance_clock(end - self.at);
                self.at = end;
                self.frames.get() - before
            }

            fn key(&mut self, key: &str) {
                self.cx.simulate_keystrokes(key);
            }

            /// A second of typing, a key every 100ms: how many frames.
            fn type_for_a_second(&mut self) -> usize {
                let before = self.frames.get();
                for _ in 0..10 {
                    self.key("a");
                    self.pass(Duration::from_millis(100));
                }
                self.frames.get() - before
            }
        }

        impl Drop for Editor<'_> {
            fn drop(&mut self) {
                // A test that failed is already on its way out.
                if !std::thread::panicking() {
                    self.cx.update(|window, _| window.remove_window());
                    self.cx.run_until_parked();
                }
            }
        }

        /// With nothing going on, a fade is 30 frames a second, whatever the
        /// display. It used to ask for a frame of the display on every frame
        /// it drew: 60, 120 or 144 windows a second, for as long as the editor
        /// had the focus.
        #[gpui::test]
        fn an_idle_fade_is_thirty_frames_a_second(cx: &mut TestAppContext) {
            for blinking in FADES {
                for hz in [60, 120, 144] {
                    let mut editor = Editor::open(blinking, hz, cx);
                    assert_eq!(editor.pass(SECOND), 30, "{blinking:?} at {hz}Hz");
                    // ...and the same the second after.
                    assert_eq!(editor.pass(SECOND), 30, "{blinking:?} at {hz}Hz");
                }
            }
        }

        /// The default is what it was: the 500ms timer, so two frames a
        /// second. A solid caret is none.
        #[gpui::test]
        fn the_default_blink_is_two_frames_a_second(cx: &mut TestAppContext) {
            for hz in [60, 144] {
                let mut editor = Editor::open(CursorBlinking::Blink, hz, cx);
                assert_eq!(editor.pass(SECOND), 2, "at {hz}Hz");
                assert_eq!(editor.pass(SECOND * 10), 20, "at {hz}Hz");
                drop(editor);

                let mut editor = Editor::open(CursorBlinking::Solid, hz, cx);
                assert_eq!(editor.pass(SECOND * 10), 0, "at {hz}Hz");
            }
        }

        /// The blink is paused while keys go down (and for 300ms after the
        /// last one): the caret is solid, and no frame is for it. Typing costs
        /// a fading caret exactly what it costs a solid one.
        #[gpui::test]
        fn typing_draws_no_frame_for_the_blink(cx: &mut TestAppContext) {
            let solid = Editor::open(CursorBlinking::Solid, 60, cx).type_for_a_second();
            assert!(solid >= 10, "a frame a key at least: {solid}");
            for blinking in ALL {
                let frames = Editor::open(blinking, 60, cx).type_for_a_second();
                assert_eq!(frames, solid, "{blinking:?}");
            }
        }

        /// Once the pause is over the fade starts again, at its own rate.
        #[gpui::test]
        fn a_fade_picks_up_after_the_pause(cx: &mut TestAppContext) {
            /// One key, and the frames until just before the blink resumes.
            fn a_key_and_the_pause(editor: &mut Editor) -> usize {
                editor.pass(Duration::from_millis(250));
                let before = editor.frames.get();
                editor.key("a");
                editor.pass(PAUSE_DELAY - Duration::from_millis(1));
                editor.frames.get() - before
            }

            // What the key costs by itself.
            let floor = a_key_and_the_pause(&mut Editor::open(CursorBlinking::Solid, 60, cx));

            for (blinking, a_second) in [
                (CursorBlinking::Blink, 2),
                (CursorBlinking::Smooth, 30),
                (CursorBlinking::Phase, 30),
                (CursorBlinking::Expand, 30),
                (CursorBlinking::Solid, 0),
            ] {
                let mut editor = Editor::open(blinking, 60, cx);
                assert_eq!(
                    a_key_and_the_pause(&mut editor),
                    floor,
                    "{blinking:?}, paused"
                );
                // The blink resumes here...
                editor.pass(Duration::from_millis(1));
                // ...and this is its second.
                assert_eq!(editor.pass(SECOND), a_second, "{blinking:?}");
            }
        }

        /// A caret that is scrolled out of view is not faded: there is nothing
        /// to see change.
        #[gpui::test]
        fn a_caret_out_of_view_is_not_faded(cx: &mut TestAppContext) {
            for blinking in FADES {
                let mut editor = Editor::open(blinking, 60, cx);
                editor
                    .input
                    .update(editor.cx, |input, cx| input.scroll_to_row(60, cx));
                editor.cx.run_until_parked();
                let visible = editor.input.read_with(editor.cx, |input, _| {
                    input.last_layout.as_ref().unwrap().visible_range.clone()
                });
                assert!(visible.start > 30, "not scrolled: {visible:?}");
                // The step that was already on its timer still lands...
                assert!(editor.pass(caret::FADE_TICK) <= 1, "{blinking:?}");
                // ...and that is the last one.
                assert_eq!(editor.pass(SECOND), 0, "{blinking:?}");

                // Back in view, it fades again.
                editor
                    .input
                    .update(editor.cx, |input, cx| input.scroll_to_row(0, cx));
                editor.cx.run_until_parked();
                assert_eq!(editor.pass(SECOND), 30, "{blinking:?}, back in view");
            }
        }

        /// Without the focus, or with the window in the back, there is no
        /// caret and nothing is drawn for one.
        #[gpui::test]
        fn a_caret_that_is_not_showing_costs_nothing(cx: &mut TestAppContext) {
            for blinking in ALL {
                let mut editor = Editor::open(blinking, 60, cx);
                editor.cx.update(|window, _| window.blur());
                editor.cx.run_until_parked();
                // Whatever was already on its way (half a second covers it).
                editor.pass(SECOND / 2);
                assert_eq!(editor.pass(SECOND), 0, "{blinking:?}, without the focus");
                drop(editor);

                let mut editor = Editor::open(blinking, 60, cx);
                editor.cx.deactivate_window();
                editor.pass(SECOND / 2);
                assert_eq!(editor.pass(SECOND), 0, "{blinking:?}, in the back");
            }
        }

        /// Back in front, or with the focus back, the caret blinks again.
        ///
        /// The input hears that it has the focus while the window is being
        /// drawn, where a notify does not get the window drawn again -- so
        /// a fade must not wait for that frame to be asked for its first step.
        #[gpui::test]
        fn a_caret_that_shows_again_blinks_again(cx: &mut TestAppContext) {
            for (blinking, a_second) in [
                (CursorBlinking::Blink, 2),
                (CursorBlinking::Smooth, 30),
                (CursorBlinking::Phase, 30),
                (CursorBlinking::Expand, 30),
                (CursorBlinking::Solid, 0),
            ] {
                // The window goes to the back and comes to the front again.
                let mut editor = Editor::open(blinking, 60, cx);
                editor.pass(SECOND / 4);
                editor.cx.deactivate_window();
                editor.pass(SECOND);
                editor.cx.update(|window, _| window.activate_window());
                editor.cx.run_until_parked();
                assert_eq!(editor.pass(SECOND), a_second, "{blinking:?}, back in front");
                assert_eq!(editor.pass(SECOND), a_second, "{blinking:?}, back in front");
                drop(editor);

                // The focus goes elsewhere and comes back.
                let mut editor = Editor::open(blinking, 60, cx);
                editor.pass(SECOND / 4);
                editor.cx.update(|window, _| window.blur());
                editor.cx.run_until_parked();
                editor.pass(SECOND);
                editor
                    .input
                    .clone()
                    .update_in(editor.cx, |input, window, cx| input.focus(window, cx));
                editor.cx.run_until_parked();
                assert_eq!(editor.pass(SECOND), a_second, "{blinking:?}, focus back");
                drop(editor);

                // The same when the handle is given the focus and nothing
                // else is said (the Tab key, a host focusing a pane): hearing
                // of it is all there is to start the blink.
                let mut editor = Editor::open(blinking, 60, cx);
                editor.pass(SECOND / 4);
                editor.cx.update(|window, _| window.blur());
                editor.cx.run_until_parked();
                editor.pass(SECOND);
                let handle = editor
                    .input
                    .read_with(editor.cx, |input, _| input.focus_handle.clone());
                editor.cx.update(|window, _| handle.focus(window));
                editor.cx.run_until_parked();
                assert_eq!(
                    editor.pass(SECOND),
                    a_second,
                    "{blinking:?}, focus back on the handle"
                );
            }
        }

        /// The setting changes under an editor that has the focus: from there
        /// on it costs what the new style costs, with nothing left running
        /// from the old one.
        #[gpui::test]
        fn a_new_style_takes_over_from_the_old(cx: &mut TestAppContext) {
            let mut editor = Editor::open(CursorBlinking::Smooth, 60, cx);
            assert_eq!(editor.pass(SECOND), 30);
            for (blinking, a_second) in [
                (CursorBlinking::Blink, 2),
                (CursorBlinking::Expand, 30),
                (CursorBlinking::Solid, 0),
                (CursorBlinking::Phase, 30),
                (CursorBlinking::Blink, 2),
                (CursorBlinking::Solid, 0),
                (CursorBlinking::Smooth, 30),
            ] {
                // Part way through whatever the old style was doing.
                editor.pass(Duration::from_millis(120));
                editor
                    .input
                    .clone()
                    .update_in(editor.cx, |input, window, cx| {
                        input.set_cursor_blinking(blinking, window, cx)
                    });
                editor.cx.run_until_parked();
                assert_eq!(editor.pass(SECOND), a_second, "to {blinking:?}");
                assert_eq!(editor.pass(SECOND), a_second, "to {blinking:?}");
            }
        }

        /// A slide is another matter: it lasts 90ms and has to be smooth, so
        /// it still asks the display for its frames. A fade never does.
        #[gpui::test]
        fn only_a_slide_asks_the_display_for_frames(cx: &mut TestAppContext) {
            let mut editor = Editor::open_with(CursorBlinking::Solid, CaretAnimation::On, 60, cx);
            editor.pass(SECOND);
            assert!(display_frames::take().is_empty(), "a caret at rest");
            editor.key("right");
            assert!(!display_frames::take().is_empty(), "a slide");
            drop(editor);

            for blinking in FADES {
                let mut editor = Editor::open(blinking, 60, cx);
                assert!(display_frames::take().is_empty(), "{blinking:?}");
                editor.pass(Duration::from_millis(10));
                editor.key("a");
                assert!(display_frames::take().is_empty(), "{blinking:?}, typing");
            }
        }
    }
}
