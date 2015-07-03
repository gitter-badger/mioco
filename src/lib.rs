// Copyright 2015 Dawid Ciężarkiewicz <dpc@dpc.pw>
// See LICENSE-MPL2 file for more information.

//! Coroutine-based handler library for mio
//!
//! Using coroutines, an event-based mio model can be simplified to a set of routines, seamlessly
//! scheduled on demand in userspace.
//!
//! With `mioco` a coroutines can be used to simplify writing asynchronous io handilng in
//! synchronous fashion.

#![feature(result_expect)]
#![warn(missing_docs)]

extern crate mio;
extern crate coroutine;
extern crate nix;

use std::cell::RefCell;
use std::rc::Rc;
use mio::{TryRead, TryWrite, Evented, Token, Handler, EventLoop};

/// State of `mioco` coroutine
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum State {
    BlockedOnWrite(Token),
    BlockedOnRead(Token),
    Running,
    Finished,
}

impl State {
    /// What is the `mio::Interest` for a given `token` at the current state
    fn to_interest_for(&self, token : mio::Token) -> mio::Interest {
        match *self {
            State::Running => panic!("wrong state"),
            State::BlockedOnRead(blocked_token) => if token == blocked_token {
                mio::Interest::readable()
            }
            else {
                mio::Interest::none()
            },
            State::BlockedOnWrite(blocked_token) => if token == blocked_token {
                mio::Interest::writable()
            } else {
                mio::Interest::none()
            },
            State::Finished => mio::Interest::hup(),
        }
    }
}

/// `mioco` can work on any type implementing this trait
pub trait ReadWrite : TryRead+TryWrite+std::io::Read+std::io::Write+Evented { }

impl<T> ReadWrite for T where T: TryRead+TryWrite+std::io::Read+std::io::Write+Evented {}

/// `mioco` coroutine
///
/// Referenced by IO running within it.
struct Coroutine {
    /// Coroutine of Coroutine itself. Stored here so it's available
    /// through every handle and `Coroutine` itself without referencing
    /// back
    pub state : State,
    coroutine : Option<coroutine::coroutine::Handle>,
}

/// Wrapped mio IO (Evented+TryRead+TryWrite)
///
/// `Handle` is just a cloneable reference to this struct
struct IO {
    coroutine: Rc<RefCell<Coroutine>>,
    token: Token,
    io : Box<ReadWrite+'static>,
    interest: mio::Interest,
    peer_hup: bool,
}


impl IO {
    /// Handle `hup` condition
    fn hup<H>(&mut self, event_loop: &mut EventLoop<H>, token: Token)
        where H : Handler {
            if self.interest == mio::Interest::hup() {
                self.interest = mio::Interest::none();
                event_loop.deregister(&*self.io).ok().expect("deregister() failed");
            } else {
                self.peer_hup = true;
                self.reregister(event_loop, token)
            }
        }

    /// Reregister oneshot handler for the next event
    fn reregister<H>(&mut self, event_loop: &mut EventLoop<H>, token : Token)
        where H : Handler {

            self.interest = self.coroutine.borrow().state.to_interest_for(token) ;

            event_loop.reregister(
                &*self.io, token,
                self.interest, mio::PollOpt::edge() | mio::PollOpt::oneshot()
                ).ok().expect("reregister failed")
        }
}

/// `mioco` wrapper over io associated with a given coroutine.
///
/// To be used to trigger events inside `mioco` coroutine. Create using
/// `Builder::io_wrap`.
///
/// It implements `readable` and `writable`, corresponding to original `mio::Handler`
/// methods. Call these from respective `mio::Handler`.
#[derive(Clone)]
pub struct ExternalHandle {
    inn : Rc<RefCell<IO>>,
}

/// `mioco` wrapper over io associated with a given coroutine.
///
/// Passed to closure function.
///
/// It implements standard library `Read` and `Write` traits that will
/// take care of blocking and unblocking coroutine when needed. 
pub struct InternalHandle {
    inn : Rc<RefCell<IO>>,
}
impl ExternalHandle {

    /// Is this IO finished and free to be removed
    /// as no more events will be reported for it
    pub fn is_finished(&self) -> bool {
        let co = &self.inn.borrow().coroutine;
        let co_b = co.borrow();
        co_b.state == State::Finished && self.inn.borrow().interest == mio::Interest::none()
    }

    /// Access the wrapped IO
    pub fn with_raw<F>(&self, f : F)
        where F : Fn(&ReadWrite) {
        let io = &self.inn.borrow().io;
        f(&**io)
    }

    /// Access the wrapped IO as mutable
    pub fn with_raw_mut<F>(&mut self, f : F)
        where F : Fn(&mut ReadWrite) {
        let mut io = &mut self.inn.borrow_mut().io;
        f(&mut **io)
    }

