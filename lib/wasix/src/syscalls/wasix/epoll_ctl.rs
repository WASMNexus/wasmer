use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc::UnboundedSender, watch};
use virtual_io::{InterestHandler, InterestType};
use wasmer_wasix_types::wasi::{
    EpollCtl, EpollEvent, EpollType, SubscriptionClock, SubscriptionUnion, Userdata,
};

use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use futures::Future;

use super::*;
use crate::{
    fs::{
        net_error_into_io_err, EpollFd, EpollInterest, EpollJoinGuard, InodeValFilePollGuard,
        InodeValFilePollGuardJoin, InodeValFilePollGuardMode, POLL_GUARD_MAX_RET,
    },
    state::PollEventSet,
    syscalls::*,
    WasiInodes,
};

/// ### `epoll_ctl()`
/// Modifies an epoll interest list
/// Output:
/// - `Fd fd`
///   The new file handle that is used to modify or wait on the interest list
#[instrument(level = "trace", skip_all, fields(timeout_ms = field::Empty, fd_guards = field::Empty, seen = field::Empty, fd), ret, err)]
pub fn epoll_ctl<M: MemorySize + 'static>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    epfd: WasiFd,
    op: EpollCtl,
    fd: WasiFd,
    event_ref: WasmPtr<EpollEvent<M>, M>,
) -> Result<Errno, WasiError> {
    let env = ctx.data();

    let memory = unsafe { env.memory_view(&ctx) };
    let event = if event_ref.offset() != M::ZERO {
        Some(wasi_try_mem_ok!(event_ref.read(&memory)))
    } else {
        None
    };

    let fd_entry = wasi_try_ok!(env.state.fs.get_fd(epfd));

    let inode = fd_entry.inode.clone();
    let tasks = env.tasks().clone();
    let mut inode_guard = inode.read();
    match inode_guard.deref() {
        Kind::Epoll {
            subscriptions, tx, ..
        } => {
            if let EpollCtl::Del | EpollCtl::Mod = op {
                let mut guard = subscriptions.lock().unwrap();
                guard.remove(&fd);

                tracing::trace!(fd, "unregistering waker");
            }
            if let EpollCtl::Add | EpollCtl::Mod = op {
                if let Some(event) = event {
                    let epoll_fd = EpollFd {
                        events: event.events,
                        ptr: wasi_try_ok!(event.data.ptr.try_into().map_err(|_| Errno::Overflow)),
                        fd: event.data.fd,
                        data1: event.data.data1,
                        data2: event.data.data2,
                    };

                    // Output debug
                    tracing::trace!(
                        peb = ?event.events,
                        ptr = ?event.data.ptr,
                        data1 = event.data.data1,
                        data2 = event.data.data2,
                        fd = event.data.fd,
                        "registering waker"
                    );

                    // Now we register the epoll waker
                    let tx = tx.clone();
                    let fd_guards = wasi_try_ok!(register_epoll_waker(&env.state, &epoll_fd, tx));

                    let mut guard = subscriptions.lock().unwrap();
                    guard.insert(event.data.fd, (epoll_fd.clone(), fd_guards));
                }
            }
            Ok(Errno::Success)
        }
        _ => Ok(Errno::Inval),
    }
}

pub struct EpollJoinWaker {
    fd: WasiFd,
    readiness: EpollType,
    tx: Arc<watch::Sender<EpollInterest>>,
}
impl EpollJoinWaker {
    pub fn new(
        fd: WasiFd,
        readiness: EpollType,
        tx: Arc<watch::Sender<EpollInterest>>,
    ) -> Arc<Self> {
        Arc::new(Self { fd, readiness, tx })
    }

