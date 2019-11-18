use super::afd::{self, Afd, AfdPollInfo};
use super::io_status_block::IoStatusBlock;
use super::Event;
use super::SocketState;
use crate::sys::Events;
use crate::{Interests, Token};

use miow::iocp::{CompletionPort, CompletionStatus};
use miow::Overlapped;
use std::collections::VecDeque;
use std::marker::PhantomPinned;
use std::mem::{forget, size_of, transmute_copy};
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::pin::Pin;
use std::ptr::null_mut;
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{io, ptr};
use winapi::shared::ntdef::NT_SUCCESS;
use winapi::shared::ntdef::{HANDLE, PVOID};
use winapi::shared::ntstatus::STATUS_CANCELLED;
use winapi::shared::winerror::{ERROR_INVALID_HANDLE, ERROR_IO_PENDING, WAIT_TIMEOUT};
use winapi::um::mswsock::SIO_BASE_HANDLE;
use winapi::um::winsock2::{WSAIoctl, INVALID_SOCKET, SOCKET_ERROR};

const POLL_GROUP__MAX_GROUP_SIZE: usize = 32;

/// Overlapped value to indicate a `Waker` event.
//
// Note: this must be null, `SelectorInner::feed_events` depends on it.
pub const WAKER_OVERLAPPED: *mut Overlapped = ptr::null_mut();

#[derive(Debug)]
struct AfdGroup {
    cp: Arc<CompletionPort>,
    afd_group: Mutex<Vec<Arc<Afd>>>,
}

impl AfdGroup {
    pub fn new(cp: Arc<CompletionPort>) -> AfdGroup {
        AfdGroup {
            afd_group: Mutex::new(Vec::new()),
            cp,
        }
    }

    pub fn acquire(&self) -> io::Result<Arc<Afd>> {
        let mut afd_group = self.afd_group.lock().unwrap();
        if afd_group.len() == 0 {
            self._alloc_afd_group(&mut afd_group)?;
        } else {
            // + 1 reference in Vec
            if Arc::strong_count(afd_group.last().unwrap()) >= POLL_GROUP__MAX_GROUP_SIZE + 1 {
                self._alloc_afd_group(&mut afd_group)?;
            }
        }
        match afd_group.last() {
            Some(arc) => Ok(arc.clone()),
            None => unreachable!(),
        }
    }

    pub fn release_unused_afd(&self) {
        let mut afd_group = self.afd_group.lock().unwrap();
        afd_group.retain(|g| Arc::strong_count(&g) > 1);
    }

    fn _alloc_afd_group(&self, afd_group: &mut Vec<Arc<Afd>>) -> io::Result<()> {
        let afd = Afd::new(&self.cp)?;
        let arc = Arc::new(afd);
        afd_group.push(arc);
        Ok(())
    }
}

#[derive(Debug)]
enum SockPollStatus {
    Idle,
    Pending,
    Cancelled,
}

#[derive(Debug)]
pub struct SockState {
    iosb: IoStatusBlock,
    poll_info: AfdPollInfo,
    afd: Arc<Afd>,

    raw_socket: RawSocket,
    base_socket: RawSocket,

    user_evts: u32,
    pending_evts: u32,

    user_data: u64,

    poll_status: SockPollStatus,
    delete_pending: bool,

    pinned: PhantomPinned,
}

impl SockState {
    fn new(raw_socket: RawSocket, afd: Arc<Afd>) -> io::Result<SockState> {
        Ok(SockState {
            iosb: IoStatusBlock::zeroed(),
            poll_info: AfdPollInfo::zeroed(),
            afd,
            raw_socket,
            base_socket: get_base_socket(raw_socket)?,
            user_evts: 0,
            pending_evts: 0,
            user_data: 0,
            poll_status: SockPollStatus::Idle,
            delete_pending: false,
            pinned: PhantomPinned,
        })
    }

    /// True if need to be added on update queue, false otherwise.
    fn set_event(&mut self, ev: Event) -> bool {
        /* afd::POLL_CONNECT_FAIL and afd::POLL_ABORT are always reported, even when not requested by the caller. */
        let events = ev.flags | afd::POLL_CONNECT_FAIL | afd::POLL_ABORT;

        self.user_evts = events;
        self.user_data = ev.data;

        (events & !self.pending_evts) != 0
    }

