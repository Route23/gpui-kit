use std::time::{Duration, Instant};

#[cfg(not(test))]
use gpui::Timer;
use gpui::{App, AsyncApp, Context, Task};

use super::caret::{self, CursorBlinking};

static INTERVAL: Duration = Duration::from_millis(500);
static PAUSE_DELAY: Duration = Duration::from_millis(300);
/// How long a cursor that the element started goes on running after the caret
/// was last painted: see [`BlinkCursor::request_frame`].
///
/// A caret that is still showing is painted more often than this -- a step of
/// a fade is a thirtieth of a second, the pause after a key 300ms.
static PAINTED_FOR: Duration = Duration::from_secs(1);

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

/// Wait until `due`, a time on that clock.
async fn sleep_until(due: Instant, cx: &AsyncApp) {
    sleep(
        due.saturating_duration_since(cx.background_executor().now()),
        cx,
    )
    .await;
}

/// How long the frame that asks for a step still takes to draw and present,
/// as a test plays it. See [`BlinkCursor::wait_for_step`].
///
/// With a display behind the window, the task that waits for the step is not
/// polled before that. A test polls it the moment it is spawned -- its clock
/// only moves once every task has had its turn -- so the time is put in by
/// hand.
#[cfg(test)]
mod rest_of_frame {
    use std::{cell::Cell, time::Duration};

    thread_local! {
        static REST: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }

    pub(super) fn get() -> Duration {
        REST.with(Cell::get)
    }

    /// `ask`, in a frame that takes `rest` more.
    pub(super) fn of<R>(rest: Duration, ask: impl FnOnce() -> R) -> R {
        REST.with(|cell| cell.set(rest));
        let asked = ask();
        REST.with(|cell| cell.set(Duration::ZERO));
        asked
    }
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
    /// caret on screen to blink. See [`Self::running`].
    active: bool,
    /// Started by the element painting a fade, with nothing said through
    /// [`Self::start`] -- and nothing will say that it has stopped either.
    /// See [`Self::request_frame`].
    by_paint: bool,
    /// When the element last painted a fade that can be seen.
    painted: Instant,
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
            by_paint: false,
            painted: Instant::now(),
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
        self.by_paint = false;
        self.style = style;
        self.cycle_start = now(cx);
        // The cycle starts over: a step of the last one is no longer due.
        self.frame = None;
        if style == CursorBlinking::Blink {
            // A blink starts lit. `blink` flips `visible`, and a cursor that has
            // run before is left with it true (a pause, or the half it was
            // stopped in) -- so an input that got the focus back showed its
            // caret for one frame and then hid it for the first 500ms. Moving
            // the caret used to cover for this: a click moves it before the
            // input hears that it has the focus, and the pause that came with
            // the move showed it. A pause no longer takes on a cursor that is
            // not running (dopamine #930), so the start has to do it itself.
            self.visible = false;
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
            self.wait_for_step(cx);
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.active = false;
        self.by_paint = false;
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
        (self.running(cx) && !self.paused)
            .then(|| now(cx).saturating_duration_since(self.cycle_start))
    }

    /// Whether there is a caret on screen to blink: started, and not stopped
    /// since.
    ///
    /// A cursor that the element started ([`Self::request_frame`]) has nobody
    /// to stop it. It is running for as long as its caret keeps being
    /// painted, and no longer.
    fn running(&self, cx: &App) -> bool {
        self.active
            && (!self.by_paint || now(cx).saturating_duration_since(self.painted) < PAINTED_FOR)
    }

