use std::cell::RefCell;
use std::rc::Rc;

use futures::{channel::mpsc, future::ready, stream::Stream, StreamExt};
use moxie::{load_once, once, state, Commit};

/// Root a state variable at the callsite, returning a `dispatch` function which may be used to send
/// messages to be processed by the provided `reducer` and `operator`.
///
/// For each message dispatched, the provided `reducer` will be invoked with a reference to the current
/// state and the message. The state will then be updated with the return value from the `reducer`.
///
/// The `operator` receives an `mpsc::UnboundedReceiver<Msg>` stream of messages which have _already_
/// been processed by the `reducer`, and returns a stream of messages which will begin the cycle over
/// again (piped to dispatch).
///
/// # Example
///
/// ```
/// use futures::{executor::LocalPool, StreamExt, future::ready};
/// use moxie::runtime::RunLoop;
/// use moxie_streams::mox_stream;
///
/// #[derive(Clone)]
/// enum Msg {
///     Ping,
///     Pong,
/// }
///
/// struct State {
///     pings: usize,
///     pongs: usize,
/// }
///
/// let mut rt = RunLoop::new(|| {
///     mox_stream(
///         State { pings: 0, pongs: 0 },
///         |state, msg| match *msg {
///             Msg::Ping => State {
///                 pings: state.pings + 1,
///                 pongs: state.pongs,
///             },
///             Msg::Pong => State {
///                 pings: state.pings,
///                 pongs: state.pongs + 1,
///             },
///         },
///         || Box::new(|in_stream| {
///             in_stream
///                 .filter(|msg| {
///                     ready(match **msg {
///                         Msg::Ping => true,
///                         _ => false,
///                     })
///                 })
///                 .map(|_| Msg::Pong)
///         }),
///     )
/// });
///
/// let mut exec = LocalPool::new();
/// rt.set_task_executor(exec.spawner());
///
/// exec.run_until_stalled();
///
/// let (first_commit, dispatch) = rt.run_once();
/// assert_eq!(first_commit.pings, 0);
/// assert_eq!(first_commit.pongs, 0);
///
/// dispatch(Msg::Pong);
/// exec.run_until_stalled();
///
/// let (second_commit, _) = rt.run_once();
/// assert_eq!(second_commit.pings, 0);
/// assert_eq!(second_commit.pongs, 1);
///
/// dispatch(Msg::Ping);
/// exec.run_until_stalled();
///
/// let (third_commit, _) = rt.run_once();
/// assert_eq!(third_commit.pings, 1);
/// assert_eq!(third_commit.pongs, 2);
/// ```
pub fn mox_stream<State: 'static, Msg: 'static, OutStream>(
    initial_state: State,
    reducer: impl Fn(&State, Rc<Msg>) -> State + 'static,
    get_operator: impl Fn() -> Box<dyn FnOnce(mpsc::UnboundedReceiver<Rc<Msg>>) -> OutStream>,
) -> (Commit<State>, impl Fn(Msg))
where
    OutStream: Stream<Item = Msg> + 'static,
{
    let (current_state, accessor) = state(|| initial_state);

    let dispatch = once(|| {
        let (action_producer, action_consumer): (
            mpsc::UnboundedSender<Msg>,
            mpsc::UnboundedReceiver<Msg>,
        ) = mpsc::unbounded();
        let p = Rc::new(RefCell::new(action_producer));
        let pc = p.clone();

        let (mut operated_action_producer, operated_action_consumer): (
            mpsc::UnboundedSender<Rc<Msg>>,
            mpsc::UnboundedReceiver<Rc<Msg>>,
        ) = mpsc::unbounded();

        let _ = load_once(move || {
            action_consumer.for_each(move |msg| {
                let mrc = Rc::new(msg);
                accessor.update(|cur| Some(reducer(cur, mrc.clone())));
                let _ = operated_action_producer.start_send(mrc);
                ready(())
            })
        });

        let _ = load_once(move || {
            get_operator()(operated_action_consumer).for_each(move |msg| {
                let _ = pc.borrow_mut().start_send(msg);
                ready(())
            })
        });

        move |msg| {
            let _ = p.borrow_mut().start_send(msg);
        }
    });

    (current_state, dispatch)
}