    fn update(&mut self, self_arc: &Pin<Arc<Mutex<SockState>>>) -> io::Result<()> {
        assert!(!self.delete_pending);

        if let SockPollStatus::Pending = self.poll_status {
            if (self.user_evts & afd::KNOWN_EVENTS & !self.pending_evts) == 0 {
                /* All the events the user is interested in are already being monitored by
                 * the pending poll operation. It might spuriously complete because of an
                 * event that we're no longer interested in; when that happens we'll submit
                 * a new poll operation with the updated event mask. */
            } else {
                /* A poll operation is already pending, but it's not monitoring for all the
                 * events that the user is interested in. Therefore, cancel the pending
                 * poll operation; when we receive it's completion package, a new poll
                 * operation will be submitted with the correct event mask. */
                self.cancel()?;
            }
        } else if let SockPollStatus::Cancelled = self.poll_status {
            /* The poll operation has already been cancelled, we're still waiting for
             * it to return. For now, there's nothing that needs to be done. */
        } else if let SockPollStatus::Idle = self.poll_status {
            /* No poll operation is pending; start one. */
            self.poll_info.exclusive = 0;
            self.poll_info.number_of_handles = 1;
            *unsafe { self.poll_info.timeout.QuadPart_mut() } = std::i64::MAX;
            self.poll_info.handles[0].handle = self.base_socket as HANDLE;
            self.poll_info.handles[0].status = 0;
            self.poll_info.handles[0].events = self.user_evts | afd::POLL_LOCAL_CLOSE;

            let overlapped_ptr: PVOID = unsafe { transmute_copy(self_arc) };

            let result = unsafe {
                self.afd
                    .poll(&mut self.poll_info, &mut *self.iosb, overlapped_ptr)
            };
            if let Err(e) = result {
                let code = e.raw_os_error().unwrap();
                if code == ERROR_IO_PENDING as i32 {
                    /* Overlapped poll operation in progress; this is expected. */
                } else if code == ERROR_INVALID_HANDLE as i32 {
                    /* Socket closed; it'll be dropped. */
                    self.mark_delete();
                    return Ok(());
                } else {
                    return Err(e);
                }
            }

            // We've effectively created another reference to the SockState
            // struct by transmuting `self_arc` to a raw pointer, but the Arc's
            // reference count has not been increased so far. Now that the
            // AFD_POLL operation has succesfully started, it is certain that
            // once the operation completes, this raw pointer will be received
            // back from the kernel and converted to an actual Arc (in
            // `feed_events()`). Therefore increase the Arc reference count here
            // by cloning it and immediately leaking the clone.
            forget(self_arc.clone());
            self.poll_status = SockPollStatus::Pending;
            self.pending_evts = self.user_evts;
        } else {
            unreachable!();
        }
        Ok(())
    }

    fn cancel(&mut self) -> io::Result<()> {
        match self.poll_status {
            SockPollStatus::Pending => {}
            _ => unreachable!(),
        };
        unsafe {
            self.afd.cancel(&mut *self.iosb)?;
        }
        self.poll_status = SockPollStatus::Cancelled;
        self.pending_evts = 0;
        Ok(())
    }

