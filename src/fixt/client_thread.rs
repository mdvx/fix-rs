// Copyright 2016 James Bendig. See the COPYRIGHT file at the top-level
// directory of this distribution.
//
// Licensed under:
//   the MIT license
//     <LICENSE-MIT or https://opensource.org/licenses/MIT>
//   or the Apache License, Version 2.0
//     <LICENSE-APACHE or https://www.apache.org/licenses/LICENSE-2.0>,
// at your option. This file may not be copied, modified, or distributed
// except according to those terms.

use mio::{Event,Events,Poll,PollOpt,Ready,Token};
use mio::channel::{Receiver,Sender};
use mio::tcp::{Shutdown,TcpStream};
use mio::timer::{Timeout,Timer};
use mio::timer::Builder as TimerBuilder;
use std::cmp;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{ErrorKind,Read,Write};
use std::mem;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use fixt::client::{ClientEvent,ConnectionTerminatedReason};
use fixt::message::FIXTMessage;
use dictionary::{CloneDictionary,standard_msg_types};
use dictionary::field_types::generic::UTCTimestampFieldType;
use dictionary::field_types::other::{BusinessRejectReason,SessionRejectReason};
use dictionary::fields::{MsgSeqNum,SenderCompID,TargetCompID,OrigSendingTime};
use dictionary::messages::{Logon,Logout,ResendRequest,TestRequest,Heartbeat,SequenceReset,Reject,BusinessMessageReject};
use field::Field;
use field_type::FieldType;
use fix::{Parser,ParseError};

//TODO: Support Application Version. FIXT 1.1, page 8.
//TODO: Make sure Logon message is sent automatically instead of waiting on caller. Althought, we
//might have to support this for testing purposes.
//TODO: Check for infinite resend loop when other side sends garbled messages, we later send
//ResendRequest, and the other side continues to send garbled messages.
//TODO: Implement ConnectionStatus handling using a state machine pattern to reduce chance of
//mistake.
//TODO: Need to make inbound and outbound MsgSeqNums adjustable at connection setup and available
//on connection termination to support persistent sessions.

const NO_INBOUND_TIMEOUT_PADDING_MS: u64 = 250;
const AUTO_DISCONNECT_AFTER_LOGOUT_RESPONSE_SECS: u64 = 10;
const AUTO_DISCONNECT_AFTER_INITIATING_LOGOUT_SECS: u64 = 10;
const AUTO_CONTINUE_AFTER_LOGOUT_RESEND_REQUEST_SECS: u64 = 10;
const EVENT_POLL_CAPACITY: usize = 1024;
const TIMER_TICK_MS: u64 = 100;
const TIMER_TIMEOUTS_PER_TICK_MAX: usize = 256;
pub const CONNECTION_COUNT_MAX: usize = 65536;
const TIMEOUTS_PER_CONNECTION_MAX: usize = 3;

pub const INTERNAL_CLIENT_EVENT_TOKEN: Token = Token(0);
const TIMEOUT_TOKEN: Token = Token(1);
pub const BASE_CONNECTION_TOKEN: Token = Token(2);

#[derive(Clone,Copy,PartialEq)]
enum LoggingOutInitiator {
    Client,
    Server
}

enum LoggingOutType {
    Ok, //Client requested logout.
    Error(ConnectionTerminatedReason), //An unrecoverable error occurred.
    ResendRequesting(LoggingOutInitiator), //LoggingOutInitiator requested logout but MsgSeqNum was higher than expected so we're trying to collect the missing messages before continuing.
    Responding, //Server requested logout and we are about to send a response.
    Responded, //Server requested logout and we sent a response.
}

enum ConnectionStatus {
    LoggingOn,
    Established,
    LoggingOut(LoggingOutType),
}

impl ConnectionStatus {
    fn is_logging_on(&self) -> bool {
        if let ConnectionStatus::LoggingOn = *self {
            true
        }
        else {
            false
        }
    }

    fn is_established(&self) -> bool {
        if let ConnectionStatus::Established = *self {
            true
        }
        else {
            false
        }
    }

    fn is_logging_out(&self) -> bool {
        if let ConnectionStatus::LoggingOut(_) = *self {
            true
        }
        else {
            false
        }
    }

    fn is_logging_out_with_error(&self) -> bool {
        if let ConnectionStatus::LoggingOut(ref logging_out_type) = *self {
            if let LoggingOutType::Error(_) = *logging_out_type {
                true
            }
            else {
                false
            }
        }
        else {
            false
        }
    }

    fn is_logging_out_with_resending_request_initiated_by_client(&self) -> bool {
        if let ConnectionStatus::LoggingOut(ref logging_out_type) = *self {
            if let LoggingOutType::ResendRequesting(ref logging_out_initiator) = *logging_out_type {
                if let LoggingOutInitiator::Client = *logging_out_initiator {
                    return true;
                }
            }
        }

        false
    }

    fn is_logging_out_with_resending_request_initiated_by_server(&self) -> bool {
        if let ConnectionStatus::LoggingOut(ref logging_out_type) = *self {
            if let LoggingOutType::ResendRequesting(ref logging_out_initiator) = *logging_out_type {
                if let LoggingOutInitiator::Server = *logging_out_initiator {
                    return true;
                }
            }
        }

        false
    }

    fn is_logging_out_with_responding(&self) -> bool {
        if let ConnectionStatus::LoggingOut(ref logging_out_type) = *self {
            if let LoggingOutType::Responding = *logging_out_type {
                true
            }
            else {
                false
            }
        }
        else {
            false
        }
    }

    fn is_logging_out_with_responded(&self) -> bool {
        if let ConnectionStatus::LoggingOut(ref logging_out_type) = *self {
            if let LoggingOutType::Responded = *logging_out_type {
                true
            }
            else {
                false
            }
        }
        else {
            false
        }
    }
}

enum TimeoutType {
    Outbound,
    Inbound,
    InboundTestRequest,
    ContinueLogout,
    Logout,
    HangUp,
}

type MsgSeqNumType = <<MsgSeqNum as Field>::Type as FieldType>::Type;

struct OutboundMessage {
    message: Box<FIXTMessage + Send>,
    auto_msg_seq_num: bool,
}

impl OutboundMessage {
    fn new<T: FIXTMessage + Send + Sized + 'static>(message: T,auto_msg_seq_num: bool) -> Self {
        OutboundMessage {
            message: Box::new(message),
            auto_msg_seq_num: auto_msg_seq_num,
        }
    }

    fn from<T: FIXTMessage + Send + Sized + 'static>(message: T) -> Self {
        OutboundMessage {
            message: Box::new(message),
            auto_msg_seq_num: true,
        }
    }

    fn from_box(message: Box<FIXTMessage + Send>) -> Self {
        OutboundMessage {
            message: message,
            auto_msg_seq_num: true,
        }
    }
}

fn reset_timeout(timer: &mut Timer<(TimeoutType,Token)>,timeout: &mut Option<Timeout>,timeout_duration: &Option<Duration>,timeout_type: TimeoutType,token: &Token) {
    if let Some(ref timeout) = *timeout {
        timer.cancel_timeout(timeout);
    }

    *timeout = if let Some(duration) = *timeout_duration {
        Some(
            timer.set_timeout(
                duration,
                (timeout_type,*token)
            ).unwrap()
        )
    }
    else {
        None
    };
}

