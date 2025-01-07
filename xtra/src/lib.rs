//! xtra is a tiny, fast, and safe actor system.
#![cfg_attr(feature = "nightly", feature(negative_impls, with_negative_coherence))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(unsafe_code)]

pub use self::address::{Address, WeakAddress};
pub use self::context::Context;
pub use self::mailbox::Mailbox;
pub use self::scoped_task::scoped;
pub use self::send_future::{ActorErasedSending, ActorNamedSending, Receiver, SendFuture};
#[allow(unused_imports)]
pub use self::spawn::*;
use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use std::any::Any;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::Poll;
use std::{fmt, mem};
// Star export so we don't have to write `cfg` attributes here.

pub mod address;
mod chan;
mod context;
mod dispatch_future;
mod envelope;
mod instrumentation;
mod mailbox;
pub mod message_channel;
mod recv_future;
/// This module contains a way to scope a future to the lifetime of an actor, stopping it before it
/// completes if the actor it is associated with stops too.
pub mod scoped_task;
mod send_future;
mod spawn;

/// Commonly used types from xtra
pub mod prelude {
    pub use crate::address::Address;
    pub use crate::context::Context;
    pub use crate::message_channel::MessageChannel;
    #[doc(no_inline)]
    pub use crate::{Actor, Handler, Mailbox};

    #[cfg(feature = "nightly")]
    pub use crate::HandlerMut;
}

/// This module contains types representing the strength of an address's reference counting, which
/// influences whether the address will keep the actor alive for as long as it lives.
pub mod refcount {
    pub use crate::chan::{RefCounter, TxEither as Either, TxStrong as Strong, TxWeak as Weak};
}

use crate::dispatch_future::DispatchFuture;
use crate::recv_future::Message;
/// Provides a default implementation of the [`Actor`] trait for the given type with a [`Stop`](Actor::Stop) type of `()` and empty lifecycle functions.
///
/// The [`Actor`] custom derive takes away some boilerplate for a standard actor:
///
/// ```rust
/// #[derive(xtra::Actor)]
/// pub struct MyActor;
/// #
/// # fn assert_actor<T: xtra::Actor>() { }
/// #
/// # fn main() {
/// #    assert_actor::<MyActor>()
/// # }
/// ```
/// This macro will generate the following [`Actor`] implementation:
///
/// ```rust,no_run
/// # use xtra::prelude::*;
/// pub struct MyActor;
///
///
/// impl xtra::Actor for MyActor {
///     type Stop = ();
///
///     async fn stopped(self) { }
/// }
/// ```
///
/// Please note that implementing the [`Actor`] trait is still very easy and this macro purposely does not support a plethora of usecases but is meant to handle the most common ones.
/// For example, whilst it does support actors with type parameters, lifetimes are entirely unsupported.
#[cfg(feature = "macros")]
pub use macros::Actor;

/// Defines that an [`Actor`] can handle a given message `M`.
///
/// # Example
///
/// ```rust
/// # use xtra::prelude::*;
/// # struct MyActor;
/// #  impl Actor for MyActor {type Stop = (); async fn stopped(self) -> Self::Stop {} }
/// struct Msg;
///
///
/// impl Handler<Msg> for MyActor {
///     type Return = u32;
///
///     async fn handle(&mut self, message: Msg, ctx: &mut Context<Self>) -> u32 {
///         20
///     }
/// }
///
/// fn main() {
/// #   #[cfg(feature = "smol")]
///     smol::block_on(async {
///         let addr = xtra::spawn_smol(MyActor, Mailbox::unbounded());
///         assert_eq!(addr.send(Msg).await, Ok(20));
///     })
/// }
/// ```
pub trait Handler<M>: Actor {
    /// The return value of this handler.
    type Return: Send + 'static;

    /// Handle a given message, returning its result.
    fn handle(
        &self,
        message: M,
        ctx: &mut Context<Self>,
    ) -> impl Future<Output = Self::Return> + Send;
}

pub trait HandlerAny<M>: Actor + Any {
    type Return: Send + 'static;