    fn wake_now(&self) {
        self.tx.send_modify(|i| {
            i.interest.insert((self.fd, self.readiness));
        });
    }
    pub fn as_waker(self: &Arc<Self>) -> Waker {
        let s: *const Self = Arc::into_raw(Arc::clone(self));
        let raw_waker = RawWaker::new(s as *const (), &VTABLE);
        unsafe { Waker::from_raw(raw_waker) }
    }
}
pub struct EpollHandler {
    fd: WasiFd,
    tx: Arc<watch::Sender<EpollInterest>>,
}
impl EpollHandler {
    pub fn new(fd: WasiFd, tx: Arc<watch::Sender<EpollInterest>>) -> Box<Self> {
        Box::new(Self { fd, tx })
    }
}
impl InterestHandler for EpollHandler {
    fn interest(&mut self, interest: InterestType) {
        let readiness = match interest {
            InterestType::Readable => EpollType::EPOLLIN,
            InterestType::Writable => EpollType::EPOLLOUT,
            InterestType::Closed => EpollType::EPOLLHUP,
            InterestType::Error => EpollType::EPOLLERR,
        };
        self.tx.send_modify(|i| {
            i.interest.insert((self.fd, readiness));
        });
    }
}

fn inline_waker_wake(s: &EpollJoinWaker) {
    let waker_arc = unsafe { Arc::from_raw(s) };
    waker_arc.wake_now();
}

fn inline_waker_clone(s: &EpollJoinWaker) -> RawWaker {
    let arc = unsafe { Arc::from_raw(s) };
    std::mem::forget(arc.clone());
    RawWaker::new(Arc::into_raw(arc) as *const (), &VTABLE)
}

const VTABLE: RawWakerVTable = unsafe {
    RawWakerVTable::new(
        |s| inline_waker_clone(&*(s as *const EpollJoinWaker)), // clone
        |s| inline_waker_wake(&*(s as *const EpollJoinWaker)),  // wake
        |s| (*(s as *const EpollJoinWaker)).wake_now(), // wake by ref (don't decrease refcount)
        |s| drop(Arc::from_raw(s as *const EpollJoinWaker)), // decrease refcount
    )
};

pub(super) fn register_epoll_waker(
    state: &Arc<WasiState>,
    event: &EpollFd,
    tx: Arc<watch::Sender<EpollInterest>>,
) -> Result<Vec<EpollJoinGuard>, Errno> {
    let mut type_ = Eventtype::FdRead;
    let mut peb = PollEventBuilder::new();
    if event.events.contains(EpollType::EPOLLOUT) {
        type_ = Eventtype::FdWrite;
        peb = peb.add(PollEvent::PollOut);
    }
    if event.events.contains(EpollType::EPOLLIN) {
        type_ = Eventtype::FdRead;
        peb = peb.add(PollEvent::PollIn);
    }
    if event.events.contains(EpollType::EPOLLERR) {
        peb = peb.add(PollEvent::PollError);
    }
    if event.events.contains(EpollType::EPOLLHUP) | event.events.contains(EpollType::EPOLLRDHUP) {
        peb = peb.add(PollEvent::PollHangUp);
    }

    // Create a dummy subscription
    let s = Subscription {
        userdata: event.data2,
        type_,
        data: SubscriptionUnion {
            fd_readwrite: SubscriptionFsReadwrite {
                file_descriptor: event.fd,
            },
        },
    };

    // Get guard object which we will register the waker against
    let mut ret = Vec::new();
    let fd_guard = poll_fd_guard(state, peb.build(), event.fd, s)?;
    match &fd_guard.mode {
        // Sockets now use epoll
        InodeValFilePollGuardMode::Socket { inner, .. } => {
            let handler = EpollHandler::new(event.fd, tx.clone());

            let mut inner = inner.protected.write().unwrap();
            let handler = inner.set_handler(handler).map_err(net_error_into_io_err)?;
            drop(inner);

            ret.push(EpollJoinGuard::Handler { fd_guard })
        }
        _ => {
            // Otherwise we fall back on the regular polling guard

            // First we create the waker
            let waker = EpollJoinWaker::new(event.fd, event.events, tx.clone());
            let waker = waker.as_waker();
            let mut cx = Context::from_waker(&waker);

            // Now we use the waker to trigger events
            let mut fd_guard = InodeValFilePollGuardJoin::new(fd_guard);
            if let Poll::Ready(_) = Pin::new(&mut fd_guard).poll(&mut cx) {
                waker.wake();
            }
            ret.push(EpollJoinGuard::Join(fd_guard));
        }
    }
    Ok(ret)
}