    /// Readable event handler
    ///
    /// This corresponds to `mio::Hnalder::readable()`.
    pub fn readable<H>(&mut self, event_loop: &mut EventLoop<H>, token: Token, hint: mio::ReadHint)
    where H : Handler {

        if hint.is_hup() {
            let mut inn = self.inn.borrow_mut();
            inn.hup(event_loop, token);
            return;
        }


        let state = {
            let co = &self.inn.borrow().coroutine;
            let co_b = co.borrow();
            co_b.state
        };

        if let State::BlockedOnRead(blocked_token) = state {
            if token == blocked_token {
                let handle = {
                    let inn = self.inn.borrow();
                    let coroutine_handle = inn.coroutine.borrow().coroutine.as_ref().map(|c| c.clone()).unwrap();
                    inn.coroutine.borrow_mut().state = State::Running;
                    coroutine_handle
                };
                handle.resume().ok().expect("resume() failed");
            }

            let mut inn = self.inn.borrow_mut();
            inn.reregister(event_loop, token)
        } else if let State::BlockedOnWrite(blocked_token) = state {
            if token == blocked_token {
                let mut inn = self.inn.borrow_mut();
                inn.reregister(event_loop, token)
            }
        }

    }

    /// Readable event handler
    ///
    /// This corresponds to `mio::Hnalder::writable()`.
    pub fn writable<H>(&mut self, event_loop: &mut EventLoop<H>, token: Token)
    where H : Handler {

        let state = {
            let co = &self.inn.borrow().coroutine;
            let co_b = co.borrow();
            co_b.state
        };

        if let State::BlockedOnWrite(blocked_token) = state {
            if token == blocked_token {
                let handle = {
                    let inn = self.inn.borrow();
                    let coroutine_handle = inn.coroutine.borrow().coroutine.as_ref().map(|c| c.clone()).unwrap();
                    inn.coroutine.borrow_mut().state = State::Running;
                    coroutine_handle
                };
                handle.resume().ok().expect("resume() failed");

                let mut inn = self.inn.borrow_mut();
                inn.reregister(event_loop, token)
            }

        } else if let State::BlockedOnRead(blocked_token) = state {
            if token == blocked_token {
                let mut inn = self.inn.borrow_mut();
                inn.reregister(event_loop, token)
            }
        }
    }
}

impl std::io::Read for InternalHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let res = self.inn.borrow_mut().io.try_read(buf);
            match res {
                Ok(None) => {
                    {
                        let inn = self.inn.borrow();
                        inn.coroutine.borrow_mut().state = State::BlockedOnRead(inn.token);
                    }
                    coroutine::Coroutine::block();
                },
                Ok(Some(r))  => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }
}

impl std::io::Write for InternalHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            let res = self.inn.borrow_mut().io.try_write(buf) ;
            match res {
                Ok(None) => {
                    {
                        let inn = self.inn.borrow();
                        inn.coroutine.borrow_mut().state = State::BlockedOnWrite(inn.token);
                    }
                    coroutine::Coroutine::block();
                },
                Ok(Some(r)) => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }

    /* TODO: Should we pass flush to TcpStream/ignore? */
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `mioco` coroutine builder
///
/// Create one with `new`, then use `wrap_io` on io that you are going to use in the coroutine
/// that you spawn with `start`.
pub struct Builder {
    coroutine : Rc<RefCell<Coroutine>>,
    handles : Vec<InternalHandle>
}

struct RefCoroutine {
    coroutine: Rc<RefCell<Coroutine>>,
}
unsafe impl Send for RefCoroutine { }

struct HandleSender(Vec<InternalHandle>);

unsafe impl Send for HandleSender {}

impl Builder {

    /// Create new Coroutine builder
    pub fn new() -> Builder {
        Builder {
            coroutine: Rc::new(RefCell::new(Coroutine {
                state: State::Running,
                coroutine: None,
            })),
            handles: Vec::with_capacity(4),
        }
    }

    /// Register `mio`'s io to be used within `mioco` coroutine
    ///
    /// Consumes the `io`, returns a `Handle` to a mio wrapper over it.
    pub fn wrap_io<H, T : 'static>(&mut self, event_loop: &mut mio::EventLoop<H>, io : T, token : Token) -> ExternalHandle
    where H : Handler,
    T : ReadWrite {

        event_loop.register_opt(
            &io, token,
            mio::Interest::readable() | mio::Interest::writable(), mio::PollOpt::edge() | mio::PollOpt::oneshot()
            ).expect("register_opt failed");

        let io = Rc::new(RefCell::new(
                     IO {
                         coroutine: self.coroutine.clone(),
                         io: Box::new(io),
                         token: token,
                         peer_hup: false,
                         interest: mio::Interest::none(),
                     }
                 ));

        let handle = ExternalHandle {
            inn: io.clone()
        };

        self.handles.push(InternalHandle {
            inn: io.clone()
        });

        handle
    }

    /// Create a `mioco` coroutine handler
    ///
    /// `f` is routine handling connection. It should not use any blocking operations,
    /// and use it's argument for all IO with it's peer
    pub fn start<F>(self, f : F)
        where F : FnOnce(&mut [InternalHandle]) + Send + 'static {

            let ioref = RefCoroutine {
                coroutine: self.coroutine.clone(),
            };

            let handles = HandleSender(self.handles);

            let coroutine_handle = coroutine::coroutine::Coroutine::spawn(move || {
                let HandleSender(mut handles) = handles;
                ioref.coroutine.borrow_mut().coroutine = Some(coroutine::Coroutine::current().clone());
                f(&mut handles);
                ioref.coroutine.borrow_mut().state = State::Finished;
            });

            coroutine_handle.resume().ok().expect("resume() failed");
        }
}