    fn handle(
        act: &GuardedActor<Self>,
        message: M,
        ctx: &mut Context<Self>,
    ) -> impl Future<Output = Self::Return> + Send;
}

impl<M, R, H> HandlerAny<M> for H
where
    H: Handler<M, Return = R>,
    R: Send + 'static,
    M: Send,
{
    type Return = R;

    async fn handle(act: &GuardedActor<Self>, message: M, ctx: &mut Context<Self>) -> Self::Return {
        let act = act.read().await;
        act.handle(message, ctx).await
    }
}

#[cfg(feature = "nightly")]
pub trait HandlerMut<M>: Actor {
    /// The return value of this handler.
    type Return: Send + 'static;

    /// Handle a given message, returning its result.
    fn handle_mut(
        &mut self,
        message: M,
        ctx: &mut Context<Self>,
    ) -> impl Future<Output = Self::Return> + Send;
}

#[cfg(feature = "nightly")]
impl<A, M> !Handler<M> for A
where
    A: HandlerMut<M>,
    A: Actor,
{
}

#[cfg(feature = "nightly")]
impl<A, M> !HandlerMut<M> for A
where
    A: Handler<M>,
    A: Actor,
{
}

#[cfg(feature = "nightly")]
impl<M, R, H> HandlerAny<M> for H
where
    H: HandlerMut<M, Return = R>,
    R: Send + 'static,
    M: Send,
{
    type Return = R;

    async fn handle(act: &GuardedActor<Self>, message: M, ctx: &mut Context<Self>) -> Self::Return {
        let mut act = act.write().await;
        act.handle_mut(message, ctx).await
    }
}

/// An actor which can handle message one at a time. Actors can only be
/// communicated with by sending messages through their [`Address`]es.
/// They can modify their private state, respond to messages, and spawn other actors. They can also
/// stop themselves through their [`Context`] by calling [`Context::stop_self`].
/// This will result in any attempt to send messages to the actor in future failing.
///
/// # Example
///
/// ```rust
/// # use xtra::prelude::*;
/// # use std::time::Duration;
/// struct MyActor;
///
///
/// impl Actor for MyActor {
///     type Stop = ();
///     async fn started(&mut self, ctx: &Mailbox<Self>) -> Result<(), Self::Stop> {
///         println!("Started!");
///
///         Ok(())
///     }
///
///     async fn stopped(self) -> Self::Stop {
///         println!("Finally stopping.");
///     }
/// }
///
/// struct Goodbye;
///
///
/// impl Handler<Goodbye> for MyActor {
///     type Return = ();
///
///     async fn handle(&mut self, _: Goodbye, ctx: &mut Context<Self>) {
///         println!("Goodbye!");
///         ctx.stop_all();
///     }
/// }
///
/// // Will print "Started!", "Goodbye!", and then "Finally stopping."
/// # #[cfg(feature = "smol")]
/// smol::block_on(async {
///     let addr = xtra::spawn_smol(MyActor, Mailbox::unbounded());
///     addr.send(Goodbye).await;
///
///     smol::Timer::after(Duration::from_secs(1)).await; // Give it time to run
/// })
/// ```
///
/// For longer examples, see the `examples` directory.
pub trait Actor: 'static + Send + Sync + Sized {
    /// Value returned from the actor when [`Actor::stopped`] is called.
    type Stop: Send + 'static;

    /// Called as soon as the actor has been started.
    #[allow(unused_variables)]
    fn started(
        &mut self,
        mailbox: &Mailbox<Self>,
    ) -> impl Future<Output = Result<(), Self::Stop>> + Send {
        async { Ok(()) }
    }

    /// Called at the end of an actor's event loop.
    ///
    /// An actor's event loop can stop for several reasons:
    ///
    /// - The actor called [`Context::stop_self`].
    /// - An actor called [`Context::stop_all`].
    /// - The last [`Address`] with a [`Strong`](crate::refcount::Strong) reference count was dropped.
    fn stopped(self) -> impl Future<Output = Self::Stop> + Send;
}

type GuardedActor<A> = async_lock::RwLock<A>;

