//! 记录单次受管 HTTP 拒绝，其他调用或不明结果只会收紧重放资格

use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct HttpReplay(Mutex<State>);

#[derive(Default)]
enum State {
    #[default]
    Untouched,
    Pending,
    Rejected,
    Ineligible,
}

impl HttpReplay {
    pub(super) fn start(&self, inference: bool) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = if inference && matches!(*state, State::Untouched) {
            State::Pending
        } else {
            State::Ineligible
        };
    }

    pub(super) fn finish(&self, status: Option<u16>) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = if status == Some(429) && matches!(*state, State::Pending) {
            State::Rejected
        } else {
            State::Ineligible
        };
    }

    pub(super) fn invalidate(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = State::Ineligible;
    }

    /// 回调资源撤销后读取，防止检查通过后仍有新的出站请求
    pub(crate) fn rejected_once(&self) -> bool {
        matches!(
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            State::Rejected
        )
    }
}