fn reset_outbound_timeout(timer: &mut Timer<(TimeoutType,Token)>,outbound_timeout: &mut Option<Timeout>,outbound_timeout_duration: &Option<Duration>,token: &Token) {
    reset_timeout(
        timer,
        outbound_timeout,
        outbound_timeout_duration,
        TimeoutType::Outbound,
        token
    );
}

fn reset_inbound_timeout(timer: &mut Timer<(TimeoutType,Token)>,inbound_timeout: &mut Option<Timeout>,inbound_timeout_duration: &Option<Duration>,token: &Token) {
    reset_timeout(
        timer,
        inbound_timeout,
        inbound_timeout_duration,
        TimeoutType::Inbound,
        token
    );
}

#[derive(Debug)]
pub enum InternalClientToThreadEvent {
    NewConnection(Token,SocketAddr),
    SendMessage(Token,Box<FIXTMessage + Send>),
    Logout(Token),
    Shutdown,
}

enum ConnectionEventError {
    TerminateConnection(Connection,ConnectionTerminatedReason),
    Shutdown,
}

enum ConnectionReadMessage {
    Message(Box<FIXTMessage + Send>),
    Error(ParseError),
}

struct Connection {
    socket: TcpStream,
    token: Token,
    outbound_messages: Vec<OutboundMessage>,
    outbound_buffer: Vec<u8>,
    outbound_msg_seq_num: MsgSeqNumType,
    outbound_heartbeat_timeout: Option<Timeout>,
    outbound_heartbeat_timeout_duration: Option<Duration>,
    inbound_buffer: Vec<u8>,
    inbound_msg_seq_num: MsgSeqNumType,
    inbound_testrequest_timeout: Option<Timeout>,
    inbound_testrequest_timeout_duration: Option<Duration>,
    inbound_resend_request_msg_seq_num: Option<MsgSeqNumType>,
    logout_timeout: Option<Timeout>,
    parser: Parser,
    status: ConnectionStatus,
    sender_comp_id: Rc<<<SenderCompID as Field>::Type as FieldType>::Type>,
    target_comp_id: Rc<<<TargetCompID as Field>::Type as FieldType>::Type>,
}

impl Connection {
    fn write(&mut self,timer: &mut Timer<(TimeoutType,Token)>) -> Result<(),ConnectionTerminatedReason> {
        //Send data until no more messages are available or until the socket returns WouldBlock.
        let mut sent_data = false;
        loop { //TODO: This loop might make this function too greedy. Maybe not?
            //Fill an outbound buffer by serializing each message in a FIFO order. Once this buffer
            //is drained, the process repeats itself.
            if self.outbound_buffer.is_empty() {
                if self.outbound_messages.is_empty() {
                    //Nothing left to write.

                    //If a Logout message was sent after an unrecoverable error, close the socket
                    //immediately.
                    if self.status.is_logging_out_with_error() {
                        let status = mem::replace(&mut self.status,ConnectionStatus::LoggingOut(LoggingOutType::Ok)); //Need to get at the error. Status should not be used again...
                        if let ConnectionStatus::LoggingOut(logging_out_type) = status {
                            if let LoggingOutType::Error(reason) = logging_out_type {
                                let _ = self.socket.shutdown(Shutdown::Both);
                                return Err(reason);
                            }
                        }
                    }
                    //Similarly, if a Logout message was sent as a response to to the server
                    //issuing a Logout, start a timer and wait so many seconds before closing the
                    //socket. This is the recommended way to respond to a Logout instead of
                    //disconnecting immediately.
                    else if self.status.is_logging_out_with_responding() {
                        self.status = ConnectionStatus::LoggingOut(LoggingOutType::Responded);

                        self.logout_timeout = Some(
                            timer.set_timeout(
                                Duration::from_secs(AUTO_DISCONNECT_AFTER_LOGOUT_RESPONSE_SECS),
                                (TimeoutType::HangUp,self.token)
                            ).unwrap()
                        );
                    }
                    break;
                }

                //Setup message to go out and serialize it.
                let mut message = self.outbound_messages.remove(0);
                message.message.setup_fixt_session_header(
                    if message.auto_msg_seq_num {
                        let result = Some(self.outbound_msg_seq_num);
                        try!(self.increment_outbound_msg_seq_num());
                        result
                    } else { None },
                    (*self.sender_comp_id).clone(),
                    (*self.target_comp_id).clone()
                );
                message.message.read(&mut self.outbound_buffer);

                //TODO: Hold onto message and pass it off to the client or some callback so the
                //library user knows exactly which messages have been sent -- although not
                //necessarily acknowledged.
            }

            //Send data. Simple.
            match self.socket.write(&self.outbound_buffer) {
                Ok(bytes_written) => {
                    //TODO: This shifting mechanism is not very efficient...
                    self.outbound_buffer.drain(0..bytes_written);
                    sent_data = true;

                },
                Err(e) => {
                    match e.kind() {
                        ErrorKind::WouldBlock => break,
                        ErrorKind::BrokenPipe => {
                            //TODO: This might not be an actual error if all logging out has been
                            //performed. Could be a Hup.
                            return Err(ConnectionTerminatedReason::SocketWriteError(e));
                        },
                        _ => return Err(ConnectionTerminatedReason::SocketWriteError(e)),
                    };
                }
            }
        }

        //If any data was sent, need to update timeout so we don't send an unnecessary Heartbeat
        //message.
        if sent_data {
            reset_outbound_timeout(timer,&mut self.outbound_heartbeat_timeout,&self.outbound_heartbeat_timeout_duration,&self.token);
        }

        Ok(())
    }

    fn read(&mut self,timer: &mut Timer<(TimeoutType,Token)>) -> Result<(Vec<ConnectionReadMessage>),::std::io::Error> {
        let mut messages = Vec::new();

        //Keep reading all available bytes on the socket until it's exhausted. The bytes are parsed
        //immediately into messages. Parse errors are stored in order of encounter relative to
        //messages because they often indicate an increase in expected inbound MsgSeqNum.
        loop {
            match self.socket.read(&mut self.inbound_buffer) {
                Ok(bytes_read) => {
                    if bytes_read == 0 {
                        //Socket exhausted.
                        break;
                    }

                    //Parse all of the read bytes.
                    let mut bytes_to_parse = bytes_read;
                    while bytes_to_parse > 0 {
                        let (bytes_parsed,result) = self.parser.parse(&self.inbound_buffer[bytes_read - bytes_to_parse..bytes_read]);

                        assert!(bytes_parsed > 0);
                        assert!(bytes_to_parse >= bytes_parsed);
                        bytes_to_parse -= bytes_parsed;

                        //Retain order by extracting messages and then the error from parser.
                        for message in self.parser.messages.drain(..) {
                            messages.push(ConnectionReadMessage::Message(message));
                        }
                        if let Err(e) = result {
                            messages.push(ConnectionReadMessage::Error(e));
                        }
                    }
                },
                Err(e) => {
                    use std::io::ErrorKind::WouldBlock;
                    if let WouldBlock = e.kind() {
                        //Socket exhausted.
                        break;
                    }

                    return Err(e);
                },
            };
        }

        //Update timeout so we don't send an unnecessary TestRequest message. read() should never
        //be called unless data is available (due to poll()) so we don't have to check if any data
        //bytes were actually read.
        reset_inbound_timeout(timer,&mut self.inbound_testrequest_timeout,&self.inbound_testrequest_timeout_duration,&self.token);

        Ok(messages)
    }

