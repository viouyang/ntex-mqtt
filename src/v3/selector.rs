use std::{fmt, future::Future, marker, pin::Pin, rc::Rc, task::Context, task::Poll, time};

use ntex::codec::{AsyncRead, AsyncWrite};
use ntex::rt::time::{sleep, Sleep};
use ntex::service::{apply_fn_factory, boxed, IntoServiceFactory, Service, ServiceFactory};
use ntex::util::{timeout::Timeout, timeout::TimeoutError, Either, Ready};

use crate::error::{MqttError, ProtocolError};
use crate::io::{DispatchItem, State};

use super::control::{ControlMessage, ControlResult};
use super::default::{DefaultControlService, DefaultPublishService};
use super::handshake::{Handshake, HandshakeAck};
use super::shared::{MqttShared, MqttSinkPool};
use super::{codec as mqtt, dispatcher::factory, MqttServer, MqttSink, Publish, Session};

pub(crate) type SelectItem<Io> =
    (mqtt::Connect, Io, State, Rc<MqttShared>, Option<Pin<Box<Sleep>>>);

type ServerFactory<Io, Err, InitErr> = boxed::BoxServiceFactory<
    (),
    (mqtt::Connect, Io, State, Rc<MqttShared>, Option<Pin<Box<Sleep>>>),
    Either<SelectItem<Io>, ()>,
    MqttError<Err>,
    InitErr,
>;

type Server<Io, Err> = boxed::BoxService<
    (mqtt::Connect, Io, State, Rc<MqttShared>, Option<Pin<Box<Sleep>>>),
    Either<SelectItem<Io>, ()>,
    MqttError<Err>,
>;

/// Mqtt server selector
///
/// Selector allows to choose different mqtt server impls depends on
/// connectt packet.
pub struct Selector<Io, Err, InitErr> {
    servers: Vec<ServerFactory<Io, Err, InitErr>>,
    max_size: u32,
    handshake_timeout: u16,
    pool: Rc<MqttSinkPool>,
    _t: marker::PhantomData<(Io, Err, InitErr)>,
}

impl<Io, Err, InitErr> Selector<Io, Err, InitErr> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Selector {
            servers: Vec::new(),
            max_size: 0,
            handshake_timeout: 0,
            pool: Default::default(),
            _t: marker::PhantomData,
        }
    }
}

impl<Io, Err, InitErr> Selector<Io, Err, InitErr>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    Err: 'static,
    InitErr: 'static,
{
    /// Set handshake timeout in millis.
    ///
    /// Handshake includes `connect` packet and response `connect-ack`.
    /// By default handshake timeuot is disabled.
    pub fn handshake_timeout(mut self, timeout: u16) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Set max inbound frame size.
    ///
    /// If max size is set to `0`, size is unlimited.
    /// By default max size is set to `0`
    pub fn max_size(mut self, size: u32) -> Self {
        self.max_size = size;
        self
    }

    /// Add server variant
    pub fn variant<F, R, St, C, Cn, P>(
        mut self,
        check: F,
        server: MqttServer<Io, St, C, Cn, P>,
    ) -> Self
    where
        F: Fn(&mqtt::Connect) -> R + 'static,
        R: Future<Output = Result<bool, Err>> + 'static,
        St: 'static,
        C: ServiceFactory<
                Config = (),
                Request = Handshake<Io>,
                Response = HandshakeAck<Io, St>,
                Error = Err,
                InitError = InitErr,
            > + 'static,
        Cn: ServiceFactory<
                Config = Session<St>,
                Request = ControlMessage,
                Response = ControlResult,
            > + 'static,
        P: ServiceFactory<Config = Session<St>, Request = Publish, Response = ()> + 'static,
        C::Error: From<Cn::Error>
            + From<Cn::InitError>
            + From<P::Error>
            + From<P::InitError>
            + fmt::Debug,
    {
        self.servers.push(boxed::factory(server.finish_selector(check)));
        self
    }
}

impl<Io, Err, InitErr> ServiceFactory for Selector<Io, Err, InitErr>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    Err: 'static,
    InitErr: 'static,
{
    type Config = ();
    type Request = Io;
    type Response = ();
    type Error = MqttError<Err>;
    type InitError = InitErr;
    type Service = SelectorService<Io, Err>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let futs: Vec<_> = self.servers.iter().map(|srv| srv.new_service(())).collect();
        let max_size = self.max_size;
        let handshake_timeout = self.handshake_timeout;
        let pool = self.pool.clone();

        Box::pin(async move {
            let mut servers = Vec::new();
            for fut in futs {
                servers.push(fut.await?);
            }
            Ok(SelectorService { max_size, handshake_timeout, pool, servers: Rc::new(servers) })
        })
    }
}

pub struct SelectorService<Io, Err> {
    servers: Rc<Vec<Server<Io, Err>>>,
    max_size: u32,
    handshake_timeout: u16,
    pool: Rc<MqttSinkPool>,
}

impl<Io, Err> Service for SelectorService<Io, Err>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    Err: 'static,
{
    type Request = Io;
    type Response = ();
    type Error = MqttError<Err>;
    type Future = Pin<Box<dyn Future<Output = Result<(), MqttError<Err>>>>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut ready = true;
        for srv in self.servers.iter() {
            ready &= srv.poll_ready(cx)?.is_ready();
        }
        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let mut ready = true;
        for srv in self.servers.iter() {
            ready &= srv.poll_shutdown(cx, is_error).is_ready()
        }
        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn call(&self, mut io: Io) -> Self::Future {
        let servers = self.servers.clone();
        let state = State::new();
        let shared = Rc::new(MqttShared::new(
            state.clone(),
            mqtt::Codec::default().max_size(self.max_size),
            16,
            self.pool.clone(),
        ));
        let delay = if self.handshake_timeout > 0 {
            Some(Box::pin(sleep(time::Duration::from_secs(self.handshake_timeout as u64))))
        } else {
            None
        };

        Box::pin(async move {
            // read first packet
            let packet = state
                .next(&mut io, &shared.codec)
                .await
                .map_err(|err| {
                    log::trace!("Error is received during mqtt handshake: {:?}", err);
                    MqttError::from(err)
                })
                .and_then(|res| {
                    res.ok_or_else(|| {
                        log::trace!("Server mqtt is disconnected during handshake");
                        MqttError::Disconnected
                    })
                })?;

            let connect = match packet {
                mqtt::Packet::Connect(connect) => connect,
                packet => {
                    log::info!("MQTT-3.1.0-1: Expected CONNECT packet, received {:?}", packet);
                    return Err(MqttError::Protocol(ProtocolError::Unexpected(
                        packet.packet_type(),
                        "MQTT-3.1.0-1: Expected CONNECT packet",
                    )));
                }
            };

            // call servers
            let mut item = (connect, io, state, shared, delay);
            for srv in servers.iter() {
                match srv.call(item).await? {
                    Either::Left(result) => {
                        item = result;
                    }
                    Either::Right(_) => return Ok(()),
                }
            }
            log::error!("Cannot handle CONNECT packet {:?}", item.0);
            Err(MqttError::ServerError("Cannot handle CONNECT packet"))
        })
    }
}