    /// Have the caret painted again when the next step of its fade is due.
    ///
    /// The element calls this each time it paints a caret that can be seen,
    /// with the style it painted it in, and the notify the timer ends in gets
    /// it painted again -- a loop for exactly as long as there is a fade to
    /// watch. The element does not ask for a caret it does not paint (the
    /// input lost the focus, the window went to the back) or that is scrolled
    /// out of view, and nothing is waited for while the blink is paused
    /// (typing: the caret is solid, and resuming notifies by itself): for
    /// those, the step already on its timer is the last.
    ///
    /// **A fade that is painted runs, whether the cursor was started or
    /// not.** [`Self::start`] and [`Self::stop`] come from the state's focus
    /// listeners, and those are on the window the state was made with. A host
    /// that makes it with another window in hand than the one it draws it in
    /// (a settings window that rebuilds the editors of the main one, in
    /// dopamine) gets a state that hears of the focus in neither -- and a
    /// cursor that is not running has no phase and waits for no step, so that
    /// caret stayed solid for good. The element only paints a caret for the
    /// input that has the focus in the window it is drawing, so being asked
    /// from there is as good as being started: a cursor that is not running
    /// starts here. Nobody is going to stop that one, so it runs for as long
    /// as it keeps being asked, and [`PAINTED_FOR`] after the last time.
    ///
    /// Only a fade, which is stepped by the paints it brings about and ends
    /// with them. `Blink` is switched by a timer that runs whether anything is
    /// painted or not, and with nothing to stop it that would be dopamine
    /// #930 again: it is left to [`Self::start`].
    ///
    /// The element used to ask for an animation frame on every frame it drew
    /// instead. That notifies the view being drawn at the rate of the
    /// display: in an application that caches none of its views, the whole
    /// window 60 to 144 times a second for as long as the editor had the
    /// focus, typing or not (dopamine #902).
    pub fn request_frame(&mut self, style: CursorBlinking, cx: &mut Context<Self>) {
        if !style.needs_animation() {
            return;
        }
        if !self.running(cx) {
            // The frame being painted shows it solid (`phase`), and the cycle
            // begins from there.
            self.start(style, cx);
            self.by_paint = true;
        }
        self.painted = now(cx);
        self.wait_for_step(cx);
    }

    /// Wait for the next step of the fade, unless one is being waited for.
    ///
    /// When the step is due is settled here, while the caret is painted, and
    /// the wait is for what is left of that when the task is first polled --
    /// which is not before the frame that asked has been drawn and presented.
    /// Counted from there, every step would come that much late, by a few
    /// milliseconds that are not the same twice: on a display whose refresh
    /// the steps fall just ahead of, some would make that refresh and some
    /// the one after.
    fn wait_for_step(&mut self, cx: &mut Context<Self>) {
        if self.frame.is_some() || !self.style.needs_animation() {
            return;
        }
        let Some(elapsed) = self.elapsed(cx) else {
            return;
        };

        let due = self.cycle_start + elapsed + caret::next_fade_frame(elapsed);
        #[cfg(test)]
        let rest = rest_of_frame::get();
        self.frame = Some(cx.spawn(async move |this, cx| {
            // Where a window would still be drawing the frame that asked.
            #[cfg(test)]
            sleep(rest, cx).await;
            sleep_until(due, cx).await;
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
        if !self.running(cx) {
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
            cursor.request_frame(CursorBlinking::Blink, cx);
            assert!(cursor.frame.is_none());
        });
    }

    /// A blink starts lit, however the cursor was left: stopped in a lit half,
    /// stopped in a dark one, or started twice (an input that is focused by
    /// hand hears of the focus as well). Left to the flip alone, a click back
    /// into an input hid its caret for the first 500ms.
    #[gpui::test]
    fn a_blink_starts_lit_whatever_it_was_left_as(cx: &mut TestAppContext) {
        let (cursor, _notified, _watch) = cursor(cx);
        let visible = |cx: &mut TestAppContext| cursor.read_with(cx, |cursor, _| cursor.visible());
        let ms = Duration::from_millis;
        let start = |cx: &mut TestAppContext| {
            cursor.update(cx, |cursor, cx| cursor.start(CursorBlinking::Blink, cx));
            cx.run_until_parked();
        };
        let stop = |cx: &mut TestAppContext| {
            cursor.update(cx, |cursor, cx| cursor.stop(cx));
            cx.run_until_parked();
        };

        // Stopped in a lit half (the usual blur: the caret was showing).
        start(cx);
        assert!(visible(cx));
        cx.executor().advance_clock(ms(200));
        stop(cx);
        start(cx);
        assert!(visible(cx), "hidden for the first half after the focus came back");
        // ...and the first half is a whole one: 500ms lit, then dark.
        cx.executor().advance_clock(ms(499));
        assert!(visible(cx));
        cx.executor().advance_clock(ms(1));
        assert!(!visible(cx));

        // Stopped in a dark half.
        stop(cx);
        start(cx);
        assert!(visible(cx));

        // Started twice running: lit, not flipped back to dark.
        stop(cx);
        start(cx);
        start(cx);
        assert!(visible(cx));
        cx.executor().advance_clock(ms(499));
        assert!(visible(cx), "a timer of the first start cut the half short");
        cx.executor().advance_clock(ms(1));
        assert!(!visible(cx));

        // A caret moved before the input heard of the focus (a click): the
        // pause does nothing, and the start still shows it -- for the whole of
        // its first half, not until a resume the pause would have set up.
        start(cx); // lit when it stops, as an input that lost the focus is
        stop(cx);
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        start(cx);
        assert!(visible(cx));
        cx.executor().advance_clock(ms(499));
        assert!(visible(cx));
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
                cursor.update(cx, |cursor, cx| cursor.request_frame(blinking, cx));
            }
            cx.executor().advance_clock(caret::FADE_TICK - ns(1));
            assert_eq!(notified.get(), 0, "{blinking:?}");
            cx.executor().advance_clock(ns(1));
            assert_eq!(notified.get(), 1, "{blinking:?}");

            // Painted 10ms after that step was due, the next one is still a
            // tick after it -- not a tick after the paint.
            let late = Duration::from_millis(10);
            cx.executor().advance_clock(late);
            cursor.update(cx, |cursor, cx| cursor.request_frame(blinking, cx));
            cx.executor().advance_clock(caret::FADE_TICK - late - ns(1));
            assert_eq!(notified.get(), 1, "{blinking:?}");
            cx.executor().advance_clock(ns(1));
            assert_eq!(notified.get(), 2, "{blinking:?}");

            // Nobody painted it since: there is no third.
            cx.executor().advance_clock(SECOND);
            assert_eq!(notified.get(), 2, "{blinking:?}");
        }
    }