    fn shutdown(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        self.outbound_messages.clear();
        self.outbound_buffer.clear();
    }

    fn initiate_logout(&mut self,timer: &mut Timer<(TimeoutType,Token)>,logging_out_type: LoggingOutType,text: &str) {
        //Begin the logout process. Use respond_to_logout() to respond to a logout message.

        assert!(match logging_out_type {
            LoggingOutType::Ok => !self.status.is_logging_out() || self.status.is_logging_out_with_resending_request_initiated_by_client(),
            LoggingOutType::Error(_) => !self.status.is_logging_out_with_error(),
            _ => false,
        });

        let mut logout = Logout::new();
        logout.text = String::from(text);

        //TODO: The clearing of outbound messages might be optional. Probably need a receipt or
        //something for those that are left unprocessed.
        self.outbound_messages.clear(); //TODO: May want to store unprocessed messages so client knows what didn't go out.
        self.outbound_messages.push(OutboundMessage::from(logout));

        //If attempting to logout cleanly, setup timer to auto-logout if we don't get a Logout
        //response. LoggingOutType::Error just disconnects immediately.
        if let LoggingOutType::Ok = logging_out_type {
            self.logout_timeout = Some(
                timer.set_timeout(
                    Duration::from_secs(AUTO_DISCONNECT_AFTER_INITIATING_LOGOUT_SECS),
                    (TimeoutType::Logout,self.token)
                ).unwrap()
            );
        }

        self.status = ConnectionStatus::LoggingOut(logging_out_type);
    }

    fn respond_to_logout(&mut self) {
        assert!(self.status.is_established() || self.status.is_logging_out_with_resending_request_initiated_by_server());

        let logout = Logout::new();
        self.outbound_messages.push(OutboundMessage::from(logout));

        self.status = ConnectionStatus::LoggingOut(LoggingOutType::Responding);
    }

    fn increment_outbound_msg_seq_num(&mut self) -> Result<(),ConnectionTerminatedReason> {
        //Check for overflow before incrementing. Just force the connection to terminate if this
        //occurs. This number is so large that the only way it can be reached is if the other party
        //issues SequenceReset-Reset with a crazy high NewSeqNo. NewSeqNo values higher than
        //u64::max_value() are outright rejected as parsing errors.
        if self.outbound_msg_seq_num == u64::max_value() {
            return Err(ConnectionTerminatedReason::OutboundMsgSeqNumMaxExceededError);
        }

        self.outbound_msg_seq_num += 1;
        Ok(())
    }

    fn increment_inbound_msg_seq_num(&mut self) -> Result<(),ConnectionTerminatedReason> {
        //See increment_outbound_msg_seq_num() for an explanation of this check.
        if self.inbound_msg_seq_num == u64::max_value() {
            return Err(ConnectionTerminatedReason::InboundMsgSeqNumMaxExceededError);
        }

        self.inbound_msg_seq_num += 1;
        Ok(())
    }

    fn clear_inbound_resend_request_msg_seq_num(&mut self,timer: &mut Timer<(TimeoutType,Token)>) {
        self.inbound_resend_request_msg_seq_num = None;

        //If server started a logout, we noticed missing messaged, and have now
        //received all of those messages, finally respond to logout.
        if self.status.is_logging_out_with_resending_request_initiated_by_server() {
            self.respond_to_logout();
        }
        //Same as above except client initiated logout and suspended it long enough to
        //retrieve messages.
        else if self.status.is_logging_out_with_resending_request_initiated_by_client() {
            self.initiate_logout(timer,LoggingOutType::Ok,"");
        }
    }
}

macro_rules! try_write_connection_or_terminate {
    ( $connection_entry:ident, $internal_thread:ident ) => {
        if let Err(e) = $connection_entry.get_mut().write(&mut $internal_thread.timer) {
            return Err(ConnectionEventError::TerminateConnection($connection_entry.remove(),e));
        }
    }
}

struct InternalThread {
    poll: Poll,
    tx: Sender<ClientEvent>,
    rx: Receiver<InternalClientToThreadEvent>,
    message_dictionary: HashMap<&'static [u8],Box<FIXTMessage + Send>>,
    sender_comp_id: Rc<<<SenderCompID as Field>::Type as FieldType>::Type>,
    target_comp_id: Rc<<<TargetCompID as Field>::Type as FieldType>::Type>,
    connections: HashMap<Token,Connection>,
    timer: Timer<(TimeoutType,Token)>,
}

impl InternalThread {
    fn on_internal_client_event(&mut self) -> Result<(),ConnectionEventError> {
        let client_event = match self.rx.try_recv() {
            Ok(e) => e,
            Err(_) => return Ok(()), //Shouldn't be possible but PROBABLY just means no client events are available.
        };

        match client_event {
            //Client wants to setup a new connection.
            InternalClientToThreadEvent::NewConnection(token,address) => {
                let socket = match TcpStream::connect(&address) {
                    Ok(socket) => socket,
                    Err(e) => {
                        self.tx.send(ClientEvent::ConnectionFailed(token.0,e)).unwrap();
                        return Ok(())
                    },
                };

                let connection = Connection {
                    socket: socket,
                    token: token,
                    outbound_messages: Vec::new(),
                    outbound_buffer: Vec::new(),
                    outbound_msg_seq_num: 1, //Starts at 1. FIXT v1.1, page 5.
                    outbound_heartbeat_timeout: None,
                    outbound_heartbeat_timeout_duration: None,
                    inbound_buffer: vec![0;1024],
                    inbound_msg_seq_num: 1, //Starts at 1 as well.
                    inbound_testrequest_timeout: None,
                    inbound_testrequest_timeout_duration: None,
                    inbound_resend_request_msg_seq_num: None,
                    logout_timeout: None,
                    parser: Parser::new(self.message_dictionary.clone()),
                    status: ConnectionStatus::LoggingOn,
                    sender_comp_id: self.sender_comp_id.clone(),
                    target_comp_id: self.target_comp_id.clone(),
                };

                //Have poll let us know when we can can read or write.
                if let Err(e) = self.poll.register(&connection.socket,connection.token,Ready::all(),PollOpt::edge()) {
                    self.tx.send(ClientEvent::ConnectionFailed(connection.token.0,e)).unwrap();
                    return Ok(())
                }

                self.connections.insert(token,connection);
            },
            //Client wants to send a message over a connection.
            InternalClientToThreadEvent::SendMessage(token,message) => {
                if let Entry::Occupied(mut connection_entry) = self.connections.entry(token) {
                    connection_entry.get_mut().outbound_messages.push(OutboundMessage::from_box(message));
                    try_write_connection_or_terminate!(connection_entry,self);
                }
                else {
                    //Silently ignore message for invalid connection.
                    //TODO: Maybe submit this to a logging system or something?
                }
            },
            //Client wants to begin the clean logout process on a connection.
            InternalClientToThreadEvent::Logout(token) => {
                if let Entry::Occupied(mut connection_entry) = self.connections.entry(token) {
                    match connection_entry.get_mut().status {
                        ConnectionStatus::LoggingOn => {
                            //Just disconnect since connection hasn't had a chance to logon.
                            return Err(ConnectionEventError::TerminateConnection(connection_entry.remove(),ConnectionTerminatedReason::ClientRequested));
                        },
                        ConnectionStatus::LoggingOut(_) => {}, //Already logging out.
                        ConnectionStatus::Established => {
                            //Begin logout.
                            connection_entry.get_mut().initiate_logout(&mut self.timer,LoggingOutType::Ok,"");
                            try_write_connection_or_terminate!(connection_entry,self);
                        },
                    };
                }
                else {
                    //Silently ignore logout for invalid connection.
                    //TODO: Maybe submit this to a logging system or something?
                }
            },
            //Client wants to shutdown all connections immediately. Incoming or outgoing messages
            //might be lost!
            InternalClientToThreadEvent::Shutdown => return Err(ConnectionEventError::Shutdown),
        };

        Ok(())
    }

