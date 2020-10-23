use std::sync::{Arc, Mutex};

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
///         |state, msg| match msg {
///             Msg::Ping => State {
///                 pings: state.pings + 1,
///                 pongs: state.pongs,
///             },
///             Msg::Pong => State {
///                 pings: state.pings,
///                 pongs: state.pongs + 1,
///             },
///         },
///         |in_stream| {
///             in_stream
///                 .filter(|msg| {
///                     ready(match msg {
///                         Msg::Ping => true,
///                         _ => false,
///                     })
///                 })
///                 .map(|_| Msg::Pong)
///         },
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
pub fn mox_stream<State: 'static, Msg: 'static + Clone, OutStream>(
    initial_state: State,
    reducer: impl Fn(&State, Msg) -> State + 'static,
    operator: impl FnOnce(mpsc::UnboundedReceiver<Msg>) -> OutStream,
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
        let p = Arc::new(Mutex::new(action_producer));
        let pc = p.clone();

        let (mut operated_action_producer, operated_action_consumer): (
            mpsc::UnboundedSender<Msg>,
            mpsc::UnboundedReceiver<Msg>,
        ) = mpsc::unbounded();

        let _ = load_once(move || {
            action_consumer.for_each(move |msg| {
                accessor.update(|cur| Some(reducer(cur, msg.clone())));
                let _ = operated_action_producer.start_send(msg);
                ready(())
            })
        });

        let _ = load_once(move || {
            operator(operated_action_consumer).for_each(move |msg| {
                let _ = pc.lock().unwrap().start_send(msg);
                ready(())
            })
        });

        move |msg| {
            let _ = p.lock().unwrap().start_send(msg);
        }
    });

    (current_state, dispatch)
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
                |state, msg| match msg {
                    Msg::Increment => state + 1,
                    Msg::Decrement => state - 1,
                },
                |in_stream| in_stream.filter(|_| ready(false)),
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
                |state, msg| match msg {
                    Msg::Ping => State {
                        pings: state.pings + 1,
                        pongs: state.pongs,
                    },
                    Msg::Pong => State {
                        pings: state.pings,
                        pongs: state.pongs + 1,
                    },
                },
                |in_stream| {
                    in_stream
                        .filter(|msg| {
                            ready(match msg {
                                Msg::Ping => true,
                                _ => false,
                            })
                        })
                        .map(|_| Msg::Pong)
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
}
