//! Cancellation-safe input bookkeeping, independent of portal transport and validation.
use std::{future::Future, time::Duration};

use crate::{
    keys::Key,
    portal::Portal,
    types::{Button, Point},
};
use anyhow::Result;

pub(crate) enum Gesture {
    Move(Point),
    Relative(f64, f64),
    Click(Point, i32, u8),
    Scroll(Point, f64, f64),
    Keys(Vec<Key>),
    Text(String, Vec<Key>, Option<Point>),
    Drag(Vec<Point>, i32, u64, Vec<Key>),
    Hold {
        keys: Vec<Key>,
        button: Option<i32>,
        duration: u64,
        dx: f64,
        dy: f64,
    },
}

impl Gesture {
    /// Resolve before input so missing keymaps/invalid aliases send nothing. A
    /// targeted paste resolves again after its click, which may change the
    /// active per-window layout. That second failure can truthfully be partial.
    pub(crate) async fn resolve(mut self, backend: &impl InputBackend) -> Result<Self> {
        let (keys, refresh_after_click) = match &mut self {
            Self::Keys(keys) | Self::Drag(_, _, _, keys) | Self::Hold { keys, .. } => (keys, false),
            Self::Text(_, keys, at) => (keys, at.is_some()),
            _ => return Ok(self),
        };
        let resolved = backend.resolve_keys(keys).await?;
        if !refresh_after_click {
            *keys = resolved;
        }
        Ok(self)
    }
}

/// Futures are Send so the controller can own this engine in its spawned actor.
pub(crate) trait InputBackend: Sync {
    fn resolve_keys(&self, keys: &[Key]) -> impl Future<Output = Result<Vec<Key>>> + Send;
    fn move_to(&self, node: u32, x: f64, y: f64) -> impl Future<Output = Result<()>> + Send;
    fn move_relative(&self, dx: f64, dy: f64) -> impl Future<Output = Result<()>> + Send;
    fn button(&self, code: i32, pressed: bool) -> impl Future<Output = Result<()>> + Send;
    fn key(&self, keysym: i32, pressed: bool) -> impl Future<Output = Result<()>> + Send;
    fn keycode(&self, code: i32, pressed: bool) -> impl Future<Output = Result<()>> + Send;
    fn scroll(&self, dx: f64, dy: f64) -> impl Future<Output = Result<()>> + Send;
    fn set_text(&self, text: &str) -> impl Future<Output = Result<()>> + Send;
}

impl InputBackend for Portal {
    async fn resolve_keys(&self, keys: &[Key]) -> Result<Vec<Key>> {
        Portal::resolve_keys(self, keys).await
    }
    async fn move_to(&self, node: u32, x: f64, y: f64) -> Result<()> {
        Portal::move_to(self, node, x, y).await
    }
    async fn move_relative(&self, dx: f64, dy: f64) -> Result<()> {
        Portal::move_relative(self, dx, dy).await
    }
    async fn button(&self, code: i32, pressed: bool) -> Result<()> {
        Portal::button(self, code, pressed).await
    }
    async fn key(&self, keysym: i32, pressed: bool) -> Result<()> {
        Portal::key(self, keysym, pressed).await
    }
    async fn keycode(&self, code: i32, pressed: bool) -> Result<()> {
        Portal::keycode(self, code, pressed).await
    }
    async fn scroll(&self, dx: f64, dy: f64) -> Result<()> {
        Portal::scroll(self, dx, dy).await
    }
    async fn set_text(&self, text: &str) -> Result<()> {
        Portal::set_text(self, text).await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    Key(Key),
    Button(i32),
}

impl Held {
    async fn send(self, backend: &impl InputBackend, pressed: bool) -> Result<()> {
        match self {
            Self::Key(Key::Keysym(code)) => backend.key(code, pressed).await,
            Self::Key(Key::Keycode(code)) => backend.keycode(code, pressed).await,
            Self::Button(code) => backend.button(code, pressed).await,
        }
    }
}

/// One ordered ledger spans all input kinds, including unacknowledged presses.
#[derive(Default)]
pub(crate) struct InputState {
    held: Vec<Held>,
}

impl InputState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    async fn press(&mut self, backend: &impl InputBackend, input: Held) -> Result<()> {
        // Delivery can precede an error or cancellation of the acknowledgement future.
        self.held.push(input);
        input.send(backend, true).await
    }

