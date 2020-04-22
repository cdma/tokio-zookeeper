use failure;
use failure::format_err;

use futures::{
    channel::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot::{self, Sender},
    },
    executor,
    future::{AbortHandle, Abortable},
    lock::Mutex as AsyncMutex,
    sink::SinkExt,
    stream::StreamExt,
};
use slog;
use slog::{debug, error, info, Logger};
use std::collections::{HashMap, HashSet};
use std::mem;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Barrier;
use tokio::time;
use tokio_util::codec::{FramedRead, FramedWrite};

// TODO clean up imports
use crate::error::{InternalError, ZkError};
use crate::proto::decoder::ZkDecoder;
use crate::proto::encoder::{RequestWrapper, ZkEncoder};
use crate::proto::{
    request::{OpCode, Request},
    response::{Response, FIRST_XID, HEARTBEAT_XID, SHUTDOWN_XID, WATCH_XID},
};
use crate::session_manager::SessionManager;
use crate::types::watch::{Watch, WatchOption, WatchType, WatchedEvent};

//
// This struct exists so the enqueuer can have a handle to the client's
// background tasks. When the enqueuer is dropped, this struct is dropped, and
// its drop() method ends the session and kills the background tasks.
//
// Note that, while all of this struct's members are Clone, this struct
// **is not Clone**. The user may clone Enqueuers, and we want all of them to
// maintain a single reference to this struct, so we avoid cloning this struct
// and put it in an Arc instead.
//
#[derive(Debug)]
struct TaskTracker {
    abort_handles: Arc<Mutex<Option<(AbortHandle, AbortHandle)>>>,
    sess_mgr: SessionManager,
}

impl Drop for TaskTracker {
    fn drop(&mut self) {
        println!("aborting!");
        if let Some((h1, h2)) = &*self.abort_handles.lock().unwrap() {
            h1.abort();
            h2.abort();
        }
        println!("aborted");
        executor::block_on(self.sess_mgr.close_session());
        println!("drop finished");
    }
}

#[derive(Clone)]
pub(crate) struct SharedState {
    rx: Arc<AsyncMutex<UnboundedReceiver<(Request, Sender<Result<Response, ZkError>>)>>>,
    ///
    /// Clone of client-facing tx to let us send server requests internally
    ///
    req_tx: UnboundedSender<(Request, Sender<Result<Response, ZkError>>)>,
    ///
    /// Global map of operations awaiting response, indexed by xid
    ///
    replies: Arc<Mutex<HashMap<i32, (OpCode, Sender<Result<Response, ZkError>>)>>>,
    ///
    /// Next xid to issue
    ///
    xid: Arc<AsyncMutex<i32>>,

    sess_mgr: SessionManager,
    ///
    /// Global map of pending watches.
    ///
    /// Watches are only registered once we have confirmed that the operation
    /// that initiated the watch did not fail. Thust, we must stage watches here
    /// first. The map is indexed by xid.
    ///
    /// The one exception: a watch can still be added if a call to exists()
    /// fails because the node does not exist yet.
    ///
    pending_watches: Arc<Mutex<HashMap<i32, (String, Watch)>>>,
    ///
    /// Global map of watches registered, indexed by path
    ///
    watches: Arc<Mutex<HashMap<String, Vec<Watch>>>>,
    ///
    /// Default watcher for state change events and non-custom watch events
    ///
    default_watcher: UnboundedSender<WatchedEvent>,
    addr: SocketAddr,
    log: slog::Logger,
}