    // This is the function called from the overlapped using as Arc<Mutex<SockState>>. Watch out for reference counting.
    fn feed_event(&mut self) -> Option<Event> {
        self.poll_status = SockPollStatus::Idle;
        self.pending_evts = 0;

        let mut afd_events = 0;
        // We use the status info in IO_STATUS_BLOCK to determine the socket poll status. It is unsafe to use a pointer of IO_STATUS_BLOCK.
        unsafe {
            if self.delete_pending {
                return None;
            } else if self.iosb.u.Status == STATUS_CANCELLED {
                /* The poll request was cancelled by CancelIoEx. */
            } else if !NT_SUCCESS(self.iosb.u.Status) {
                /* The overlapped request itself failed in an unexpected way. */
                afd_events = afd::POLL_CONNECT_FAIL;
            } else if self.poll_info.number_of_handles < 1 {
                /* This poll operation succeeded but didn't report any socket events. */
            } else if self.poll_info.handles[0].events & afd::POLL_LOCAL_CLOSE != 0 {
                /* The poll operation reported that the socket was closed. */
                self.mark_delete();
                return None;
            } else {
                afd_events = self.poll_info.handles[0].events;
            }
        }

        afd_events &= self.user_evts;

        if afd_events == 0 {
            return None;
        }

        // In mio, we have to simulate Edge-triggered behavior to match API usage.
        // The strategy here is to intercept all read/write from user that could cause WouldBlock usage,
        // then reregister the socket to reset the interests.

        // Reset readable event
        if (afd_events & interests_to_afd_flags(Interests::READABLE)) != 0 {
            self.user_evts &= !(interests_to_afd_flags(Interests::READABLE));
        }
        // Reset writable event
        if (afd_events & interests_to_afd_flags(Interests::WRITABLE)) != 0 {
            self.user_evts &= !interests_to_afd_flags(Interests::WRITABLE);
        }

        Some(Event {
            data: self.user_data,
            flags: afd_events,
        })
    }

    pub fn is_pending_deletion(&self) -> bool {
        self.delete_pending
    }

    pub fn mark_delete(&mut self) {
        if !self.delete_pending {
            if let SockPollStatus::Pending = self.poll_status {
                drop(self.cancel());
            }

            self.delete_pending = true;
        }
    }
}

impl Drop for SockState {
    fn drop(&mut self) {
        self.mark_delete();
    }
}

/// Each Selector has a globally unique(ish) ID associated with it. This ID
/// gets tracked by `TcpStream`, `TcpListener`, etc... when they are first
/// registered with the `Selector`. If a type that is previously associated with
/// a `Selector` attempts to register itself with a different `Selector`, the
/// operation will return with an error. This matches windows behavior.
#[cfg(debug_assertions)]
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// Windows implementaion of `sys::Selector`
///
/// Edge-triggered event notification is simulated by resetting internal event flag of each socket state `SockState`
/// and setting all events back by intercepting all requests that could cause `io::ErrorKind::WouldBlock` happening.
///
/// This selector is currently only support socket due to `Afd` driver is winsock2 specific.
#[derive(Debug)]
pub struct Selector {
    #[cfg(debug_assertions)]
    id: usize,

    inner: Arc<SelectorInner>,
}

impl Selector {
    pub fn new() -> io::Result<Selector> {
        SelectorInner::new().map(|inner| {
            #[cfg(debug_assertions)]
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1;
            Selector {
                #[cfg(debug_assertions)]
                id,
                inner: Arc::new(inner),
            }
        })
    }

    pub fn try_clone(&self) -> io::Result<Selector> {
        Ok(Selector {
            #[cfg(debug_assertions)]
            id: self.id,
            inner: Arc::clone(&self.inner),
        })
    }

    /// # Safety
    ///
    /// This requires a mutable reference to self because only a single thread
    /// can poll IOCP at a time.
    pub fn select(&mut self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.select(events, timeout)
    }

    pub fn register<S: SocketState + AsRawSocket>(
        &self,
        socket: &S,
        token: Token,
        interests: Interests,
    ) -> io::Result<()> {
        self.inner.register(socket, token, interests)
    }

    pub fn reregister<S: SocketState>(
        &self,
        socket: &S,
        token: Token,
        interests: Interests,
    ) -> io::Result<()> {
        self.inner.reregister(socket, token, interests)
    }

    pub fn deregister<S: SocketState>(&self, socket: &S) -> io::Result<()> {
        self.inner.deregister(socket)
    }

    #[cfg(debug_assertions)]
    pub fn id(&self) -> usize {
        self.id
    }

    pub(super) fn clone_inner(&self) -> Arc<SelectorInner> {
        self.inner.clone()
    }

    pub(super) fn clone_port(&self) -> Arc<CompletionPort> {
        self.inner.cp.clone()
    }
}

#[derive(Debug)]
pub struct SelectorInner {
    cp: Arc<CompletionPort>,
    update_queue: Mutex<VecDeque<Pin<Arc<Mutex<SockState>>>>>,
    afd_group: AfdGroup,
    is_polling: AtomicBool,
}