    async fn release(&mut self, backend: &impl InputBackend, input: Held) -> Result<()> {
        input.send(backend, false).await?;
        if let Some(index) = self.held.iter().rposition(|held| *held == input) {
            self.held.remove(index);
        }
        Ok(())
    }

    async fn press_keys(&mut self, backend: &impl InputBackend, keys: &[Key]) -> Result<()> {
        for &key in keys {
            self.press(backend, Held::Key(key)).await?;
        }
        Ok(())
    }

    async fn send_keys(&mut self, backend: &impl InputBackend, keys: Vec<Key>) -> Result<()> {
        self.press_keys(backend, &keys).await?;
        tokio::time::sleep(Duration::from_millis(30)).await;
        for key in keys.into_iter().rev() {
            self.release(backend, Held::Key(key)).await?;
        }
        Ok(())
    }

    /// The caller must run release_all after success, error, or dropping this future.
    /// Geometry, bounds, chord and duration validation belong to the controller.
    pub(crate) async fn send(
        &mut self,
        backend: &impl InputBackend,
        node: u32,
        gesture: Gesture,
    ) -> Result<()> {
        match gesture {
            Gesture::Move(p) => backend.move_to(node, p.x, p.y).await?,
            Gesture::Relative(dx, dy) => backend.move_relative(dx, dy).await?,
            Gesture::Click(p, button, count) => {
                backend.move_to(node, p.x, p.y).await?;
                for n in 0..count {
                    if n > 0 {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                    }
                    self.press(backend, Held::Button(button)).await?;
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    self.release(backend, Held::Button(button)).await?;
                }
            }
            Gesture::Scroll(p, dx, dy) => {
                backend.move_to(node, p.x, p.y).await?;
                backend.scroll(dx, dy).await?;
            }
            Gesture::Keys(keys) => self.send_keys(backend, keys).await?,
            Gesture::Text(text, keys, at) => {
                if let Some(p) = at {
                    backend.move_to(node, p.x, p.y).await?;
                    self.press(backend, Held::Button(Button::Left.code()))
                        .await?;
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    self.release(backend, Held::Button(Button::Left.code()))
                        .await?;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let keys = backend.resolve_keys(&keys).await?;
                backend.set_text(&text).await?;
                self.send_keys(backend, keys).await?;
            }
            Gesture::Drag(points, button, duration, keys) => {
                self.press_keys(backend, &keys).await?;
                let first = points[0];
                backend.move_to(node, first.x, first.y).await?;
                self.press(backend, Held::Button(button)).await?;
                // Sparse paths still need timed intermediate motion, not an endpoint jump.
                let steps = (duration / 16).max((points.len() - 1) as u64);
                let start = tokio::time::Instant::now();
                for step in 1..=steps {
                    let t = step as f64 / steps as f64 * (points.len() - 1) as f64;
                    let index = (t.floor() as usize).min(points.len() - 2);
                    let frac = (t - index as f64).min(1.0);
                    let a = points[index];
                    let b = points[index + 1];
                    tokio::time::sleep_until(
                        start + Duration::from_millis(duration * step / steps),
                    )
                    .await;
                    backend
                        .move_to(node, a.x + (b.x - a.x) * frac, a.y + (b.y - a.y) * frac)
                        .await?;
                }
                self.release(backend, Held::Button(button)).await?;
            }
            Gesture::Hold {
                keys,
                button,
                duration,
                dx,
                dy,
            } => {
                self.press_keys(backend, &keys).await?;
                if let Some(button) = button {
                    self.press(backend, Held::Button(button)).await?;
                }
                if dx == 0.0 && dy == 0.0 {
                    tokio::time::sleep(Duration::from_millis(duration)).await;
                } else {
                    let steps = (duration / 16).max(1);
                    let start = tokio::time::Instant::now();
                    for step in 1..=steps {
                        tokio::time::sleep_until(
                            start + Duration::from_millis(duration * step / steps),
                        )
                        .await;
                        backend
                            .move_relative(dx / steps as f64, dy / steps as f64)
                            .await?;
                    }
                }
                // Unconditional controller cleanup releases holds (and drag modifiers).
            }
        }
        Ok(())
    }

    /// Drain on attempted cleanup; any error requires the caller to close the session.
    /// Use a single deadline, not two seconds per key on an unresponsive backend.
    /// Keep unattempted entries in the ledger if this cleanup future itself is dropped.
    pub(crate) async fn release_all(&mut self, backend: &impl InputBackend) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut errors = Vec::new();
        while let Some(&input) = self.held.last() {
            match tokio::time::timeout_at(deadline, input.send(backend, false)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(format!("Release {input:?}: {error:#}")),
                Err(_) => errors.push(format!(
                    "Release {input:?}: cleanup deadline exceeded; close input session"
                )),
            }
            self.held.pop();
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    #[derive(Clone, Debug, PartialEq)]
    enum Event {
        Move(u32, f64, f64),
        Relative(f64, f64),
        Button(i32, bool),
        Key(i32, bool),
        Code(i32, bool),
        Scroll(f64, f64),
        Text(String),
    }

    #[derive(Default)]
    struct Fake {
        events: Mutex<Vec<Event>>,
        fail: Option<Event>,
        block: Option<Event>,
        reached: CancellationToken,
        dead_releases: bool,
    }
    impl Fake {
        fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }
        async fn record(&self, event: Event) -> Result<()> {
            self.events.lock().unwrap().push(event.clone());
            if self.block.as_ref() == Some(&event)
                || (self.dead_releases
                    && matches!(
                        event,
                        Event::Button(_, false) | Event::Key(_, false) | Event::Code(_, false)
                    ))
            {
                self.reached.cancel();
                std::future::pending::<()>().await;
            }
            anyhow::ensure!(self.fail.as_ref() != Some(&event), "injected failure");
            Ok(())
        }
    }
    impl InputBackend for Fake {
        async fn resolve_keys(&self, keys: &[Key]) -> Result<Vec<Key>> {
            Ok(keys
                .iter()
                .map(|key| match key {
                    Key::Keysym(0xffe3) => Key::Keycode(29),
                    Key::Keysym(0xffe1) => Key::Keycode(42),
                    key => *key,
                })
                .collect())
        }
        async fn move_to(&self, n: u32, x: f64, y: f64) -> Result<()> {
            self.record(Event::Move(n, x, y)).await
        }
        async fn move_relative(&self, x: f64, y: f64) -> Result<()> {
            self.record(Event::Relative(x, y)).await
        }
        async fn button(&self, c: i32, p: bool) -> Result<()> {
            self.record(Event::Button(c, p)).await
        }
        async fn key(&self, c: i32, p: bool) -> Result<()> {
            self.record(Event::Key(c, p)).await
        }
        async fn keycode(&self, c: i32, p: bool) -> Result<()> {
            self.record(Event::Code(c, p)).await
        }
        async fn scroll(&self, x: f64, y: f64) -> Result<()> {
            self.record(Event::Scroll(x, y)).await
        }
        async fn set_text(&self, s: &str) -> Result<()> {
            self.record(Event::Text(s.into())).await
        }
    }
    fn p(x: f64, y: f64) -> Point {
        Point { x, y }
    }
    async fn cancel_at_block(state: &mut InputState, backend: &Fake, gesture: Gesture) {
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = state.send(backend, 7, gesture) => panic!("expected cancellation, got {result:?}"),
                _ = backend.reached.cancelled() => {}
            }
        }).await.expect("backend never reached cancellation point");
        assert!(state.release_all(backend).await.is_empty());
    }

    #[tokio::test]
    async fn scroll_moves_first_and_never_replays_a_failed_submission() {
        let event = Event::Scroll(2.5, -7.25);
        for fail in [None, Some(event.clone())] {
            let b = Fake {
                fail: fail.clone(),
                ..Fake::default()
            };
            let mut state = InputState::new();
            let result = state
                .send(&b, 7, Gesture::Scroll(p(3., 4.), 2.5, -7.25))
                .await;
            assert_eq!(result.is_err(), fail.is_some());
            assert!(state.release_all(&b).await.is_empty());
            assert_eq!(b.events(), vec![Event::Move(7, 3., 4.), event.clone()]);
        }
    }
    #[tokio::test]
    async fn failed_press_ack_is_still_released() {
        let b = Fake {
            fail: Some(Event::Button(272, true)),
            ..Fake::default()
        };
        let mut s = InputState::new();
        assert!(
            s.send(&b, 7, Gesture::Click(p(1., 2.), 272, 1))
                .await
                .is_err()
        );
        assert!(s.release_all(&b).await.is_empty());
        assert_eq!(
            b.events(),
            vec![
                Event::Move(7, 1., 2.),
                Event::Button(272, true),
                Event::Button(272, false)
            ]
        );
    }

    #[tokio::test]
    async fn cancelled_unacknowledged_button_is_released() {
        let b = Fake {
            block: Some(Event::Button(272, true)),
            ..Fake::default()
        };
        let mut s = InputState::new();
        cancel_at_block(&mut s, &b, Gesture::Click(p(1., 2.), 272, 1)).await;
        assert_eq!(
            b.events(),
            vec![
                Event::Move(7, 1., 2.),
                Event::Button(272, true),
                Event::Button(272, false)
            ]
        );
    }

    #[tokio::test]
    async fn cancellation_during_drag_motion_releases_button() {
        let b = Fake {
            block: Some(Event::Move(7, 3., 4.)),
            ..Fake::default()
        };
        let mut s = InputState::new();
        cancel_at_block(
            &mut s,
            &b,
            Gesture::Drag(vec![p(1., 2.), p(3., 4.)], 272, 50, vec![]),
        )
        .await;
        assert_eq!(b.events().last(), Some(&Event::Button(272, false)));
        assert!(s.held.is_empty());
    }

    #[tokio::test]
    async fn cancelled_unacknowledged_keycode_is_released() {
        let b = Fake {
            block: Some(Event::Code(17, true)),
            ..Fake::default()
        };
        let mut s = InputState::new();
        cancel_at_block(
            &mut s,
            &b,
            Gesture::Hold {
                keys: vec![Key::Keycode(17)],
                button: None,
                duration: 100,
                dx: 0.,
                dy: 0.,
            },
        )
        .await;
        assert_eq!(
            b.events(),
            vec![Event::Code(17, true), Event::Code(17, false)]
        );
    }

    #[tokio::test]
    async fn chord_order_is_reversed_without_duplicate_cleanup() {
        let b = Fake::default();
        let mut s = InputState::new();
        s.send(
            &b,
            7,
            Gesture::Keys(
                crate::keys::chord(&["CTRL".into(), "SHIFT".into(), "V".into()]).unwrap(),
            )
            .resolve(&b)
            .await
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(s.release_all(&b).await.is_empty());
        assert_eq!(
            b.events(),
            vec![
                Event::Code(29, true),
                Event::Code(42, true),
                Event::Key(118, true),
                Event::Key(118, false),
                Event::Code(42, false),
                Event::Code(29, false)
            ]
        );
    }

    #[tokio::test]
    async fn cleanup_reverses_global_order_and_reports_release_failure() {
        let b = Fake {
            fail: Some(Event::Code(17, false)),
            ..Fake::default()
        };
        let mut s = InputState::new();
        s.send(
            &b,
            7,
            Gesture::Hold {
                keys: vec![Key::Keycode(42), Key::Keycode(17)],
                button: Some(272),
                duration: 1,
                dx: 0.,
                dy: 0.,
            },
        )
        .await
        .unwrap();
        let errors = s.release_all(&b).await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("Keycode(17)"));
        assert!(errors[0].contains("injected failure"));
        assert_eq!(
            &b.events()[3..],
            &[
                Event::Button(272, false),
                Event::Code(17, false),
                Event::Code(42, false)
            ]
        );
        assert!(s.release_all(&b).await.is_empty());
        assert_eq!(b.events().len(), 6);
    }

    #[tokio::test]
    async fn dead_backend_has_one_total_cleanup_budget() {
        let b = Fake {
            dead_releases: true,
            ..Fake::default()
        };
        let mut s = InputState::new();
        s.send(
            &b,
            7,
            Gesture::Hold {
                keys: vec![Key::Keysym(1), Key::Keysym(2), Key::Keycode(17)],
                button: Some(272),
                duration: 1,
                dx: 0.,
                dy: 0.,
            },
        )
        .await
        .unwrap();
        let errors = tokio::time::timeout(Duration::from_secs(3), s.release_all(&b))
            .await
            .expect("cleanup exceeded shared budget");
        assert_eq!(errors.len(), 4);
        assert!(errors.iter().all(|e| e.contains("deadline exceeded")));
        assert_eq!(
            &b.events()[4..],
            &[
                Event::Button(272, false),
                Event::Code(17, false),
                Event::Key(2, false),
                Event::Key(1, false)
            ]
        );
    }

    #[tokio::test]
    async fn failed_move_never_clicks() {
        let b = Fake {
            fail: Some(Event::Move(7, 1., 2.)),
            ..Fake::default()
        };
        let mut s = InputState::new();
        assert!(
            s.send(&b, 7, Gesture::Click(p(1., 2.), 272, 1))
                .await
                .is_err()
        );
        assert!(s.release_all(&b).await.is_empty());
        assert_eq!(b.events(), vec![Event::Move(7, 1., 2.)]);
    }

    #[tokio::test]
    async fn failed_unicode_clipboard_never_pastes() {
        let text = "日本語 café".to_owned();
        let b = Fake {
            fail: Some(Event::Text(text.clone())),
            ..Fake::default()
        };
        let mut s = InputState::new();
        assert!(
            s.send(
                &b,
                7,
                Gesture::Text(text.clone(), vec![Key::Keycode(29), Key::Keysym(118)], None)
            )
            .await
            .is_err()
        );
        assert!(s.release_all(&b).await.is_empty());
        assert_eq!(b.events(), vec![Event::Text(text)]);
    }

    #[tokio::test]
    async fn cancelled_mixed_chord_releases_symbol_before_physical_modifier() {
        let b = Fake {
            block: Some(Event::Key(108, true)),
            ..Fake::default()
        };
        let mut s = InputState::new();
        cancel_at_block(
            &mut s,
            &b,
            Gesture::Keys(crate::keys::chord(&["CTRL".into(), "L".into()]).unwrap())
                .resolve(&b)
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            b.events(),
            vec![
                Event::Code(29, true),
                Event::Key(108, true),
                Event::Key(108, false),
                Event::Code(29, false)
            ]
        );
    }

    #[tokio::test]
    async fn targeted_physical_paste_and_failed_modifier_cleanup() {
        let b = Fake::default();
        let mut state = InputState::new();
        state
            .send(
                &b,
                7,
                Gesture::Text(
                    "café 日本語".into(),
                    crate::keys::physical(&[29, 47]).unwrap(),
                    Some(p(1., 2.)),
                ),
            )
            .await
            .unwrap();
        assert!(state.release_all(&b).await.is_empty());
        assert_eq!(
            b.events(),
            vec![
                Event::Move(7, 1., 2.),
                Event::Button(272, true),
                Event::Button(272, false),
                Event::Text("café 日本語".into()),
                Event::Code(29, true),
                Event::Code(47, true),
                Event::Code(47, false),
                Event::Code(29, false)
            ]
        );
        let b = Fake {
            fail: Some(Event::Code(29, true)),
            ..Fake::default()
        };
        assert!(
            state
                .send(
                    &b,
                    7,
                    Gesture::Keys(crate::keys::chord(&["CTRL".into(), "L".into()]).unwrap())
                        .resolve(&b)
                        .await
                        .unwrap()
                )
                .await
                .is_err()
        );
        assert!(state.release_all(&b).await.is_empty());
        assert_eq!(
            b.events(),
            vec![Event::Code(29, true), Event::Code(29, false)]
        );
    }

    #[tokio::test]
    async fn relative_motion_preserves_deltas() {
        let b = Fake::default();
        let mut s = InputState::new();
        s.send(&b, 7, Gesture::Relative(-12.5, 4.25)).await.unwrap();
        assert_eq!(b.events(), vec![Event::Relative(-12.5, 4.25)]);
    }

    #[tokio::test]
    async fn modifier_drag_interpolates_then_releases_button_before_modifier() {
        let b = Fake::default();
        let mut s = InputState::new();
        s.send(
            &b,
            7,
            Gesture::Drag(
                vec![p(0., 0.), p(30., 60.)],
                272,
                50,
                vec![Key::Keycode(42)],
            ),
        )
        .await
        .unwrap();
        assert!(s.release_all(&b).await.is_empty());
        assert_eq!(
            b.events(),
            vec![
                Event::Code(42, true),
                Event::Move(7, 0., 0.),
                Event::Button(272, true),
                Event::Move(7, 10., 20.),
                Event::Move(7, 20., 40.),
                Event::Move(7, 30., 60.),
                Event::Button(272, false),
                Event::Code(42, false)
            ]
        );
    }
}