impl SharedState {
    pub(crate) async fn start(
        addr: SocketAddr,
        default_watcher: UnboundedSender<WatchedEvent>,
        session_timeout: Duration,
        read_only: bool,
        log: slog::Logger,
    ) -> Enqueuer {
        //
        // This thread will wait at the barrier until the background thread
        // connects to the ZooKeeper server.
        //
        let barrier = Arc::new(Barrier::new(2));
        let bg_barrier = Arc::clone(&barrier);

        let xid = Arc::new(AsyncMutex::new(FIRST_XID));

        let (tx, rx) = mpsc::unbounded();

        let sess_mgr = SessionManager::new(
            addr,
            Arc::clone(&xid),
            session_timeout,
            read_only,
            log.clone(),
        );
        // TODO explain this nested arc stuff
        let abort_handles = Arc::new(Mutex::new(None));
        let enqueuer = Enqueuer {
            tx: tx.clone(),
            task_tracker: Arc::new(TaskTracker {
                abort_handles: Arc::clone(&abort_handles),
                sess_mgr: sess_mgr.clone(),
            }),
        };

        tokio::task::spawn(async move {
            let mut s = SharedState {
                xid,
                req_tx: tx,
                rx: Arc::new(AsyncMutex::new(rx)),
                log: log.clone(),
                sess_mgr,
                replies: Arc::new(Mutex::new(HashMap::new())),
                pending_watches: Arc::new(Mutex::new(HashMap::new())),
                watches: Arc::new(Mutex::new(HashMap::new())),
                default_watcher,
                addr,
            };
            match s.run(bg_barrier, abort_handles).await {
                // TODO destroy watches so any waiting channels resolve with
                // error
                Err(e) => error!(log, "Client exiting with error: {:?}", e),
                Ok(_) => info!(log, "Client exiting"),
            }
        });
        barrier.wait().await;

        // TODO should this function ever return an error?

        enqueuer
    }

    async fn run(
        &mut self,
        barrier: Arc<Barrier>,
        abort_handles: Arc<Mutex<Option<(AbortHandle, AbortHandle)>>>,
    ) -> Result<(), InternalError> {
        let mut first = true;
        loop {
            if self.sess_mgr.is_exited().await {
                return Ok(());
            }

            //
            // If we really can't reconnect, we have no choice but to exit.
            //
            let (tx, rx) = self.sess_mgr.reconnect().await.map_err(|e| {
                //
                // We need to clear these before we exit so any inflight
                // requests sent using req_tx (e.g. reregister_watches) get
                // canceled. We need to make sure they get canceled so all
                // associated threads exit and the SharedState gets dropped,
                // ensuring proper shutdown.
                //
                self.replies.lock().unwrap().clear();
                self.pending_watches.lock().unwrap().clear();
                e
            })?;

            //
            // Allow the initial call to start() to resolve
            //
            if first {
                first = false;
                barrier.wait().await;
            } else {
                self.replies.lock().unwrap().clear();
                self.pending_watches.lock().unwrap().clear();
                let mut reregister_state = self.clone();
                tokio::task::spawn(async move {
                    if let Err(e) = reregister_state.reregister_watches().await {
                        error!(
                            reregister_state.log,
                            "Error re-registering watches: {:?}", e
                        );
                    }
                });
            }
            // TODO I will have to think about a lot of edge cases with interrupted
            // reconnect loop state probably

            let mut enc_state = self.clone();
            let mut dec_state = self.clone();

            let (enc_abort_handle, enc_abort_registration) = AbortHandle::new_pair();
            let (dec_abort_handle, dec_abort_registration) = AbortHandle::new_pair();

            {
                let mut abort_handles = abort_handles.lock().unwrap();
                *abort_handles = Some((enc_abort_handle.clone(), dec_abort_handle.clone()));
            }

            let enc_task_handle = tokio::task::spawn(Abortable::new(
                async move { enc_state.run_encoder(tx, dec_abort_handle).await },
                enc_abort_registration,
            ));

            let dec_task_handle = tokio::task::spawn(Abortable::new(
                async move { dec_state.run_decoder(rx, enc_abort_handle).await },
                dec_abort_registration,
            ));

            //
            // These futures below _never exit unless they fail_, which is why
            // they return an error as their "Ok" value.
            //
            // If these futures are aborted, we don't need to do anything other
            // than begin the reconnect loop again.
            //
            // TODO the error layering here is wrongly interpreted
            // TODO simplify abort logic -- just wait for a reliable failure
            // (e.g. the decoder) and then abort the encoder, rather than
            // having them fine-grained abort each other? Not sure if that
            // would work ok.
            if let Ok(e) = dec_task_handle.await {
                println!("Decoder error: {:?}", e);
                error!(self.log, "Decoder error: {:?}", e);
            } else {
                println!("decoder future aborted");
                debug!(self.log, "decoder future aborted");
            }
            if let Ok(e) = enc_task_handle.await {
                println!("Encoder error: {:?}", e);
                error!(self.log, "Encoder error: {:?}", e);
            } else {
                println!("encoder future aborted");
                debug!(self.log, "encoder future aborted");
            }
        }
    }