// We have ensured thread safety by introducing lock manually.
unsafe impl Sync for SelectorInner {}

impl SelectorInner {
    pub fn new() -> io::Result<SelectorInner> {
        CompletionPort::new(0).map(|cp| {
            let cp = Arc::new(cp);
            let cp_afd = Arc::clone(&cp);

            SelectorInner {
                cp,
                update_queue: Mutex::new(VecDeque::new()),
                afd_group: AfdGroup::new(cp_afd),
                is_polling: AtomicBool::new(false),
            }
        })
    }

    /// # Safety
    ///
    /// May only be calling via `Selector::select`.
    pub fn select(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        events.clear();

        let mut n = 0;
        let start = Instant::now();

        loop {
            if timeout.is_none() {
                let len = self.select2(&mut events.statuses, &mut events.events, None)?;
                if len == 0 {
                    continue;
                }
                return Ok(());
            } else {
                if n >= events.statuses.len() {
                    return Ok(());
                }
                let timeout = timeout.unwrap();
                let deadline = start + timeout;
                let now = Instant::now();
                if timeout.as_nanos() != 0 {
                    if now >= deadline {
                        return Ok(());
                    }
                    let len = self.select2(
                        &mut events.statuses[n..],
                        &mut events.events,
                        Some(deadline - now),
                    )?;
                    if len == 0 {
                        return Ok(());
                    }
                    n += len;
                } else {
                    self.select2(&mut events.statuses[n..], &mut events.events, Some(timeout))?;
                    return Ok(());
                }
            }
        }
    }

    pub fn select2(
        &self,
        statuses: &mut [CompletionStatus],
        events: &mut Vec<Event>,
        timeout: Option<Duration>,
    ) -> io::Result<usize> {
        assert_eq!(self.is_polling.swap(true, Ordering::AcqRel), false);

        unsafe { self.update_sockets_events() }?;

        let result = self.cp.get_many(statuses, timeout);

        self.is_polling.store(false, Ordering::Relaxed);

        match result {
            Ok(iocp_events) => Ok(unsafe { self.feed_events(events, iocp_events) }),
            Err(ref e) if e.raw_os_error() == Some(WAIT_TIMEOUT as i32) => Ok(0),
            Err(e) => Err(e),
        }
    }

    pub fn register<S: SocketState + AsRawSocket>(
        &self,
        socket: &S,
        token: Token,
        interests: Interests,
    ) -> io::Result<()> {
        if socket.get_sock_state().is_some() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }

        let flags = interests_to_afd_flags(interests);

        let sock = self._alloc_sock_for_rawsocket(socket.as_raw_socket())?;
        let event = Event {
            flags,
            data: token.0 as u64,
        };

        {
            sock.lock().unwrap().set_event(event);
        }
        socket.set_sock_state(Some(sock));
        unsafe {
            self.add_socket_to_update_queue(socket);
            self.update_sockets_events_if_polling()?;
        }