/// An error related to the actor system
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Error {
    /// The actor is no longer running and disconnected from the sending address.
    Disconnected,
    /// The message request operation was interrupted. This happens when the message result sender
    /// is dropped. Therefore, it should never be returned from [`detached`](SendFuture::detach) [`SendFuture`]s
    /// This could be due to the actor's event loop being shut down, or due to a custom timeout.
    /// Unlike [`Error::Disconnected`], it does not necessarily imply that any retries or further
    /// attempts to interact with the actor will result in an error.
    Interrupted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Disconnected => f.write_str("Actor address disconnected"),
            Error::Interrupted => f.write_str("Message request interrupted"),
        }
    }
}

impl std::error::Error for Error {}

/// Run the provided actor.
///
/// This is the primary event loop of an actor which takes messages out of the mailbox and hands
/// them to the actor.
pub async fn run<A, L>(mailbox: Mailbox<A>, mut actor: A, concurrency_limit: L) -> A::Stop
where
    A: Actor,
    L: Into<Option<usize>>,
{
    if let Err(stop) = actor.started(&mailbox).await {
        return stop;
    }

    let concurrency_limit = concurrency_limit
        .into()
        .filter(|l| *l > 0)
        .unwrap_or(usize::MAX);

    let guarded_actor = GuardedActor::new(actor);

    ConcurrentProcessor::new(&guarded_actor, mailbox, concurrency_limit).await;

    let actor = guarded_actor.into_inner();
    actor.stopped().await
}

struct ConcurrentProcessor<'a, A> {
    actor: &'a GuardedActor<A>,
    state: ProcessingState<'a, A>,
}

enum ProcessingState<'a, A> {
    Active {
        mailbox: Mailbox<A>,
        active: FuturesUnordered<DispatchFuture<'a, A>>,
        limit: usize,
    },
    Closing {
        active: FuturesUnordered<DispatchFuture<'a, A>>,
    },
    Done,
}

impl<'a, A: Actor> ConcurrentProcessor<'a, A> {
    fn new(actor: &'a GuardedActor<A>, mailbox: Mailbox<A>, limit: usize) -> Self {
        Self {
            actor,
            state: ProcessingState::Active {
                mailbox,
                active: FuturesUnordered::new(),
                limit,
            },
        }
    }
}

impl<'a, A: Actor> Future for ConcurrentProcessor<'a, A> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        loop {
            match mem::replace(&mut self.state, ProcessingState::Done) {
                ProcessingState::Active {
                    mailbox,
                    mut active,
                    limit,
                } => {
                    let mut pending = false;

                    if !active.is_empty() {
                        match active.poll_next_unpin(cx) {
                            Poll::Ready(Some(ControlFlow::Break(_))) => {
                                self.state = ProcessingState::Closing { active };
                                continue;
                            }
                            Poll::Pending => {
                                pending = true;
                            }
                            _ => {}
                        }
                    }

                    if active.len() < limit || active.is_empty() {
                        match mailbox.next().poll_unpin(cx) {
                            Poll::Ready(m) => {
                                active.push(m.dispatch_to(&self.actor));
                                pending = false;
                            }
                            Poll::Pending => {
                                pending = true;
                            }
                        }
                    }

                    let is_active = !active.is_empty();

                    self.state = ProcessingState::Active {
                        mailbox,
                        active,
                        limit,
                    };

                    if pending {
                        return Poll::Pending;
                    }

                    if !is_active {
                        panic!("FuturesUnordered should never be inactive at this point");
                    }
                }
                ProcessingState::Closing { mut active } => match active.poll_next_unpin(cx) {
                    Poll::Ready(None) => {
                        self.state = ProcessingState::Done;
                        return Poll::Ready(());
                    }
                    Poll::Ready(Some(_)) => self.state = ProcessingState::Closing { active },
                    Poll::Pending => {
                        self.state = ProcessingState::Closing { active };
                        return Poll::Pending;
                    }
                },
                ProcessingState::Done => {
                    panic!("polled after completion");
                }
            }
        }
    }
}