    fn on_timeout(&mut self) -> Result<(),ConnectionEventError> {
        if let Some((timeout_type,token)) = self.timer.poll() {
            if let Entry::Occupied(mut connection_entry) = self.connections.entry(token) {
                match timeout_type {
                    TimeoutType::Outbound if connection_entry.get().status.is_established() => {
                        //We haven't sent any data in a while. Send a Heartbeat to let other side
                        //know we're still around.
                        let mut heartbeat = Heartbeat::new();
                        heartbeat.test_req_id = String::from(""); //Left blank when not responding to TestRequest.
                        connection_entry.get_mut().outbound_messages.push(OutboundMessage::from(heartbeat));
                    },
                    TimeoutType::Inbound if connection_entry.get().status.is_established() => {
                        //Other side hasn't sent any data in a while. Send a TestRequest to see if
                        //it's still around.
                        let mut test_request = TestRequest::new();

                        //Use current time as TestReqID as recommended. This might not exactly
                        //match the SendingTime field depending on when it gets sent though.
                        let now_time = UTCTimestampFieldType::new_now();
                        let mut now_time_buffer = Vec::new();
                        UTCTimestampFieldType::read(&now_time,&mut now_time_buffer);
                        test_request.test_req_id = String::from_utf8_lossy(&now_time_buffer[..]).into_owned();

                        connection_entry.get_mut().outbound_messages.push(OutboundMessage::from(test_request));

                        //Start a TimeoutType::InboundTestRequest timer to auto-disconnect if we
                        //don't get a response in time. Note that any reploy what-so-ever will stop
                        //the auto-disconnect -- even if this TestRequest is ignored and later gap
                        //filled. The overhead in maintaining a list of sent TestReqIds does not
                        //seem worth the effort. It would only be useful for debugging reasons,
                        //right?
                        //TODO: This might belong in the Connection::write() function so we don't
                        //disconnect before the TestRequest is actually sent. On the other hand, if
                        //this doesn't go out in a reasonable amount of time, we're backlogged and
                        //might be having negative consequences on the network.
                        connection_entry.get_mut().inbound_testrequest_timeout = Some(
                            self.timer.set_timeout(
                                connection_entry.get_mut().inbound_testrequest_timeout_duration.unwrap(),
                                (TimeoutType::InboundTestRequest,token),
                            ).unwrap()
                        );
                    },
                    TimeoutType::InboundTestRequest if connection_entry.get().status.is_established() => {
                        connection_entry.get_mut().shutdown();
                        println!("Shutting down connection after other side failed to respond to TestRequest before timeout");
                        return Err(ConnectionEventError::TerminateConnection(connection_entry.remove(),ConnectionTerminatedReason::TestRequestNotRespondedError));
                    },
                    TimeoutType::ContinueLogout if connection_entry.get().status.is_logging_out_with_resending_request_initiated_by_server() => {
                        connection_entry.get_mut().respond_to_logout();
                    },
                    TimeoutType::Logout => {
                        connection_entry.get_mut().shutdown();
                        println!("Shutting down connection after no Logout response before timeout");
                        return Err(ConnectionEventError::TerminateConnection(connection_entry.remove(),ConnectionTerminatedReason::LogoutNoResponseError));
                    },
                    TimeoutType::HangUp => {
                        connection_entry.get_mut().shutdown();
                        println!("Shutting down connection after other side failed to disconnect before timeout");
                        return Err(ConnectionEventError::TerminateConnection(connection_entry.remove(),ConnectionTerminatedReason::LogoutNoHangUpError));
                    },
                    TimeoutType::Outbound |
                    TimeoutType::Inbound |
                    TimeoutType::InboundTestRequest |
                    TimeoutType::ContinueLogout => {}, //Special conditions only. Handled above.
                }

                //Write any new Heartbeat or TestRequest messages.
                try_write_connection_or_terminate!(connection_entry,self);
            }
        }

        Ok(())
    }

    fn on_network(&mut self,event: &Event) -> Result<(),ConnectionEventError> {
        //Note: Each event.kind() can indicate more than one state. For example: is_readable() and
        //is_hup() can both return true.

        if let Entry::Occupied(mut connection_entry) = self.connections.entry(event.token()) {
            //Read all of the bytes available on the socket, parse into messages, perform internal
            //book keeping on the messages, and then pass them off to the application.
            if event.kind().is_readable() {
                let result = connection_entry.get_mut().read(&mut self.timer);
                if let Err(e) = result {
                    return Err(ConnectionEventError::TerminateConnection(connection_entry.remove(),ConnectionTerminatedReason::SocketReadError(e)));
                }

                if let Ok(messages) = result {
                    for message in messages {
                        let result = match message {
                            ConnectionReadMessage::Message(message) =>
                                InternalThread::on_network_message(connection_entry.get_mut(),message,&self.tx,&mut self.timer),
                            ConnectionReadMessage::Error(parse_error) =>
                                InternalThread::on_network_parse_error(connection_entry.get_mut(),parse_error,&self.tx),
                        };

                        if let Err(e) = result {
                            return Err(ConnectionEventError::TerminateConnection(connection_entry.remove(),e));
                        }
                    }
                }

                //Send any new messages that were generated automatically as a response.
                //Determining if a new message is available to go out can be kind of
                //complicated so just blindly try for now. We can optimize this if it's a
                //performance concern later.
                try_write_connection_or_terminate!(connection_entry,self);
            }

            //Write all pending messages out to the socket until they are exhausted or the socket
            //fills up and would block. Whichever happens first.
            if event.kind().is_writable() {
                try_write_connection_or_terminate!(connection_entry,self);
            }

            //Socket was closed on the other side. If already responded to a Logout initiated by
            //the other side, then this is expected and the logout operation was performed cleanly.
            //Otherwise, the connection dropped for some unknown reason.
            if event.kind().is_hup() {
                if connection_entry.get_mut().status.is_logging_out_with_responded() {
                    println!("Shutting down connection after server logged out cleanly.");
                    return Err(ConnectionEventError::TerminateConnection(connection_entry.remove(),ConnectionTerminatedReason::ServerRequested));
                }
                else {
                    //Coax a ConnectionTerminatedReason::SocketWriteError to simplify error
                    //handling.
                    try_write_connection_or_terminate!(connection_entry,self);
                }
            }
        }

        Ok(())
    }