        Ok(())
    }

    pub fn reregister<S: SocketState>(
        &self,
        socket: &S,
        token: Token,
        interests: Interests,
    ) -> io::Result<()> {
        let flags = interests_to_afd_flags(interests);

        let sock = match socket.get_sock_state() {
            Some(sock) => sock,
            None => return Err(io::Error::from(io::ErrorKind::NotFound)),
        };
        let event = Event {
            flags,
            data: token.0 as u64,
        };

        {
            sock.lock().unwrap().set_event(event);
        }
        unsafe {
            self.add_socket_to_update_queue(socket);
            self.update_sockets_events_if_polling()?;
        }

        Ok(())
    }

    pub fn deregister<S: SocketState>(&self, socket: &S) -> io::Result<()> {
        if socket.get_sock_state().is_none() {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        socket.set_sock_state(None);
        self.afd_group.release_unused_afd();
        Ok(())
    }

    unsafe fn update_sockets_events(&self) -> io::Result<()> {
        let mut update_queue = self.update_queue.lock().unwrap();
        loop {
            let sock = match update_queue.pop_front() {
                Some(sock) => sock,
                None => break,
            };
            let mut sock_internal = sock.lock().unwrap();
            if !sock_internal.is_pending_deletion() {
                sock_internal.update(&sock).unwrap();
            }
        }
        self.afd_group.release_unused_afd();
        Ok(())
    }

    /// This function is called by register() and reregister() to start an
    /// IOCTL_AFD_POLL operation corresponding to the registered events, but
    /// only if necessary.
    ///
    /// Since it is not possible to modify or synchronously cancel an AFD_POLL
    /// operation, and there can be only one active AFD_POLL operation per
    /// (socket, completion port) pair at any time, it is expensive to change
    /// a socket's event registration after it has been submitted to the kernel.
    ///
    /// Therefore, if no other threads are polling when interest in a socket
    /// event is (re)registered, the socket is added to the 'update queue', but
    /// the actual syscall to start the IOCTL_AFD_POLL operation is deferred
    /// until just before the GetQueuedCompletionStatusEx() syscall is made.
    ///
    /// However, when another thread is already blocked on
    /// GetQueuedCompletionStatusEx() we tell the kernel about the registered
    /// socket event(s) immediately.
    unsafe fn update_sockets_events_if_polling(&self) -> io::Result<()> {
        if self.is_polling.load(Ordering::Acquire) {
            self.update_sockets_events()
        } else {
            Ok(())
        }
    }

    unsafe fn add_socket_to_update_queue<S: SocketState>(&self, socket: &S) {
        let sock_state = socket.get_sock_state().unwrap();
        let mut update_queue = self.update_queue.lock().unwrap();
        update_queue.push_back(sock_state);
    }

    // It returns processed count of iocp_events rather than the events itself.
    unsafe fn feed_events(
        &self,
        events: &mut Vec<Event>,
        iocp_events: &[CompletionStatus],
    ) -> usize {
        let mut n = 0;
        let mut update_queue = self.update_queue.lock().unwrap();
        for iocp_event in iocp_events.iter() {
            if iocp_event.overlapped().is_null() {
                // `Waker` event, we'll add a readable event to match the other platforms.
                events.push(Event {
                    flags: afd::POLL_RECEIVE,
                    data: iocp_event.token() as u64,
                });
                n += 1;
                continue;
            }
            let sock_arc: Pin<Arc<Mutex<SockState>>> = transmute_copy(&iocp_event.overlapped());
            let mut sock_guard = sock_arc.lock().unwrap();
            match sock_guard.feed_event() {
                Some(e) => {
                    events.push(e);
                }
                None => {}
            }
            n += 1;
            if !sock_guard.is_pending_deletion() {
                update_queue.push_back(sock_arc.clone());
            }
        }
        self.afd_group.release_unused_afd();
        n
    }

    fn _alloc_sock_for_rawsocket(
        &self,
        raw_socket: RawSocket,
    ) -> io::Result<Pin<Arc<Mutex<SockState>>>> {
        let afd = self.afd_group.acquire()?;
        Ok(Arc::pin(Mutex::new(SockState::new(raw_socket, afd)?)))
    }
}

fn interests_to_afd_flags(interests: Interests) -> u32 {
    let mut flags = 0;

    if interests.is_readable() {
        // afd::POLL_DISCONNECT for is_read_hup()
        flags |= afd::POLL_RECEIVE | afd::POLL_ACCEPT | afd::POLL_DISCONNECT;
    }

    if interests.is_writable() {
        flags |= afd::POLL_SEND;
    }

    flags
}

fn get_base_socket(raw_socket: RawSocket) -> io::Result<RawSocket> {
    let mut base_socket: RawSocket = 0;
    let mut bytes: u32 = 0;

    unsafe {
        if WSAIoctl(
            raw_socket as usize,
            SIO_BASE_HANDLE,
            null_mut(),
            0,
            &mut base_socket as *mut _ as PVOID,
            size_of::<RawSocket>() as u32,
            &mut bytes,
            null_mut(),
            None,
        ) == SOCKET_ERROR
        {
            return Err(io::Error::from_raw_os_error(INVALID_SOCKET as i32));
        }
    }
    Ok(base_socket)
}
