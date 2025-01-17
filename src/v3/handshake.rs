use std::{fmt, rc::Rc};

use ntex::{io::IoBoxed, time::Seconds};

use super::codec as mqtt;
use super::shared::MqttShared;
use super::sink::MqttSink;

/// Connect message
pub struct Handshake {
    io: IoBoxed,
    pkt: Box<mqtt::Connect>,
    shared: Rc<MqttShared>,
}

impl Handshake {
    pub(crate) fn new(pkt: Box<mqtt::Connect>, io: IoBoxed, shared: Rc<MqttShared>) -> Self {
        Self { io, pkt, shared }
    }

    pub fn packet(&self) -> &mqtt::Connect {
        &self.pkt
    }

    pub fn packet_mut(&mut self) -> &mut mqtt::Connect {
        &mut self.pkt
    }

    #[inline]
    pub fn io(&self) -> &IoBoxed {
        &self.io
    }

    /// Returns mqtt server sink
    pub fn sink(&self) -> MqttSink {
        MqttSink::new(self.shared.clone())
    }

    /// Ack handshake message and set state
    pub fn ack<St>(self, st: St, session_present: bool) -> HandshakeAck<St> {
        let Handshake { io, shared, pkt } = self;
        // [MQTT-3.1.2-24].
        let keepalive = if pkt.keep_alive != 0 {
            (pkt.keep_alive >> 1).checked_add(pkt.keep_alive).unwrap_or(u16::MAX)
        } else {
            30
        };
        HandshakeAck {
            io,
            shared,
            session_present,
            session: Some(st),
            keepalive: Seconds(keepalive),
            return_code: mqtt::ConnectAckReason::ConnectionAccepted,
        }
    }

    /// Create connect ack object with `identifier rejected` return code
    pub fn identifier_rejected<St>(self) -> HandshakeAck<St> {
        HandshakeAck {
            io: self.io,
            shared: self.shared,
            session: None,
            session_present: false,
            keepalive: Seconds(30),
            return_code: mqtt::ConnectAckReason::IdentifierRejected,
        }
    }

    /// Create connect ack object with `bad user name or password` return code
    pub fn bad_username_or_pwd<St>(self) -> HandshakeAck<St> {
        HandshakeAck {
            io: self.io,
            shared: self.shared,
            session: None,
            session_present: false,
            keepalive: Seconds(30),
            return_code: mqtt::ConnectAckReason::BadUserNameOrPassword,
        }
    }

    /// Create connect ack object with `not authorized` return code
    pub fn not_authorized<St>(self) -> HandshakeAck<St> {
        HandshakeAck {
            io: self.io,
            shared: self.shared,
            session: None,
            session_present: false,
            keepalive: Seconds(30),
            return_code: mqtt::ConnectAckReason::NotAuthorized,
        }
    }

    /// Create connect ack object with `service unavailable` return code
    pub fn service_unavailable<St>(self) -> HandshakeAck<St> {
        HandshakeAck {
            io: self.io,
            shared: self.shared,
            session: None,
            session_present: false,
            keepalive: Seconds(30),
            return_code: mqtt::ConnectAckReason::ServiceUnavailable,
        }
    }
}

impl fmt::Debug for Handshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.pkt.fmt(f)
    }
}

/// Ack connect message
pub struct HandshakeAck<St> {
    pub(crate) io: IoBoxed,
    pub(crate) session: Option<St>,
    pub(crate) session_present: bool,
    pub(crate) return_code: mqtt::ConnectAckReason,
    pub(crate) shared: Rc<MqttShared>,
    pub(crate) keepalive: Seconds,
}

impl<St> HandshakeAck<St> {
    /// Set idle time-out for the connection in seconds
    ///
    /// By default idle time-out is set to 30 seconds.
    pub fn idle_timeout(mut self, timeout: Seconds) -> Self {
        self.keepalive = timeout;
        self
    }
}