    fn on_network_message(connection: &mut Connection,mut message: Box<FIXTMessage + Send>,tx: &Sender<ClientEvent>,timer: &mut Timer<(TimeoutType,Token)>) -> Result<(),ConnectionTerminatedReason>  {
        //Perform book keeping needed to maintain the FIX connection and then pass off the message
        //to the client.

        fn if_on_resend_request(connection: &mut Connection,message: Box<FIXTMessage + Send>,msg_seq_num: MsgSeqNumType,tx: &Sender<ClientEvent>,timer: &mut Timer<(TimeoutType,Token)>) -> Option<Box<FIXTMessage + Send>> {
            let mut rejected = false;

            if let Some(resend_request) = message.as_any().downcast_ref::<ResendRequest>() {
                //Outright reject the message when BeginSeqNo > EndSeqNo because it doesn't make
                //sense. The exact response to this scenario does not appear to be described in the
                //spec.
                if resend_request.begin_seq_no > resend_request.end_seq_no && resend_request.end_seq_no != 0 {
                    let mut reject = Reject::new();
                    reject.ref_seq_num = msg_seq_num;
                    reject.session_reject_reason = Some(SessionRejectReason::ValueIsIncorrectForThisTag);
                    reject.text = String::from("EndSeqNo must be greater than BeginSeqNo or set to 0");
                    connection.outbound_messages.push(OutboundMessage::from(reject));

                    rejected = true;
                }
                else {
                    //Cap the end range of the resend request to the highest sent MsgSeqNum. The spec
                    //doesn't describe what to do when EndSeqNo is greater than the highest sent
                    //MsgSeqNum. BUT, it apparently was a common pattern in older versions of the
                    //protocol to set EndSeqNo to a really high number (ie. 999999) to mean the same
                    //thing as setting it to 0 now.
                    let end_seq_no = if resend_request.end_seq_no > connection.outbound_msg_seq_num || resend_request.end_seq_no == 0 {
                        connection.outbound_msg_seq_num - 1
                    }
                    else {
                        resend_request.end_seq_no
                    };

                    //Fill message gap by resending messages.
                    //TODO: This shouldn't always be a gap fill. Only for
                    //administrative messages. Need to handle business messages
                    //appropriately.
                    let mut sequence_reset = SequenceReset::new();
                    sequence_reset.gap_fill_flag = true;
                    sequence_reset.msg_seq_num = resend_request.begin_seq_no;
                    sequence_reset.new_seq_no = if resend_request.end_seq_no == 0 { connection.outbound_msg_seq_num } else { resend_request.end_seq_no + 1 }; //TODO: Handle potential overflow.
                    connection.outbound_messages.push(OutboundMessage::new(sequence_reset,false));
                }

                //If:
                // 1. The server initiates a logout
                // 2. We acknowledge the logout
                // 3. The server sends a ResendRequest (instead of disconnecting AND before our
                //    disconnect timeout)
                //Then we need to assume the logout was cancelled.
                //See FIXT v1.1, page 42.
                if connection.status.is_logging_out_with_responded() {
                    connection.status = ConnectionStatus::Established;

                    //Stop timeout so we don't auto-disconnect.
                    if let Some(ref timeout) = connection.logout_timeout {
                        timer.cancel_timeout(timeout);
                    }
                }
            }

            //Appease the borrow checker by fully handling reject much later than where occurred.
            if rejected {
                tx.send(ClientEvent::MessageRejected(connection.token.0,message)).unwrap();
                None
            }
            else {
                Some(message)
            }
        }

        fn reject_for_sending_time_accuracy(connection: &mut Connection,message: Box<FIXTMessage + Send>,msg_seq_num: MsgSeqNumType,tx: &Sender<ClientEvent>) {
            let mut reject = Reject::new();
            reject.ref_seq_num = msg_seq_num;
            reject.session_reject_reason = Some(SessionRejectReason::SendingTimeAccuracyProblem);
            reject.text = String::from("SendingTime accuracy problem");
            connection.outbound_messages.push(OutboundMessage::from(reject));

            tx.send(ClientEvent::MessageRejected(connection.token.0,message)).unwrap();
        }

        fn on_greater_than_expected_msg_seq_num(connection: &mut Connection,mut message: Box<FIXTMessage + Send>,msg_seq_num: MsgSeqNumType,tx: &Sender<ClientEvent>,timer: &mut Timer<(TimeoutType,Token)>) -> Option<Box<FIXTMessage + Send>> {
            //FIXT v1.1, page 13: We should reply to ResendRequest first when MsgSeqNum is higher
            //than expected. Afterwards, we should send our own ResendRequest.
            message = match if_on_resend_request(connection,message,msg_seq_num,tx,timer) {
                Some(message) => message,
                None => return None,
            };

            //Fetch the messages the server says were sent but we never
            //received using a ResendRequest.
            let mut resend_request = ResendRequest::new();
            resend_request.begin_seq_no = connection.inbound_msg_seq_num;
            resend_request.end_seq_no = 0;
            connection.outbound_messages.push(OutboundMessage::from(resend_request));

            //Keep track of the newest msg_seq_num that's been seen so we know when the message gap has
            //been filled.
            connection.inbound_resend_request_msg_seq_num = Some(
                cmp::max(connection.inbound_resend_request_msg_seq_num.unwrap_or(msg_seq_num),msg_seq_num)
            );

            //Handle Logout messages as a special case where we need to delicately retrieve the
            //missing messages while still going through with the logout process. See FIXT v1.1,
            //page 42 for details.
            //Start by figuring out if Logout message is a response to our Logout or the other side
            //is initiating a logout.
            if let Some(logout) = message.as_any().downcast_ref::<Logout>() {
                let logging_out_initiator = if let ConnectionStatus::LoggingOut(ref logging_out_type) = connection.status {
                    match logging_out_type {
                        &LoggingOutType::Ok => { //Server acknowledged our logout but we're missing some messages.
                            Some(LoggingOutInitiator::Client)
                        },
                        &LoggingOutType::Responding | //Server sent two diffrent Logouts in a row with messages inbetween missing.
                        &LoggingOutType::Responded => { //Server cancelled original logout and we're some how missing some messages.
                            Some(LoggingOutInitiator::Server)
                        },
                        &LoggingOutType::Error(_) => { None } //Does not matter. We are closing the connection immediately.
                        &LoggingOutType::ResendRequesting(logging_out_initiator) => { //Server resent Logout before fully responding to our ResendRequest.
                            None //No change so timeout timer can't be kept alive perpetually.
                        },
                    }
                }
                else {
                    //Server is initiating logout.
                    Some(LoggingOutInitiator::Server)
                };

                //Begin watching for missing messages so we can finish logging out.
                if let Some(logging_out_initiator) = logging_out_initiator {
                    connection.status = ConnectionStatus::LoggingOut(LoggingOutType::ResendRequesting(logging_out_initiator));

                    //Start a timer to acknowledge Logout if messages are not fulfilled in a reasonable
                    //amount of time. If they are fulfilled sooner, we'll just acknowledge sooner.
                    match logging_out_initiator {
                        LoggingOutInitiator::Server => {
                            let timeout_duration = Some(Duration::from_secs(AUTO_CONTINUE_AFTER_LOGOUT_RESEND_REQUEST_SECS));
                            reset_timeout(
                                timer,
                                &mut connection.logout_timeout,
                                &timeout_duration,
                                TimeoutType::ContinueLogout,
                                &connection.token
                            );
                        }
                        LoggingOutInitiator::Client => {
                            //Let the auto-disconnect timer continue even though some messages
                            //might be lost. This is because if the server ignores our
                            //ResendRequest but responds to a new Logout attempt, we'll have three
                            //possibly catastrophic outcomes.
                            //1. Logout response has MsgSeqNum < expected: Critical error.
                            //2. Logout response has MsgSeqNum > expected: That's the current
                            //   situation so well be looping.
                            //3. Logout response has MsgSeqNum == expected: But last time the
                            //   MsgSeqNum was higher so there is a serious numbering issue.
                            //   Critical error.
                            //If the other side does respond to our ResendRequest appropriately,
                            //we'll restart a clean Logout process.
                        },
                    }
                }
            }

            Some(message)
        }

        fn on_less_than_expected_msg_seq_num(connection: &mut Connection,message: Box<FIXTMessage + Send>,msg_seq_num: MsgSeqNumType,tx: &Sender<ClientEvent>,timer: &mut Timer<(TimeoutType,Token)>) {
            //Messages with MsgSeqNum lower than expected are never processed as normal. They are
            //either duplicates (as indicated) or an unrecoverable error where one side fell
            //out of sync.
            if message.is_poss_dup() {
                if message.orig_sending_time() <= message.sending_time() {
                    //Duplicate message that otherwise seems correct.
                    tx.send(ClientEvent::MessageReceivedDuplicate(connection.token.0,message)).unwrap();
                }
                else {
                    //Reject message even though it's a duplicate. Currently, we probably don't
                    //care about the OrigSendingTime vs SendingTime but this is correct processing
                    //according to the spec.
                    reject_for_sending_time_accuracy(connection,message,msg_seq_num,tx);
                }
            }
            else {
                use std::fmt::Write;

                let mut text = String::new();
                let _ = write!(text,"MsgSeqNum too low, expecting {} but received {}",connection.inbound_msg_seq_num,msg_seq_num);
                connection.initiate_logout(timer,LoggingOutType::Error(ConnectionTerminatedReason::InboundMsgSeqNumLowerThanExpectedError),&text);
            }
        }

        fn on_expected_msg_seq_num(connection: &mut Connection,mut message: Box<FIXTMessage + Send>,msg_seq_num: MsgSeqNumType,tx: &Sender<ClientEvent>,timer: &mut Timer<(TimeoutType,Token)>) -> Result<Option<Box<FIXTMessage + Send>>,ConnectionTerminatedReason> {
            //Start by incrementing expected inbound MsgSeqNum since the message is at least
            //formatted correctly and matches the expected MsgSeqNum.
            try!(connection.increment_inbound_msg_seq_num());

            //Handle general FIXT message validation.
            if message.is_poss_dup() && message.orig_sending_time() > message.sending_time() {
                reject_for_sending_time_accuracy(connection,message,msg_seq_num,tx);
                return Ok(None);
            }

            //Handle SequenceReset-GapFill messages.
            if let Some(sequence_reset) = message.as_any_mut().downcast_mut::<SequenceReset>() {
                if sequence_reset.gap_fill_flag {
                    if sequence_reset.new_seq_no > connection.inbound_msg_seq_num {
                        //Fast forward to the new expected inbound MsgSeqNum.
                        connection.inbound_msg_seq_num = sequence_reset.new_seq_no;
                    }
                    else {
                        //Attempting to rewind MsgSeqNum is not allowed according to FIXT v1.1,
                        //page 29.
                        use std::fmt::Write;

                        let mut reject = Reject::new();
                        reject.ref_seq_num = msg_seq_num;
                        reject.session_reject_reason = Some(SessionRejectReason::ValueIsIncorrectForThisTag);
                        let _ = write!(&mut reject.text,"Attempt to lower sequence number, invalid value NewSeqNo={}",sequence_reset.new_seq_no);
                        connection.outbound_messages.push(OutboundMessage::from(reject));

                        tx.send(ClientEvent::MessageRejected(connection.token.0,Box::new(mem::replace(sequence_reset,SequenceReset::new())))).unwrap();
                    }
                }
                else {
                    //This should have been handled earlier as a special case that ignores
                    //MsgSeqNum.
                    unreachable!();
                }
            }

            //Handle ResendRequest messages.
            message = match if_on_resend_request(connection,message,msg_seq_num,tx,timer) {
                Some(message) => message,
                None => return Ok(None),
            };

            //Handle Logout messages.
            if let Some(logout) = message.as_any().downcast_ref::<Logout>() {
                //Server responded to our Logout.
                if let ConnectionStatus::LoggingOut(_) = connection.status {
                    connection.shutdown();
                    return Err(ConnectionTerminatedReason::ClientRequested);
                }
                //Server started logout process.
                else {
                    connection.respond_to_logout();
                }
            }

            Ok(Some(message))
        }

        //Every message must have SenderCompID and TargetCompID set to the expected values or else
        //the message must be rejected and we should logout. See FIXT 1.1, page 52.
        if *message.sender_comp_id() != *connection.target_comp_id {
            connection.initiate_logout(timer,LoggingOutType::Error(ConnectionTerminatedReason::SenderCompIDWrongError),"SenderCompID is wrong");

            let mut reject = Reject::new();
            reject.ref_seq_num = connection.inbound_msg_seq_num;
            reject.session_reject_reason = Some(SessionRejectReason::CompIDProblem);
            reject.text = String::from("CompID problem");
            connection.outbound_messages.insert(0,OutboundMessage::from(reject));

            tx.send(ClientEvent::MessageRejected(connection.token.0,message)).unwrap();

            return Ok(());
        }
        else if *message.target_comp_id() != *connection.sender_comp_id {
            connection.initiate_logout(timer,LoggingOutType::Error(ConnectionTerminatedReason::TargetCompIDWrongError),"TargetCompID is wrong");

            let mut reject = Reject::new();
            reject.ref_seq_num = connection.inbound_msg_seq_num;
            reject.session_reject_reason = Some(SessionRejectReason::CompIDProblem);
            reject.text = String::from("CompID problem");
            connection.outbound_messages.insert(0,OutboundMessage::from(reject));

            tx.send(ClientEvent::MessageRejected(connection.token.0,message)).unwrap();

            return Ok(());
        }

        //When the connection first starts, it sends a Logon message to the server. The server then
        //must respond with a Logon acknowleding the Logon, a Logout rejecting the Logout, or just
        //disconnecting. In this case, if a  Logon is received, we setup timers to send periodic
        //messages in case there is no activity. We then notify the client that the session is
        //established and other messages can now be sent or received.
        let just_logged_on = if connection.status.is_logging_on() {
            if let Some(message) = message.as_any().downcast_ref::<Logon>() {
                connection.status = ConnectionStatus::Established;

                if message.heart_bt_int > 0 {
                    connection.outbound_heartbeat_timeout_duration = Some(
                        Duration::from_secs(message.heart_bt_int as u64)
                    );
                    reset_outbound_timeout(timer,&mut connection.outbound_heartbeat_timeout,&connection.outbound_heartbeat_timeout_duration,&connection.token);
                    connection.inbound_testrequest_timeout_duration = Some(
                        Duration::from_millis(message.heart_bt_int as u64 * 1000 + NO_INBOUND_TIMEOUT_PADDING_MS),
                    );
                    reset_inbound_timeout(timer,&mut connection.inbound_testrequest_timeout,&connection.inbound_testrequest_timeout_duration,&connection.token);
                }
                else if message.heart_bt_int < 0 {
                    connection.initiate_logout(timer,LoggingOutType::Error(ConnectionTerminatedReason::LogonHeartBtIntNegativeError),"HeartBtInt cannot be negative");
                    return Ok(());
                }

                //TODO: Need to take MaxMessageSize into account.
                //TODO: Optionally support filtering message types (NoMsgTypes).
                tx.send(ClientEvent::SessionEstablished(connection.token.0)).unwrap();
            }
            else {
                connection.initiate_logout(timer,LoggingOutType::Error(ConnectionTerminatedReason::LogonNotFirstMessageError),"First message not a logon");
                return Ok(());
            }

            true
        }
        else {
            false
        };

        //Perform MsgSeqNum error handling if MsgSeqNum > or < expected. Otherwise, perform
        //administrative message handling and related book keeping.
        let msg_seq_num = message.msg_seq_num();
        if message.as_any_mut().downcast_mut::<SequenceReset>().map_or(false,|sequence_reset| {
            if !sequence_reset.gap_fill_flag {
                if sequence_reset.new_seq_no > connection.inbound_msg_seq_num {
                    connection.inbound_msg_seq_num = sequence_reset.new_seq_no;
                    connection.clear_inbound_resend_request_msg_seq_num(timer);
                }
                else if sequence_reset.new_seq_no == connection.inbound_msg_seq_num {
                    tx.send(ClientEvent::SequenceResetResetHasNoEffect(connection.token.0)).unwrap();
                }
                else {//if sequence_reset.new_seq_no < connection.inbound_msg_seq_num
                    use std::fmt::Write;

                    let mut reject = Reject::new();
                    reject.ref_seq_num = msg_seq_num;
                    reject.session_reject_reason = Some(SessionRejectReason::ValueIsIncorrectForThisTag); //TODO: Is there a better reason or maybe leave this blank?
                    let _ = write!(&mut reject.text,"Attempt to lower sequence number, invalid value NewSeqNo={}",sequence_reset.new_seq_no);
                    connection.outbound_messages.push(OutboundMessage::from(reject));

                    tx.send(ClientEvent::SequenceResetResetInThePast(connection.token.0)).unwrap();
                }

                true
            }
            else {
                false
            }
        }) {
            //Special case where MsgSeqNum does not matter. Handled above.
        }
        else if msg_seq_num > connection.inbound_msg_seq_num {
            message = match on_greater_than_expected_msg_seq_num(connection,message,msg_seq_num,tx,timer) {
                Some(message) => message,
                None => return Ok(()),
            };

            //The only message that can be processed out of order is the Logon message. Every other
            //one will be discarded and we'll wait for the in-order resend.
            if !just_logged_on {
                //Message is discarded.
                return Ok(());
            }
        }
        else if msg_seq_num < connection.inbound_msg_seq_num {
            on_less_than_expected_msg_seq_num(connection,message,msg_seq_num,tx,timer);
            return Ok(());
        }
        else {
            message = match try!(on_expected_msg_seq_num(connection,message,msg_seq_num,tx,timer)) {
                Some(message) => message,
                None => return Ok(()),
            };

            //If the current message has caught up with our outstanding ResendRequest, mark it as
            //such so we don't send another.
            if let Some(resend_request_msg_seq_num) = connection.inbound_resend_request_msg_seq_num {
                if resend_request_msg_seq_num <= connection.inbound_msg_seq_num {
                    connection.clear_inbound_resend_request_msg_seq_num(timer);
                }
            }
        }

        //Reply to TestRequest automatically with a Heartbeat. Typical keep alive stuff.
        if let Some(test_request) = message.as_any().downcast_ref::<TestRequest>() {
            let mut heartbeat = Heartbeat::new();
            heartbeat.test_req_id = test_request.test_req_id.clone();
            connection.outbound_messages.push(OutboundMessage::from(heartbeat));
        }

        tx.send(ClientEvent::MessageReceived(connection.token.0,message)).unwrap();

        Ok(())
    }

