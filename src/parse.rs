use super::protocol::*;
use std::{
    io::{BufRead, BufReader, Read},
    str::FromStr,
};

use quick_xml::{
    events::{attributes::Attributes, Event},
    Reader,
};

use bumpalo::Bump;

pub fn parse<'a, S: Read>(stream: S, bump: &'a Bump) -> &'a Protocol<'a> {
    let mut reader = Reader::from_reader(BufReader::new(stream));
    reader.trim_text(true).expand_empty_elements(true);
    // Skip first <?xml ... ?> event
    let _ = reader.read_event_into(&mut Vec::new());
    parse_protocol(reader, bump)
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

fn parse_protocol<'a, R: BufRead>(mut reader: Reader<R>, bump: &'a Bump) -> &'a Protocol<'a> {
    let protocol = match reader.read_event_into(&mut Vec::new()) {
        Ok(Event::Start(bytes)) => {
            assert!(bytes.name().into_inner() == b"protocol", "Missing protocol toplevel tag");
            if let Some(attr) = bytes
                .attributes()
                .filter_map(|res| res.ok())
                .find(|attr| attr.key.into_inner() == b"name")
            {
                bump.alloc(Protocol::new(decode_utf8_or_panic(attr.value.into_owned())))
            } else {
                panic!("Protocol must have a name");
            }
        }
        e => panic!("Ill-formed protocol file: {:?}", e),
    };

    loop {
        match reader.read_event_into(&mut Vec::new()) {
            Ok(Event::Start(bytes)) => {
                if let b"interface" = bytes.name().into_inner() {
                    let interface = parse_interface(&mut reader, bytes.attributes(), bump);
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
    reader: &mut Reader<R>, attrs: Attributes, bump: &'a Bump,
) -> &'a Interface<'a> {
    let interface = bump.alloc(Interface::new());
    for attr in attrs.filter_map(|res| res.ok()) {
        match attr.key.into_inner() {
            b"name" => interface.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"version" => interface.parsed_version = parse_or_panic(&attr.value),
            _ => {}
        }
    }

    loop {
        match reader.read_event_into(&mut Vec::new()) {
            Ok(Event::Start(bytes)) => interface.requests.push(parse_message(
                reader,
                interface.requests.len() as u32,
                bytes.attributes(),
                bytes.name().into_inner(), // event or request
                bump,
            )),
            Ok(Event::End(bytes)) if bytes.name().into_inner() == b"interface" => break,
            _ => {}
        }
    }

    interface
}

fn parse_message<'a, R: BufRead>(
    reader: &mut Reader<R>, opcode: u32, attrs: Attributes, event_or_request: &[u8], bump: &'a Bump,
) -> &'a Message<'a> {
    let message = bump.alloc(Message::new(opcode));
    for attr in attrs.filter_map(|res| res.ok()) {
        match attr.key.into_inner() {
            b"name" => message.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"since" => message.since = parse_or_panic(&attr.value),
            _ => {}
        }
    }

    loop {
        match reader.read_event_into(&mut Vec::new()) {
            Ok(Event::Start(bytes)) if bytes.name().into_inner() == b"arg" => {
                message.args.push(parse_arg(reader, bytes.attributes()))
            }
            Ok(Event::End(bytes)) if bytes.name().into_inner() == event_or_request => break,
            _ => {}
        }
    }

    message
}

fn parse_arg<R: BufRead>(_reader: &mut Reader<R>, attrs: Attributes) -> Arg {
    let mut arg = Arg::new();
    for attr in attrs.filter_map(|res| res.ok()) {
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
