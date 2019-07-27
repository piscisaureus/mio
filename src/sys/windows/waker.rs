use super::afd::AFD_POLL_RECEIVE;
use super::completion_handler;
use super::{Event, Selector, SelectorInner};
use crate::Token;

use miow::iocp::CompletionStatus;
use miow::Overlapped;
use std::io;
use std::sync::Arc;
use winapi::um::minwinbase::OVERLAPPED_ENTRY;

#[derive(Debug)]
pub struct Waker {
    token: Token,
    selector: Arc<SelectorInner>,
}

impl Waker {
    pub fn new(selector: &Selector, token: Token) -> io::Result<Waker> {
        Ok(Waker {
            token,
            selector: selector.clone_inner(),
        })
    }

    pub fn wake(&self) -> io::Result<()> {
        let key = completion_handler::as_key(Self::handle_completion);
        let overlapped = self.token.0 as *mut Overlapped;
        let status = CompletionStatus::new(0, key, overlapped);
        self.selector.port().post(status)
    }

    fn handle_completion(completion: &OVERLAPPED_ENTRY) -> Option<Event> {
        Some(Event {
            flags: AFD_POLL_RECEIVE, // TODO: why use an AFD flag here?
            data: completion.lpOverlapped as u64,
        })
    }
}