    fn on_network_parse_error(connection: &mut Connection,parse_error: ParseError,tx: &Sender<ClientEvent>)-> Result<(),ConnectionTerminatedReason> {
        fn push_reject(connection: &mut Connection,ref_msg_type: &[u8],ref_tag_id: &Vec<u8>,session_reject_reason: SessionRejectReason,text: &str) -> Result<(),ConnectionTerminatedReason> {
            let mut reject = Reject::new();
            reject.ref_msg_type = String::from_utf8_lossy(ref_msg_type).into_owned();
            reject.ref_tag_id = String::from_utf8_lossy(ref_tag_id).into_owned();
            reject.ref_seq_num = connection.inbound_msg_seq_num;
            reject.session_reject_reason = Some(session_reject_reason);
            reject.text = String::from(text);
            connection.outbound_messages.push(OutboundMessage::from(reject));

            try!(connection.increment_inbound_msg_seq_num());

            Ok(())
        }

        match connection.status {
            //There's no room for errors when attempting to logon. If the network data cannot be
            //parsed, just disconnect immediately.
            ConnectionStatus::LoggingOn => {
                connection.shutdown();
                return Err(ConnectionTerminatedReason::LogonParseError(parse_error));
            },
            //Handle parse error as normal. Usually just respond with a Reject and increment the
            //expected inbound MsgSeqNum.
            _ => {
                match parse_error {
                    ParseError::MissingRequiredTag(ref tag,_) => {
                        try!(push_reject(connection,b"",tag,SessionRejectReason::RequiredTagMissing,"Required tag missing"));
                    },
                    ParseError::UnexpectedTag(ref tag) => {
                        try!(push_reject(connection,b"",tag,SessionRejectReason::TagNotDefinedForThisMessageType,"Tag not defined for this message type"));
                    },
                    ParseError::UnknownTag(ref tag) => {
                        try!(push_reject(connection,b"",tag,SessionRejectReason::InvalidTagNumber,"Invalid tag number"));
                    },
                    ParseError::NoValueAfterTag(ref tag) => {
                        try!(push_reject(connection,b"",tag,SessionRejectReason::TagSpecifiedWithoutAValue,"Tag specified without a value"));
                    },
                    ParseError::OutOfRangeTag(ref tag) => {
                        try!(push_reject(connection,b"",tag,SessionRejectReason::ValueIsIncorrectForThisTag,"Value is incorrect (out of range) for this tag"));
                    },
                    ParseError::WrongFormatTag(ref tag) => {
                        try!(push_reject(connection,b"",tag,SessionRejectReason::IncorrectDataFormatForValue,"Incorrect data format for value"));
                    },
                    /* TODO: These should probably be considered garbled instead of
                     * responding to.
                     ParseError::BeginStrNotFirstTag |
                     ParseError::BodyLengthNotSecondTag |
                     ParseError::MsgTypeNotThirdTag |
                     ParseError::ChecksumNotLastTag |
                     ParseError::MissingPrecedingLengthTag(_) |
                     ParseError::MissingFollowingLengthTag(_) => {
                         try!(push_reject(connection,None,SessionRejectReason::TagSpecifiedOutOfRequiredOrder,"Tag specified out of required order"));
                     },
                     */
                    ParseError::DuplicateTag(ref tag) => {
                        try!(push_reject(connection,b"",tag,SessionRejectReason::TagAppearsMoreThanOnce,"Tag appears more than once"));
                    },
                    ParseError::MissingConditionallyRequiredTag(ref tag,ref message) => {
                        if *tag == OrigSendingTime::tag() { //Session level conditionally required tag.
                            try!(push_reject(connection,message.msg_type(),tag,SessionRejectReason::RequiredTagMissing,"Conditionally required tag missing"));
                        }
                        else {
                            let mut business_message_reject = BusinessMessageReject::new();
                            business_message_reject.ref_seq_num = connection.inbound_msg_seq_num;
                            business_message_reject.ref_msg_type = String::from_utf8_lossy(message.msg_type()).into_owned();
                            business_message_reject.business_reject_reason = BusinessRejectReason::ConditionallyRequiredFieldMissing;
                            business_message_reject.business_reject_ref_id = String::from_utf8_lossy(tag).into_owned();
                            business_message_reject.text = String::from("Conditionally required field missing");
                            connection.outbound_messages.push(OutboundMessage::from(business_message_reject));

                            try!(connection.increment_inbound_msg_seq_num());
                        }
                    },
                    ParseError::MissingFirstRepeatingGroupTagAfterNumberOfRepeatingGroupTag(ref tag) |
                    ParseError::NonRepeatingGroupTagInRepeatingGroup(ref tag) |
                    ParseError::RepeatingGroupTagWithNoRepeatingGroup(ref tag) => {
                        try!(push_reject(connection,b"",tag,SessionRejectReason::IncorrectNumInGroupCountForRepeatingGroup,"Incorrect NumInGroup count for repeating group"));
                    },
                    ParseError::MsgTypeUnknown(ref msg_type) => {
                        //If we're here, we know the MsgType is not user defined. So we just need
                        //to know if it's defined in the spec (Unsupported MsgType) or completely
                        //unknown (Invalid MsgType).
                        if standard_msg_types().contains(&msg_type[..]) {
                            //MsgType is unsupported.
                            let mut business_message_reject = BusinessMessageReject::new();
                            business_message_reject.ref_seq_num = connection.inbound_msg_seq_num;
                            business_message_reject.ref_msg_type = String::from_utf8_lossy(msg_type).into_owned();
                            business_message_reject.business_reject_reason = BusinessRejectReason::UnsupportedMessageType;
                            business_message_reject.business_reject_ref_id = business_message_reject.ref_msg_type.clone();
                            business_message_reject.text = String::from("Unsupported Message Type");
                            connection.outbound_messages.push(OutboundMessage::from(business_message_reject));

                            try!(connection.increment_inbound_msg_seq_num());
                        }
                        else {
                            //MsgType is invalid.
                            try!(push_reject(connection,&msg_type[..],msg_type,SessionRejectReason::InvalidMsgType,"Invalid MsgType"));
                        }
                    },
                    _ => {}, //TODO: Support other errors as appropriate.
                };

                //Tell user about the garbled message just in case they care.
                tx.send(ClientEvent::MessageReceivedGarbled(connection.token.0,parse_error)).unwrap();
            },
        };

        Ok(())
    }
}

