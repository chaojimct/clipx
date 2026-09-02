use std::sync::{Arc, Mutex};

const GATE_WINDOW_MS: i64 = 600;

/// 写回剪贴板前置位、延时复位；监听侧丢弃 Gate 窗口内的事件，杜绝历史自环。
#[derive(Clone)]
pub struct ClipboardGate(Arc<Mutex<Option<i64>>>);

impl ClipboardGate {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    pub fn arm(&self) {
        *self.0.lock().unwrap() = Some(crate::now_ms());
    }

    pub fn should_suppress(&self, now: i64) -> bool {
        let mut guard = self.0.lock().unwrap();
        match *guard {
            Some(t) if now - t < GATE_WINDOW_MS => true,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }
}

impl Default for ClipboardGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_within_window_then_releases() {
        let gate = ClipboardGate::new();
        let t = crate::now_ms();
        gate.arm();
        assert!(gate.should_suppress(t));
        assert!(gate.should_suppress(t + GATE_WINDOW_MS - 1));
        assert!(!gate.should_suppress(t + GATE_WINDOW_MS));
        assert!(!gate.should_suppress(t + GATE_WINDOW_MS + 1));
    }

    #[test]
    fn unarmed_gate_never_suppresses() {
        let gate = ClipboardGate::new();
        assert!(!gate.should_suppress(crate::now_ms()));
    }
}
