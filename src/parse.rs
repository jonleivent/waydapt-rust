#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]

#[allow(clippy::wildcard_imports)]
use super::protocol::*;
use crate::crate_traits::Alloc;
use std::{
    io::{BufRead, BufReader, Read},
    str::FromStr,
};

use quick_xml::{
    events::{attributes::Attributes, Event},
    Reader,
};

pub(crate) fn parse<S: Read>(stream: S, alloc: &impl Alloc) -> &Protocol {
    let mut reader = Reader::from_reader(BufReader::new(stream));
    reader.trim_text(true).expand_empty_elements(true);
    // Skip first <?xml ... ?> event
    let _ = reader.read_event_into(&mut Vec::new());
    parse_protocol(reader, alloc)
}

fn decode_utf8_or_panic(txt: Vec<u8>) -> String {
    String::from_utf8(txt)
        .unwrap_or_else(|e| panic!("Invalid UTF8: '{}'", String::from_utf8_lossy(&e.into_bytes())))
}

fn parse_or_panic<T: FromStr>(txt: &[u8]) -> T {
    if let Ok(s) = std::str::from_utf8(txt) {
        if let Ok(result) = s.parse() {
            return result;
        }
    }
    panic!(
        "Invalid value '{}' for parsing type '{}'",
        String::from_utf8_lossy(txt),
        std::any::type_name::<T>()
    )
}

fn parse_protocol<R: BufRead>(mut reader: Reader<R>, alloc: &impl Alloc) -> &Protocol {
    let mut buf = Vec::new();
    let protocol = match reader.read_event_into(&mut buf) {
        Ok(Event::Start(bytes)) => {
            assert!(bytes.name().into_inner() == b"protocol", "Missing protocol toplevel tag");
            if let Some(attr) = bytes
                .attributes()
                .filter_map(Result::ok)
                .find(|attr| attr.key.into_inner() == b"name")
            {
                alloc.alloc(Protocol::new(decode_utf8_or_panic(attr.value.into_owned())))
            } else {
                panic!("Protocol must have a name");
            }
        }
        e => panic!("Ill-formed protocol file: {e:?}"),
    };

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(bytes)) => {
                if let b"interface" = bytes.name().into_inner() {
                    let interface = parse_interface(&mut reader, bytes.attributes(), alloc);
                    protocol.interfaces.push(interface);
                }
            }
            Ok(Event::End(bytes)) => {
                let name = bytes.name().into_inner();
                assert!(
                    name == b"protocol",
                    "Unexpected closing token `{}`",
                    String::from_utf8_lossy(name)
                );
                break;
            }
            _ => {}
        }
    }

    protocol
}

fn parse_interface<'a, R: BufRead>(
    reader: &mut Reader<R>, attrs: Attributes, alloc: &'a impl Alloc,
) -> &'a Interface<'a> {
    let interface = alloc.alloc(Interface::new());
    for attr in attrs.filter_map(Result::ok) {
        match attr.key.into_inner() {
            b"name" => interface.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"version" => interface.parsed_version = parse_or_panic(&attr.value),
            _ => {}
        }
    }

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            #[allow(clippy::cast_possible_truncation)]
            Ok(Event::Start(bytes)) => {
                let event_or_request = bytes.name().into_inner();
                let is_request = event_or_request == b"request";
                let msg = parse_message(
                    reader,
                    interface.requests.len() as u32,
                    bytes.attributes(),
                    event_or_request, // event or request
                    is_request,
                    alloc,
                );
                if is_request {
                    interface.requests.push(msg);
                } else {
                    interface.events.push(msg);
                }
            }
            Ok(Event::End(bytes)) if bytes.name().into_inner() == b"interface" => break,
            _ => {}
        }
    }

    interface
}

fn parse_message<'a, R: BufRead>(
    reader: &mut Reader<R>, opcode: u32, attrs: Attributes, event_or_request: &[u8],
    is_request: bool, alloc: &'a impl Alloc,
) -> &'a Message<'a> {
    let message = alloc.alloc(Message::new(opcode));
    message.is_request = is_request;
    for attr in attrs.filter_map(Result::ok) {
        match attr.key.into_inner() {
            b"name" => message.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"since" => message.since = parse_or_panic(&attr.value),
            _ => {}
        }
    }

    let mut num_fds = 0;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(bytes)) if bytes.name().into_inner() == b"arg" => {
                let arg = parse_arg(reader, bytes.attributes());
                if arg.typ == Type::Fd {
                    num_fds += 1;
                }
                message.args.push(arg);
            }
            Ok(Event::End(bytes)) if bytes.name().into_inner() == event_or_request => break,
            _ => {}
        }
    }
    message.num_fds = num_fds;
    message
}

fn parse_arg<R: BufRead>(_reader: &mut Reader<R>, attrs: Attributes) -> Arg {
    let mut arg = Arg::new();
    for attr in attrs.filter_map(Result::ok) {
        match attr.key.into_inner() {
            b"name" => arg.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"type" => arg.typ = parse_type(&attr.value),
            b"interface" => arg.interface_name = Some(parse_or_panic(&attr.value)),
            _ => {}
        }
    }

    arg
}

fn parse_type(txt: &[u8]) -> Type {
    match txt {
        b"int" => Type::Int,
        b"uint" => Type::Uint,
        b"fixed" => Type::Fixed,
        b"string" => Type::String,
        b"object" => Type::Object,
        b"new_id" => Type::NewId,
        b"array" => Type::Array,
        b"fd" => Type::Fd,
        e => panic!("Unexpected type: {}", String::from_utf8_lossy(e)),
    }
}
