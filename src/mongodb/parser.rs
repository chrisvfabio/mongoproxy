use super::messages::{self,MsgHeader,MsgOpMsg,MsgOpQuery,MsgOpReply,MongoMessage,OpCode};
use std::io::Read;
use std::time::{Instant};
use log::{debug,info,warn};
use prometheus::{CounterVec, HistogramVec};


lazy_static! {
    static ref OPCODE_COUNTER: CounterVec =
        register_counter_vec!(
            "opcode_count_total",
            "Number of different opcodes encountered",
            &["opcode"]).unwrap();

    static ref UNSUPPORTED_OPCODE_COUNTER: CounterVec =
        register_counter_vec!(
            "unsupported_opcode_count_total",
            "Number of unrecognized opcodes in MongoDb header",
            &["opcode"]).unwrap();

    static ref HEADER_PARSE_ERRORS_COUNTER: CounterVec =
        register_counter_vec!(
            "header_parse_error_count_total",
            "Header parse errors",
            &["error"]).unwrap();

    static ref SERVER_RESPONSE_TIME_SECONDS: HistogramVec =
        register_histogram_vec!(
            "server_response_time_seconds",
            "Backend response latency",
            &["op", "collection"]).unwrap();

    static ref CLIENT_BYTES_SENT_TOTAL: CounterVec =
        register_counter_vec!(
            "client_bytes_sent_total",
            "Total number of bytes sent by the client",
            &["client"]).unwrap();

    static ref SERVER_BYTES_SENT_TOTAL: CounterVec =
        register_counter_vec!(
            "sever_bytes_sent_total",
            "Total number of bytes sent by the server",
            &["client"]).unwrap();
}

pub struct MongoStatsTracker {
    client:                 MongoProtocolParser,
    server:                 MongoProtocolParser,
    client_addr:            String,
    client_request_time:    Instant,
}

impl MongoStatsTracker {
    pub fn new(client_addr: &String) -> Self {
        MongoStatsTracker {
            client: MongoProtocolParser::new(),
            server: MongoProtocolParser::new(),
            client_addr: client_addr.clone(),
            client_request_time: Instant::now(),
        }
    }

    pub fn track_client_request(&mut self, buf: &Vec<u8>) {
        CLIENT_BYTES_SENT_TOTAL.with_label_values(&[&self.client_addr])
            .inc_by(buf.len() as f64);

        for msg in self.client.parse_buffer(buf) {
            info!("client: hdr: {}", self.client.header);
            info!("client: msg: {}", msg);
            self.client_request_time = Instant::now();

            match msg {
                MongoMessage::Msg(m) => {
                    for s in m.sections {
                        for elem in s.iter() {
                            println!("element: {:?}", elem);
                        }
                    }
                },
                _ => {},
            }
        }
    }

    pub fn track_server_response(&mut self, buf: &Vec<u8>) {
        SERVER_BYTES_SENT_TOTAL.with_label_values(&[&self.client_addr])
            .inc_by(buf.len() as f64);

        for msg in self.server.parse_buffer(buf) {
            info!("server: hdr: {}", self.server.header);
            info!("server: msg: {}", msg);

            let time_to_response = self.client_request_time.elapsed().as_millis();
            SERVER_RESPONSE_TIME_SECONDS
                .with_label_values(&[&"foo", &"bar"])
                .observe(time_to_response as f64 / 1000.0);
        }
    }
}

pub struct MongoProtocolParser {
    header:         messages::MsgHeader,
    have_header:    bool,
    want_bytes:     usize,
    message_buf:    Vec<u8>,
    have_message:   bool,
    parser_active:  bool,
}

impl MongoProtocolParser {

    pub fn new() -> MongoProtocolParser {
        MongoProtocolParser{
            header: MsgHeader::new(),
            have_header: false,
            want_bytes: messages::HEADER_LENGTH,
            message_buf: Vec::new(),
            parser_active: true,
            have_message: false,
        }
    }