    /// The wait for a step counts from the paint that asked for it, not from
    /// when the task that waits is first polled -- which is not before the
    /// frame that asked is drawn and presented. The step is on the grid
    /// however long that takes; and when it takes longer than there was left
    /// to wait, the step comes as soon as the frame is out of the way.
    #[gpui::test]
    fn a_step_does_not_wait_for_the_rest_of_its_frame_as_well(cx: &mut TestAppContext) {
        let (ms, ns) = (Duration::from_millis, Duration::from_nanos);
        for blinking in FADES {
            let (cursor, notified, _watch) = cursor(cx);
            // Painted 2ms after a step, in a frame that takes `rest` more.
            let paint = |rest: Duration, cx: &mut TestAppContext| {
                cx.executor().advance_clock(ms(2));
                rest_of_frame::of(rest, || {
                    cursor.update(cx, |cursor, cx| cursor.request_frame(blinking, cx))
                });
            };

            cursor.update(cx, |cursor, cx| cursor.start(blinking, cx));
            cx.run_until_parked();
            notified.set(0);
            cx.executor().advance_clock(caret::FADE_TICK);
            assert_eq!(notified.get(), 1, "{blinking:?}");

            for (step, rest) in [ms(0), ms(3), ms(7), ms(20)].into_iter().enumerate() {
                paint(rest, cx);
                cx.executor()
                    .advance_clock(caret::FADE_TICK - ms(2) - ns(1));
                assert_eq!(notified.get(), 1 + step, "{blinking:?}, {rest:?}: early");
                cx.executor().advance_clock(ns(1));
                assert_eq!(notified.get(), 2 + step, "{blinking:?}, {rest:?}: late");
            }

            // 40ms is longer than a tick: the step was due before the frame
            // was over.
            paint(ms(40), cx);
            cx.executor().advance_clock(ms(40) - ns(1));
            assert_eq!(notified.get(), 5, "{blinking:?}: before its frame was over");
            cx.executor().advance_clock(ns(1));
            assert_eq!(notified.get(), 6, "{blinking:?}: waited again");
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
                cx.observe(&cursor, move |cursor, cx| {
                    cursor.update(cx, |cursor, cx| cursor.request_frame(blinking, cx))
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
                cursor.request_frame(blinking, cx);
                assert!(cursor.frame.is_some());

                cursor.pause(cx);
                assert!(
                    cursor.frame.is_none(),
                    "{blinking:?}: still waiting for a step"
                );
                cursor.request_frame(blinking, cx);
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
                cursor.request_frame(blinking, cx);
                assert!(cursor.frame.is_some(), "{blinking:?}");
            });
        }
    }

