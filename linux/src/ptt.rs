use std::time::{Duration, Instant};

#[derive(Debug, PartialEq)]
pub enum Action {
    Start(bool),
    Stop,
    Cancel(bool),
}
#[derive(Default)]
pub struct PushToTalk {
    down: Option<Instant>,
    last_tap: Option<Instant>,
    active: bool,
    hands_free: bool,
}
impl PushToTalk {
    pub fn press(&mut self, now: Instant) -> Option<Action> {
        if self.down.is_some() {
            return None;
        }
        self.down = Some(now);
        if self.hands_free || self.active {
            return None;
        }
        self.active = true;
        Some(Action::Start(false))
    }
    pub fn release(&mut self, now: Instant) -> Option<Action> {
        let down = self.down.take()?;
        if self.hands_free {
            self.hands_free = false;
            self.active = false;
            return Some(Action::Stop);
        }
        if now.duration_since(down) < Duration::from_millis(250) {
            if self
                .last_tap
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(350))
            {
                self.last_tap = None;
                self.hands_free = true;
                self.active = true;
                return Some(Action::Start(true));
            }
            self.last_tap = Some(now);
            self.active = false;
            return Some(Action::Cancel(true));
        }
        if self.active {
            self.active = false;
            Some(Action::Stop)
        } else {
            None
        }
    }
    pub fn cancel(&mut self) -> Option<Action> {
        let active = self.active;
        self.down = None;
        self.last_tap = None;
        self.active = false;
        self.hands_free = false;
        active.then_some(Action::Cancel(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hold_repeat_release_and_cancel_never_commit_cancelled_audio() {
        let mut ptt = PushToTalk::default();
        let t = Instant::now();
        assert_eq!(ptt.press(t), Some(Action::Start(false)));
        assert_eq!(ptt.press(t + Duration::from_millis(40)), None);
        assert_eq!(ptt.release(t + Duration::from_secs(1)), Some(Action::Stop));
        assert_eq!(
            ptt.press(t + Duration::from_secs(2)),
            Some(Action::Start(false))
        );
        assert_eq!(ptt.cancel(), Some(Action::Cancel(false)));
        assert_eq!(ptt.release(t + Duration::from_secs(3)), None);
    }
    #[test]
    fn double_tap_stays_active_until_a_later_tap() {
        let mut ptt = PushToTalk::default();
        let t = Instant::now();
        ptt.press(t);
        assert_eq!(
            ptt.release(t + Duration::from_millis(50)),
            Some(Action::Cancel(true))
        );
        ptt.press(t + Duration::from_millis(100));
        assert_eq!(
            ptt.release(t + Duration::from_millis(150)),
            Some(Action::Start(true))
        );
        assert_eq!(ptt.press(t + Duration::from_secs(3)), None);
        assert_eq!(
            ptt.release(t + Duration::from_millis(3050)),
            Some(Action::Stop)
        );
    }
    #[test]
    fn slow_taps_do_not_enter_hands_free() {
        let mut ptt = PushToTalk::default();
        let t = Instant::now();
        ptt.press(t);
        ptt.release(t + Duration::from_millis(50));
        ptt.press(t + Duration::from_secs(1));
        assert_eq!(
            ptt.release(t + Duration::from_millis(1050)),
            Some(Action::Cancel(true))
        );
    }
}