    // Parse the buffer and return the parsed object when ready.
    //
    // parse_buffer expects that it is being fed chunks of the incoming
    // stream. It tries to assemble MongoDb messages and returns the parsed
    // message.
    //
    // The first message we always want to see is the MongoDb message header.
    // This header in turn contains the length of the message that follows. So
    // we try to read message length worth of bytes and parse the message. Once
    // the message is parsed we expect a header again and the process repeats.
    //
    // TODO: Stop the parser if any of the internal routines returns an error
    // TODO: We probably don't need to return a Vec here, and do just 2 passes
    // over the buffer to collect both the header *and* the message.
    //
    pub fn parse_buffer(&mut self, buf: &Vec<u8>) -> Vec<MongoMessage> {
        let mut result = Vec::new();

        if !self.parser_active {
            return result;
        }

        self.message_buf.extend(buf);

        let mut loop_counter = 0;
        while self.message_buf.len() >= self.want_bytes {
            // Make a note of how many bytes we got as we're going to
            // overwrite it later.
            let new_buffer_start = self.want_bytes;

            if !self.have_header {
                match MsgHeader::from_reader(&self.message_buf[..self.want_bytes]) {
                    Ok(header) => {
                        assert!(header.message_length >= messages::HEADER_LENGTH);
                        self.header = header;
                        self.have_header = true;
                        self.have_message = false;
                        self.want_bytes = self.header.message_length - messages::HEADER_LENGTH;
                        debug!("parser: have header {:?}, want {} more bytes", self.header, self.want_bytes);
                    },
                    Err(e) => {
                        warn!("parser: failed to read a header, stopping: {}", e);
                        self.parser_active = false;
                        HEADER_PARSE_ERRORS_COUNTER.with_label_values(&[&e.to_string()]).inc();
                    },
                }
            } else {
                result.push(extract_message(self.header.op_code, &self.message_buf[..self.want_bytes]));

                // We got the payload, time to ask for a header again
                self.have_header = false;
                self.have_message = true;
                self.want_bytes = messages::HEADER_LENGTH;
            }

            // Point the message_buf to the bytes that we haven't yet processed
            // And don't worry about performance, yet
            self.message_buf = self.message_buf[new_buffer_start..].to_vec();
            debug!("message_buf capacity={}", self.message_buf.capacity());
            loop_counter += 1;
            debug!("loop {}: {} bytes in buffer, want {}", loop_counter, self.message_buf.len(), self.want_bytes);
        }

        if self.message_buf.len() > 0 {
            warn!("parser: {} surplus bytes in buf and I want {}.",
                self.message_buf.len(), self.want_bytes);
        }

        result
    }
}

fn extract_message(op_code: u32, mut rdr: impl Read) -> MongoMessage {
    OPCODE_COUNTER.with_label_values(&[&op_code.to_string()]).inc();

    match num_traits::FromPrimitive::from_u32(op_code) {
        Some(OpCode::OpReply) => {
            let msg = MsgOpReply::from_reader(&mut rdr).unwrap();
            return MongoMessage::Reply(msg);
        }
        Some(OpCode::OpQuery) => {
            let msg = MsgOpQuery::from_reader(&mut rdr).unwrap();
            return MongoMessage::Query(msg);
        },
        Some(OpCode::OpMsg) => {
            let msg = MsgOpMsg::from_reader(&mut rdr).unwrap();
            return MongoMessage::Msg(msg);
        },
        Some(OpCode::OpPing) => {},
        Some(OpCode::OpPong) => {},
        None => {
            UNSUPPORTED_OPCODE_COUNTER.with_label_values(&[&op_code.to_string()]).inc();
            warn!("Unhandled OP: {}", op_code);
        },
    }
    return MongoMessage::None;
}

#[test]
fn test_parse_buffer_header() {
    let hdr = MsgHeader {
        message_length: messages::HEADER_LENGTH,
        request_id: 1234,
        response_to: 5678,
        op_code: 1,
    };

    let mut buf = [0 as u8; messages::HEADER_LENGTH];
    hdr.write(&mut buf[..]).unwrap();

    let mut parser = MongoProtocolParser::new();
    let result = parser.parse_buffer(&buf.to_vec());

    assert_eq!(result.len(), 0);                // just the header
    assert_eq!(parser.have_header, true);
    assert_eq!(parser.have_message, false);
    assert_eq!(parser.want_bytes, 0);
}