#[macro_export]
macro_rules! combine_operators {
    ( $( $x:expr ),* ) => {
        {
            let mut in_producers = vec![];
            let (out_producer, out_consumer): (
                futures::channel::mpsc::UnboundedSender<Msg>,
                futures::channel::mpsc::UnboundedReceiver<Msg>,
            ) = futures::channel::mpsc::unbounded();
            let op = std::rc::Rc::new(std::cell::RefCell::new(out_producer));
            $(
                let out = op.clone();
                let (p, c): (
                    futures::channel::mpsc::UnboundedSender<Rc<Msg>>,
                    futures::channel::mpsc::UnboundedReceiver<Rc<Msg>>,
                ) = futures::channel::mpsc::unbounded();
                let _ = load_once(|| ($x)(c).for_each(move |msg| {
                    let _ = out.borrow_mut().start_send(msg);
                    futures::future::ready(())
                }));
                in_producers.push(p);
            )*
            move |in_stream: futures::channel::mpsc::UnboundedReceiver<Rc<Msg>>| {
                let _ = moxie::load_once(move || in_stream.for_each(move |mrc| {
                    in_producers.iter_mut().for_each(|p| {
                        let _ = p.start_send(mrc.clone());
                    });
                    futures::future::ready(())
                }));
                out_consumer
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::LocalPool;
    use moxie::runtime::RunLoop;

    #[test]
    fn state_update() {
        #[derive(Clone)]
        enum Msg {
            Increment,
            Decrement,
        }

        let mut rt = RunLoop::new(|| {
            mox_stream(
                0,
                |state, msg| match *msg {
                    Msg::Increment => state + 1,
                    Msg::Decrement => state - 1,
                },
                || Box::new(|in_stream| in_stream.filter(|_| ready(false)).map(|_| Msg::Decrement)),
            )
        });

        let mut exec = LocalPool::new();
        rt.set_task_executor(exec.spawner());

        exec.run_until_stalled();

        let (first_commit, dispatch) = rt.run_once();
        assert_eq!(*first_commit, 0);

        dispatch(Msg::Increment);
        exec.run_until_stalled();

        let (second_commit, _) = rt.run_once();
        assert_eq!(*second_commit, 1);

        dispatch(Msg::Decrement);
        exec.run_until_stalled();

        let (third_commit, _) = rt.run_once();
        assert_eq!(*third_commit, 0);
    }

    #[test]
    fn operator() {
        #[derive(Clone)]
        enum Msg {
            Ping,
            Pong,
        }

        struct State {
            pings: usize,
            pongs: usize,
        }

        let mut rt = RunLoop::new(|| {
            mox_stream(
                State { pings: 0, pongs: 0 },
                |state, msg| match *msg {
                    Msg::Ping => State {
                        pings: state.pings + 1,
                        pongs: state.pongs,
                    },
                    Msg::Pong => State {
                        pings: state.pings,
                        pongs: state.pongs + 1,
                    },
                },
                || {
                    Box::new(|in_stream| {
                        in_stream
                            .filter(|msg| {
                                ready(match **msg {
                                    Msg::Ping => true,
                                    _ => false,
                                })
                            })
                            .map(|_| Msg::Pong)
                    })
                },
            )
        });

        let mut exec = LocalPool::new();
        rt.set_task_executor(exec.spawner());

        exec.run_until_stalled();

        let (first_commit, dispatch) = rt.run_once();
        assert_eq!(first_commit.pings, 0);
        assert_eq!(first_commit.pongs, 0);

        dispatch(Msg::Pong);
        exec.run_until_stalled();

        let (second_commit, _) = rt.run_once();
        assert_eq!(second_commit.pings, 0);
        assert_eq!(second_commit.pongs, 1);

        dispatch(Msg::Ping);
        exec.run_until_stalled();

        let (third_commit, _) = rt.run_once();
        assert_eq!(third_commit.pings, 1);
        assert_eq!(third_commit.pongs, 2);
    }

    #[test]
    fn combine_operator() {
        #[derive(Clone)]
        enum Msg {
            Tic,
            Tac,
            Toe,
        }

        struct State {
            tic: usize,
            tac: usize,
            toe: usize,
        }

        let tic_operator = |in_stream: mpsc::UnboundedReceiver<Rc<Msg>>| {
            in_stream
                .filter(|mrc| match **mrc {
                    Msg::Tic => ready(true),
                    _ => ready(false),
                })
                .map(|_| Msg::Tac)
        };
        let tac_operator = |in_stream: mpsc::UnboundedReceiver<Rc<Msg>>| {
            in_stream
                .filter(|mrc| match **mrc {
                    Msg::Tac => ready(true),
                    _ => ready(false),
                })
                .map(|_| Msg::Toe)
        };

        let mut rt = RunLoop::new(|| {
            mox_stream(
                State {
                    tic: 0,
                    tac: 0,
                    toe: 0,
                },
                |state, msg| match *msg {
                    Msg::Tic => State {
                        tic: state.tic + 1,
                        tac: state.tac,
                        toe: state.toe,
                    },
                    Msg::Tac => State {
                        tic: state.tic,
                        tac: state.tac + 1,
                        toe: state.toe,
                    },
                    Msg::Toe => State {
                        tic: state.tic,
                        tac: state.tac,
                        toe: state.toe + 1,
                    },
                },
                || Box::new(combine_operators!(tic_operator, tac_operator)),
            )
        });

        let mut exec = LocalPool::new();
        rt.set_task_executor(exec.spawner());

        exec.run_until_stalled();

        let (first_commit, dispatch) = rt.run_once();
        assert_eq!(first_commit.tic, 0);
        assert_eq!(first_commit.tac, 0);
        assert_eq!(first_commit.toe, 0);

        dispatch(Msg::Toe);
        exec.run_until_stalled();

        let (second_commit, _) = rt.run_once();
        assert_eq!(second_commit.tic, 0);
        assert_eq!(second_commit.tac, 0);
        assert_eq!(second_commit.toe, 1);

        dispatch(Msg::Tic);
        exec.run_until_stalled();

        let (third_commit, _) = rt.run_once();
        assert_eq!(third_commit.tic, 1);
        assert_eq!(third_commit.tac, 1);
        assert_eq!(third_commit.toe, 2);
    }
}