    /// No step for the styles that do not fade, and being painted does not
    /// start them: `Blink` is switched by a timer that runs whether the caret
    /// is painted or not, and only `start` sets it going. A fade that has
    /// stopped waits for no step either.
    #[gpui::test]
    fn only_a_fade_waits_for_a_step(cx: &mut TestAppContext) {
        let (cursor, notified, _watch) = cursor(cx);
        cursor.update(cx, |cursor, cx| {
            for blinking in [CursorBlinking::Blink, CursorBlinking::Solid] {
                // Never started, or stopped.
                cursor.request_frame(blinking, cx);
                assert!(!cursor.active, "{blinking:?}: started");
                assert!(cursor.frame.is_none(), "{blinking:?}");
                assert_eq!(cursor.epoch, 0, "{blinking:?}: a timer");

                cursor.start(blinking, cx);
                let epoch = cursor.epoch;
                cursor.request_frame(blinking, cx);
                assert!(cursor.frame.is_none(), "{blinking:?}");
                assert_eq!(cursor.epoch, epoch, "{blinking:?}");
                cursor.stop(cx);
            }

            for blinking in FADES {
                cursor.start(blinking, cx);
                cursor.request_frame(blinking, cx);
                assert!(cursor.frame.is_some(), "{blinking:?}");
                // Lost the focus: the step it was waiting for goes with it.
                cursor.stop(cx);
                assert!(cursor.frame.is_none(), "{blinking:?}");
            }
        });
        cx.run_until_parked();
        notified.set(0);
        cx.executor().advance_clock(SECOND);
        assert_eq!(notified.get(), 0);
    }