    // TODO handle weirdness described here: https://github.com/joyent/node-zkstream/blob/fe7dadcfd59af3632302f807fad33e51f5b41be3/lib/zk-session.js#L497-L526
    async fn reregister_watches(&mut self) -> Result<(), InternalError> {
        let mut data = HashSet::new();
        let mut exists = HashSet::new();
        let mut children = HashSet::new();
        for (path, watchlist) in self.watches.lock().unwrap().iter() {
            for watch in watchlist {
                let set = match watch.wtype {
                    WatchType::Child => &mut children,
                    WatchType::Data => &mut data,
                    WatchType::Exist => &mut exists,
                };
                set.insert(path.clone());
            }
        }
        if data.is_empty() && exists.is_empty() && children.is_empty() {
            return Ok(());
        }
        let req = Request::SetWatches {
            last_zxid_seen: self.sess_mgr.get_zxid().await,
            data,
            exists,
            children,
        };
        let (tx, rx) = oneshot::channel();
        self.req_tx.unbounded_send((req, tx))?;
        rx.await??;
        Ok(())
    }

    async fn run_decoder(
        &mut self,
        rx: ReadHalf<TcpStream>,
        enc_abort_handle: AbortHandle,
    ) -> InternalError {
        async fn recv_msg(
            decoder: &mut FramedRead<ReadHalf<TcpStream>, ZkDecoder>,
            sess_mgr: SessionManager,
            log: Logger,
        ) -> Result<(), InternalError> {
            match decoder.next().await {
                Some(item) => {
                    match item? {
                        //
                        // The decoder encountered some server error to be
                        // handled internally, or client logic error. We can't
                        // really do anything, so we just log the error.
                        //
                        // TODO I'm not sure that these should ever happen --
                        // should we panic instead in decoder if they do?
                        Err(e) => error!(log, "Server Error; doing nothing: {:?}", e),
                        Ok(zxid) => sess_mgr.set_zxid(zxid).await,
                    }
                    Ok(())
                }
                None => Err(InternalError::ConnectionEnded),
            }
        }

        let mut decoder = FramedRead::new(
            rx,
            ZkDecoder::new(
                Arc::clone(&self.replies),
                Arc::clone(&self.watches),
                Arc::clone(&self.pending_watches),
                self.default_watcher.clone(),
                self.log.clone(),
            ),
        );

        loop {
            if let Err(e) = recv_msg(&mut decoder, self.sess_mgr.clone(), self.log.clone()).await {
                //
                // The stream encountered an unrecoverable error. We stop the
                // encoder and then exit ourselves.
                //
                enc_abort_handle.abort();
                return e;
            }
        }
    }