pub fn internal_client_thread(poll: Poll,
                              tx: Sender<ClientEvent>,
                              rx: Receiver<InternalClientToThreadEvent>,
                              message_dictionary: HashMap<&'static [u8],Box<FIXTMessage + Send>>,
                              sender_comp_id: <<SenderCompID as Field>::Type as FieldType>::Type,
                              target_comp_id: <<TargetCompID as Field>::Type as FieldType>::Type) {
    //TODO: There should probably be a mechanism to log every possible message, even those we
    //handle automatically. One method might be to have a layer above this that handles the
    //automatic stuff and allows for logging...this is probably just too low level.

    let mut internal_thread = InternalThread {
        poll: poll,
        tx: tx,
        rx: rx,
        message_dictionary: message_dictionary,
        sender_comp_id: Rc::new(sender_comp_id),
        target_comp_id: Rc::new(target_comp_id),
        connections: HashMap::new(),
        timer: TimerBuilder::default()
            .tick_duration(Duration::from_millis(TIMER_TICK_MS))
            .num_slots(TIMER_TIMEOUTS_PER_TICK_MAX)
            .capacity(CONNECTION_COUNT_MAX * TIMEOUTS_PER_CONNECTION_MAX)
            .build(),
    };
    let mut terminated_connections: Vec<(Connection,ConnectionTerminatedReason)> = Vec::new();

    //Have poll let us know when we need to send a heartbeat, testrequest, or respond to some other
    //timeout.
    if let Err(e) = internal_thread.poll.register(&internal_thread.timer,TIMEOUT_TOKEN,Ready::readable(),PollOpt::level()) {
        internal_thread.tx.send(ClientEvent::FatalError("Cannot register timer for polling",e)).unwrap();
        return;
    }

    //Poll events sent by Client, triggered by timer timeout, or network activity and act upon them
    //on a per-connection basis.
    let mut events = Events::with_capacity(EVENT_POLL_CAPACITY);
    loop {
        if let Err(e) = internal_thread.poll.poll(&mut events,None) {
            internal_thread.tx.send(ClientEvent::FatalError("Cannot poll events",e)).unwrap();
            return;
        }

        for event in events.iter() {
            let result = match event.token() {
                INTERNAL_CLIENT_EVENT_TOKEN => internal_thread.on_internal_client_event(),
                TIMEOUT_TOKEN => internal_thread.on_timeout(),
                _ => internal_thread.on_network(&event),
            };

            //Handle errors from event. Terminated connections are stored until all events have
            //been processed so we don't accidentally re-use an event token for a new connection
            //with one that was terminated and still has events pending.
            if let Err(e) = result {
                match e {
                    ConnectionEventError::TerminateConnection(connection,e) => {
                        terminated_connections.push((connection,e));
                    },
                    ConnectionEventError::Shutdown => return,
                };
            }
        }

        //Clean-up connections that have been shutdown (cleanly or on error).
        for (connection,e) in terminated_connections.drain(..) {
            let _ = internal_thread.poll.deregister(&connection.socket);
            if let Some(ref timeout) = connection.outbound_heartbeat_timeout {
                internal_thread.timer.cancel_timeout(timeout);
            }
            if let Some(ref timeout) = connection.inbound_testrequest_timeout {
                internal_thread.timer.cancel_timeout(timeout);
            }
            if let Some(ref timeout) = connection.logout_timeout {
                internal_thread.timer.cancel_timeout(timeout);
            }

            internal_thread.tx.send(ClientEvent::ConnectionTerminated(connection.token.0,e)).unwrap();
        }
    }
}