    /// A fade that is painted runs, whether the cursor was started or not.
    ///
    /// A state hears of the focus on the window it was made with. Made with
    /// another one than it is drawn in, it hears nothing, nobody starts its
    /// cursor -- and that caret used to be painted solid for good.
    #[gpui::test]
    fn painting_a_fade_starts_a_cursor_nobody_started(cx: &mut TestAppContext) {
        let ms = Duration::from_millis;
        for blinking in FADES {
            let (cursor, notified, _watch) = cursor(cx);
            let phase =
                |cx: &mut TestAppContext| cursor.read_with(cx, |cursor, cx| cursor.phase(cx));
            // The element: paint -- which reads the phase -- and ask.
            let element = cx.update(|cx| {
                cx.observe(&cursor, move |cursor, cx| {
                    cursor.update(cx, |cursor, cx| cursor.request_frame(blinking, cx))
                })
            });

            // The frame it is first painted in is solid, whenever that is...
            cx.executor().advance_clock(ms(1234));
            assert_eq!(phase(cx), 0., "{blinking:?}");
            cursor.update(cx, |cursor, cx| {
                cursor.request_frame(blinking, cx);
                assert!(cursor.by_paint, "{blinking:?}");
                assert!(cursor.frame.is_some(), "{blinking:?}: no first step");
            });
            cx.run_until_parked();
            notified.set(0);
            // ...and the cycle begins there, at 30 frames a second.
            cx.executor().advance_clock(ms(250));
            assert_eq!(phase(cx), 0.25, "{blinking:?}");
            cx.executor().advance_clock(ms(750));
            assert_eq!(notified.get(), 30, "{blinking:?}");

            // A key pauses it like any other cursor: solid, and no step.
            cx.executor().advance_clock(ms(120));
            cursor.update(cx, |cursor, cx| {
                cursor.pause(cx);
                assert!(cursor.paused, "{blinking:?}");
                assert!(cursor.frame.is_none(), "{blinking:?}");
            });
            cx.run_until_parked();
            notified.set(0);
            cx.executor().advance_clock(PAUSE_DELAY - ms(1));
            assert_eq!(phase(cx), 0., "{blinking:?}");
            assert_eq!(notified.get(), 0, "{blinking:?}, paused");
            cx.executor().advance_clock(ms(1) + SECOND);
            assert_eq!(notified.get(), 1 + 30, "{blinking:?}, resumed");

            // Nobody tells it that the focus has gone either: the caret is
            // not painted any more, and that is all. It is running for a
            // while yet, and then it is not.
            drop(element);
            cx.executor().advance_clock(PAINTED_FOR / 2);
            assert_ne!(phase(cx), 0., "{blinking:?}: stopped at once");
            cx.executor().advance_clock(PAINTED_FOR / 2 + ms(50));
            assert_eq!(phase(cx), 0., "{blinking:?}");
            notified.set(0);
            // Moving the caret schedules nothing then (dopamine #930).
            cursor.update(cx, |cursor, cx| {
                let epoch = cursor.epoch;
                cursor.pause(cx);
                assert!(!cursor.paused, "{blinking:?}");
                assert_eq!(cursor.epoch, epoch, "{blinking:?}: a resume was scheduled");
            });
            cx.executor().advance_clock(SECOND * 5);
            assert_eq!(notified.get(), 0, "{blinking:?}, not painted");

            // Painted again, it starts over: solid, and from there.
            cursor.update(cx, |cursor, cx| cursor.request_frame(blinking, cx));
            assert_eq!(phase(cx), 0., "{blinking:?}");
            cx.executor().advance_clock(ms(100));
            assert_eq!(phase(cx), 0.1, "{blinking:?}");

            // Told after all -- an input is drawn with the focus before it
            // hears of it -- it runs until it is told to stop, painted or
            // not.
            cursor.update(cx, |cursor, cx| {
                cursor.start(blinking, cx);
                assert!(!cursor.by_paint, "{blinking:?}");
            });
            cx.executor().advance_clock(PAINTED_FOR * 3 + ms(125));
            assert_eq!(phase(cx), 0.125, "{blinking:?}");
            cursor.update(cx, |cursor, cx| cursor.stop(cx));
            assert_eq!(phase(cx), 0., "{blinking:?}");
        }
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
            AnyWindowHandle, EntityInputHandler as _, IntoElement, Modifiers, ParentElement as _,
            Render, Styled as _, VisualTestContext, Window, div, point, px, size,
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

        /// A window with nothing in it, for a state to be made in that is drawn
        /// in another: see [`Editor::open_made_elsewhere`].
        struct Elsewhere;

        impl Render for Elsewhere {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
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
            /// The window the state was made in, when it is not the one it is
            /// drawn in.
            elsewhere: Option<AnyWindowHandle>,
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
                Self::init(cx);
                Self::show(hz, None, cx, move |window, cx| {
                    cx.new(|cx| Self::state(blinking, slide, window, cx))
                })
            }

            /// An editor whose state was made with another window in hand
            /// than the one it is drawn in.
            ///
            /// The state listens for the focus and for the window coming to
            /// the front on the window it is made with, so this one hears of
            /// neither: nothing starts its cursor and nothing stops it. A host
            /// does it without noticing -- a click in the settings window of
            /// dopamine rebuilds the editors of its main window, with the
            /// window the click came in. The focus comes the way a click gives
            /// it: to the handle, and nothing is said to the state.
            fn open_made_elsewhere(
                blinking: CursorBlinking,
                hz: u32,
                cx: &'a mut TestAppContext,
            ) -> Self {
                Self::init(cx);
                let elsewhere = cx.add_window(|_, _| Elsewhere);
                let input = elsewhere
                    .update(cx, |_, window, cx| {
                        window.activate_window();
                        cx.new(|cx| Self::state(blinking, CaretAnimation::Off, window, cx))
                    })
                    .unwrap();
                cx.run_until_parked();
                Self::show(hz, Some(elsewhere.into()), cx, move |_, _| input)
            }

            fn init(cx: &mut TestAppContext) {
                if !cx.has_global::<crate::Theme>() {
                    cx.update(crate::init);
                }
            }

            /// A dozen rows of 120: a frame is cheap, and the caret can be
            /// scrolled out of view.
            fn state(
                blinking: CursorBlinking,
                slide: CaretAnimation,
                window: &mut Window,
                cx: &mut Context<InputState>,
            ) -> InputState {
                InputState::new(window, cx)
                    .code_editor("text")
                    .cursor_blinking(blinking)
                    .caret_animation(slide)
                    // Its fade after a scroll is a timer of its own, on the
                    // wall clock: not counted here.
                    .scrollbar_style(Some(ScrollbarShow::Always), true, true, 0, false, false)
                    .default_value("let caret = blink();\n".repeat(120))
            }

            /// A window in front on the editor `make` hands over, with the
            /// focus in it. Made `elsewhere`, the focus is only given to the
            /// handle; otherwise the state is asked to take it, which starts
            /// the blink there and then.
            fn show(
                hz: u32,
                elsewhere: Option<AnyWindowHandle>,
                cx: &'a mut TestAppContext,
                make: impl FnOnce(&mut Window, &mut App) -> Entity<InputState> + 'static,
            ) -> Self {
                // Whatever the window before this one still had asked for.
                display_frames::take();

                let frames = Rc::new(Cell::new(0));
                let slot = Rc::new(RefCell::new(None));
                let (_, cx) = cx.add_window_view({
                    let (frames, slot) = (frames.clone(), slot.clone());
                    move |window, cx| {
                        let input = make(window, cx);
                        *slot.borrow_mut() = Some(input.clone());
                        let host = cx.new(|_| Host { input, frames });
                        Root::new(host, window, cx)
                    }
                });
                let input = slot.take().unwrap();

                cx.simulate_resize(size(px(640.), px(280.)));
                cx.update(|window, _| window.activate_window());
                cx.run_until_parked();
                let mut editor = Self {
                    cx,
                    input,
                    frames,
                    hz,
                    refreshes: 0,
                    at: Duration::ZERO,
                    elsewhere,
                };
                editor.focus();
                editor
            }

            /// Give the editor the focus: see [`Self::show`].
            fn focus(&mut self) {
                if self.elsewhere.is_some() {
                    let handle = self
                        .input
                        .read_with(self.cx, |input, _| input.focus_handle.clone());
                    self.cx.update(|window, _| handle.focus(window));
                } else {
                    self.input
                        .clone()
                        .update_in(self.cx, |input, window, cx| input.focus(window, cx));
                }
                self.cx.run_until_parked();
            }

            /// Take the focus away from the editor.
            fn blur(&mut self) {
                self.cx.update(|window, _| window.blur());
                self.cx.run_until_parked();
            }

            /// Click into the text: the focus, the way it usually comes.
            fn click(&mut self) {
                self.cx
                    .simulate_click(point(px(200.), px(100.)), Modifiers::none());
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
                    if let Some(elsewhere) = self.elsewhere.take() {
                        elsewhere
                            .update(self.cx, |_, window, _| window.remove_window())
                            .ok();
                    }
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
        /// Text that is being composed moves the caret as typing does, but no
        /// key goes down for it: an input method takes the keys. A fade is
        /// paused for it all the same -- composing costs it what it costs a
        /// solid caret. The default blinks through it, as it always did.
        #[gpui::test]
        fn composing_draws_no_frame_for_a_fade(cx: &mut TestAppContext) {
            /// A second of composing, the marked text a character longer
            /// every 100ms: how many frames.
            fn compose_for_a_second(editor: &mut Editor) -> usize {
                let before = editor.frames.get();
                let mut marked = String::new();
                for _ in 0..10 {
                    marked.push('\u{3042}');
                    editor
                        .input
                        .clone()
                        .update_in(editor.cx, |input, window, cx| {
                            input.replace_and_mark_text_in_range(None, &marked, None, window, cx)
                        });
                    editor.pass(Duration::from_millis(100));
                }
                editor.frames.get() - before
            }

            let solid = compose_for_a_second(&mut Editor::open(CursorBlinking::Solid, 60, cx));
            assert!(solid >= 10, "a frame a change at least: {solid}");
            for blinking in FADES {
                let mut editor = Editor::open(blinking, 60, cx);
                assert_eq!(compose_for_a_second(&mut editor), solid, "{blinking:?}");
                // The last change was 100ms ago: the fade picks up when the
                // pause after it is over.
                editor.pass(PAUSE_DELAY - Duration::from_millis(100));
                assert_eq!(editor.pass(SECOND), 30, "{blinking:?}, after the pause");
            }

            let blink = compose_for_a_second(&mut Editor::open(CursorBlinking::Blink, 60, cx));
            assert_eq!(blink, solid + 2, "the default, two frames a second on top");
        }

        /// A state that was made with another window in hand than the one it
        /// is drawn in hears nothing of the focus, and nobody starts its
        /// cursor. A fade runs all the same, for as long as its caret is
        /// painted -- it used to be painted solid for good -- and costs what
        /// it costs any editor: nothing while typing, nothing without a caret.
        #[gpui::test]
        fn a_fade_runs_in_an_editor_that_was_made_elsewhere(cx: &mut TestAppContext) {
            let solid =
                Editor::open_made_elsewhere(CursorBlinking::Solid, 60, cx).type_for_a_second();
            assert!(solid >= 10, "a frame a key at least: {solid}");

            for blinking in FADES {
                let mut editor = Editor::open_made_elsewhere(blinking, 60, cx);
                assert_eq!(editor.pass(SECOND), 30, "{blinking:?}");
                assert_eq!(editor.pass(SECOND), 30, "{blinking:?}");

                assert_eq!(editor.type_for_a_second(), solid, "{blinking:?}, typing");
                // The last key was 100ms ago.
                editor.pass(PAUSE_DELAY - Duration::from_millis(100));
                assert_eq!(editor.pass(SECOND), 30, "{blinking:?}, after the pause");

                // Nothing tells it that the focus went, either.
                editor.blur();
                // Whatever was already on its way (half a second covers it).
                editor.pass(SECOND / 2);
                assert_eq!(
                    editor.pass(SECOND * 3),
                    0,
                    "{blinking:?}, without the focus"
                );
                // Back with a click, which is how it comes.
                editor.click();
                assert_eq!(editor.pass(SECOND), 30, "{blinking:?}, clicked");

                editor.cx.deactivate_window();
                editor.pass(SECOND / 2);
                assert_eq!(editor.pass(SECOND * 3), 0, "{blinking:?}, in the back");
                editor.cx.update(|window, _| window.activate_window());
                editor.cx.run_until_parked();
                assert_eq!(editor.pass(SECOND), 30, "{blinking:?}, back in front");
                assert_eq!(editor.pass(SECOND), 30, "{blinking:?}, back in front");
            }
        }

        /// The default is not started by being painted. A fade ends when its
        /// caret is not painted any more; the 500ms timer of `Blink` runs
        /// whether it is or not, and nothing would stop it in an editor that
        /// hears of no blur: two frames a second for a caret nobody sees
        /// (dopamine #930). So there is no timer -- and with the default
        /// blink, such an editor has no caret to show. That one is the host's
        /// to put right: a state has to be made with the window it is drawn
        /// in.
        #[gpui::test]
        fn the_default_blink_is_not_started_by_being_painted(cx: &mut TestAppContext) {
            let mut editor = Editor::open_made_elsewhere(CursorBlinking::Blink, 60, cx);
            assert_eq!(editor.pass(SECOND * 2), 0);
            editor.blur();
            editor.pass(SECOND / 2);
            assert_eq!(editor.pass(SECOND * 2), 0, "without the focus");
        }
    }
}