    async fn run_encoder(
        &mut self,
        tx: WriteHalf<TcpStream>,
        dec_abort_handle: AbortHandle,
    ) -> InternalError {
        let mut encoder = FramedWrite::new(tx, ZkEncoder::new());
        //
        // This only changes upon reconnect, and this function doesn't run
        // across reconnects, so it's safe to only fetch the heartbeat interval
        // once.
        //
        let heartbeat_interval = self.sess_mgr.get_heartbeat_interval().await;
        // TODO look into SinkExt::with() and send_all() instead of looping manually?
        loop {
            let timeout_result =
                time::timeout(heartbeat_interval, self.rx.lock().await.next()).await;
            let (mut request, response_tx) = match timeout_result {
                Err(_) => {
                    //
                    // Heartbeat interval elapsed without us having anything to
                    // send. Send a heartbeat!
                    //
                    debug!(self.log, "Sending heartbeat");
                    if let Err(e) = encoder
                        .send(RequestWrapper {
                            xid: HEARTBEAT_XID,
                            req: Request::Ping,
                        })
                        .await
                    {
                        dec_abort_handle.abort();
                        return InternalError::from(e);
                    }
                    continue;
                }
                Ok(tuple) => {
                    tuple.expect("internal enqueuer rx dropped")
                    //
                    // TODO I'm not sure if this is right
                    // The user dropped the handle to zk.
                    // TODO mark client for shutdown once there are no watches
                    // left. Or, just shut it down immediately -- who is
                    // going to drop the zk handle but keep a watch handle
                    // around?
                    // TODO make sure to send closesession before shutting down
                    //
                    // info!(self.log, "ZK handle dropped; client exiting");

                    // dec_abort_handle.abort();
                    // return;
                }
            };

            let mut xid_handle = self.xid.lock().await;
            // Skip special xids
            while *xid_handle == SHUTDOWN_XID
                || *xid_handle == WATCH_XID
                || *xid_handle == HEARTBEAT_XID
            {
                *xid_handle += 1;
            }
            let new_xid = *xid_handle;
            *xid_handle += 1;
            debug!(self.log, "enqueueing request {:?}", request; "xid" => new_xid);

            //
            // Register a watch, if necessary
            //
            match request {
                Request::GetData {
                    ref path,
                    ref mut watch,
                    ..
                }
                | Request::GetChildren {
                    ref path,
                    ref mut watch,
                    ..
                }
                | Request::Exists {
                    ref path,
                    ref mut watch,
                    ..
                } => {
                    if let WatchOption::Oneshot(_) = *watch {
                        //
                        // Replace the request's watch field so we can use the
                        // Sender separately from the request below. It's safe
                        // to replace with WatchOption::Global because all watch
                        // types other than WatchOption::None are serialized
                        // identically.
                        //
                        let w = mem::replace(watch, WatchOption::Global);
                        if let WatchOption::Oneshot(tx) = w {
                            let wtype = match request {
                                Request::GetData { .. } => WatchType::Data,
                                Request::GetChildren { .. } => WatchType::Child,
                                Request::Exists { .. } => WatchType::Exist,
                                _ => unreachable!(),
                            };
                            debug!(
                                self.log,
                                "adding pending watch";
                                "xid" => new_xid,
                                "path" => path,
                                "wtype" => ?wtype
                            );
                            self.pending_watches
                                .lock()
                                .unwrap()
                                .insert(new_xid, (path.to_string(), Watch { wtype, tx }));
                        } else {
                            unreachable!();
                        }
                    }
                }
                _ => {}
            }

            //
            // Store response info
            //
            if let Request::Connect { .. } = request {
                unreachable!("connect request sent over client-facing channel");
            } else {
                let mut replies = self.replies.lock().unwrap();
                let old = replies.insert(new_xid, (request.opcode(), response_tx));
                assert!(old.is_none());
            };

            // XXX we should really send this in the background but then we have
            // to handle encoder lifetime/ownership. Blah!
            if let Err(e) = encoder
                .send(RequestWrapper {
                    xid: new_xid,
                    req: request,
                })
                .await
            {
                dec_abort_handle.abort();
                return InternalError::from(e);
            }
        }
    }
}

// impl<S> Future for Packetizer<S>
// where
//     S: ZooKeeperTransport,
// {
//     type Output = Result<(), failure::Error>;

//     fn poll(&mut self) -> Poll<Self::Output> {
//         trace!(self.logger, "packetizer polled");
//         if !self.exiting {
//             trace!(self.logger, "poll_enqueue");
//             match self.poll_enqueue() {
//                 Ok(_) => {}
//                 Err(()) => {
//                     // no more requests will be enqueued
//                     self.exiting = true;

//                     if let PacketizerState::Connected(ref mut ap) = self.state {
//                         // send CloseSession
//                         // length is fixed
//                         ap.outbox
//                             .write_i32::<BigEndian>(8)
//                             .expect("Vec::write should never fail");
//                         // xid
//                         ap.outbox
//                             .write_i32::<BigEndian>(0)
//                             .expect("Vec::write should never fail");
//                         // opcode
//                         ap.outbox
//                             .write_i32::<BigEndian>(request::OpCode::CloseSession as i32)
//                             .expect("Vec::write should never fail");
//                     } else {
//                         unreachable!("poll_enqueue will never return Err() if not connected");
//                     }
//                 }
//             }
//         }

//         self.state
//             .poll(self.exiting, &mut self.logger, &mut self.default_watcher)
//     }
// }

#[derive(Clone, Debug)]
pub(crate) struct Enqueuer {
    tx: UnboundedSender<(Request, Sender<Result<Response, ZkError>>)>,
    task_tracker: Arc<TaskTracker>,
}

impl Enqueuer {
    pub(crate) async fn enqueue(
        &self,
        request: Request,
    ) -> Result<Result<Response, ZkError>, failure::Error> {
        let (tx, rx) = oneshot::channel();
        match self.tx.unbounded_send((request, tx)) {
            Ok(()) => rx
                .await
                .map_err(|e| format_err!("Error processing request: {:?}", e)),
            Err(e) => Err(format_err!("failed to enqueue new request: {:?}", e)),
        }

        // TODO Map error to something more informative -- i.e. if rx channel got
        // closed, indicate that the zk state is failed/expired as necessary
    }
}
