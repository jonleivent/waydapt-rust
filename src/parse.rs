use super::protocol::*;
use std::{
    io::{BufRead, BufReader, Read},
    str::FromStr,
};

use quick_xml::{
    events::{attributes::Attributes, Event},
    Reader,
};

use std::collections::HashMap;

use bumpalo::Bump;

pub fn parse<'g, 'a, S: Read>(
    stream: S,
    interfaces: &'a mut HashMap<&'a str, Vec<&'a Interface<'g, 'a>>>,
    bump: &'a Bump,
) -> &'a mut Protocol<'g, 'a> {
    let mut reader = Reader::from_reader(BufReader::new(stream));
    reader.trim_text(true).expand_empty_elements(true);
    // Skip first <?xml ... ?> event
    let _ = reader.read_event_into(&mut Vec::new());
    parse_protocol(reader, interfaces, bump)
}

fn decode_utf8_or_panic(txt: Vec<u8>) -> String {
    match String::from_utf8(txt) {
        Ok(txt) => txt,
        Err(e) => panic!(
            "Invalid UTF8: '{}'",
            String::from_utf8_lossy(&e.into_bytes())
        ),
    }
}

fn parse_or_panic<T: FromStr>(txt: &[u8]) -> T {
    match std::str::from_utf8(txt)
        .ok()
        .and_then(|val| val.parse().ok())
    {
        Some(version) => version,
        None => panic!(
            "Invalid value '{}' for parsing type '{}'",
            String::from_utf8_lossy(txt),
            std::any::type_name::<T>()
        ),
    }
}

fn parse_protocol<'g, 'a, R: BufRead>(
    mut reader: Reader<R>,
    interfaces: &'a mut HashMap<&'a str, Vec<&'a Interface<'g, 'a>>>,
    bump: &'a Bump,
) -> &'a mut Protocol<'g, 'a> {
    let protocol = match reader.read_event_into(&mut Vec::new()) {
        Ok(Event::Start(bytes)) => {
            assert!(
                bytes.name().into_inner() == b"protocol",
                "Missing protocol toplevel tag"
            );
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
                    interfaces
                        .entry(&interface.name)
                        .and_modify(|v| v.push(interface))
                        .or_insert(vec![interface]);
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

fn parse_interface<'g, 'a, R: BufRead>(
    reader: &mut Reader<R>,
    attrs: Attributes,
    bump: &'a Bump,
) -> &'a mut Interface<'g, 'a> {
    let interface = bump.alloc(Interface::new());
    for attr in attrs.filter_map(|res| res.ok()) {
        match attr.key.into_inner() {
            b"name" => interface.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"version" => interface.version = parse_or_panic(&attr.value),
            _ => {}
        }
    }

    loop {
        match reader.read_event_into(&mut Vec::new()) {
            Ok(Event::Start(bytes)) => match bytes.name().into_inner() {
                b"request" => interface.requests.push(parse_request(
                    reader,
                    interface.requests.len() as u32,
                    bytes.attributes(),
                    bump,
                )),
                b"event" => interface.events.push(parse_event(
                    reader,
                    interface.events.len() as u32,
                    bytes.attributes(),
                    bump,
                )),
                _ => {}
            },
            Ok(Event::End(bytes)) if bytes.name().into_inner() == b"interface" => break,
            _ => {}
        }
    }

    interface
}

fn parse_request<'g, 'a, R: BufRead>(
    reader: &mut Reader<R>,
    opcode: u32,
    attrs: Attributes,
    bump: &'a Bump,
) -> &'a mut Message<'g, 'a> {
    let request = bump.alloc(Message::new(opcode));
    for attr in attrs.filter_map(|res| res.ok()) {
        match attr.key.into_inner() {
            b"name" => request.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"since" => request.since = parse_or_panic(&attr.value),
            _ => {}
        }
    }

    loop {
        match reader.read_event_into(&mut Vec::new()) {
            Ok(Event::Start(bytes)) => {
                if let b"arg" = bytes.name().into_inner() {
                    request.args.push(parse_arg(reader, bytes.attributes()))
                }
            }
            Ok(Event::End(bytes)) if bytes.name().into_inner() == b"request" => break,
            _ => {}
        }
    }

    request
}

fn parse_event<'g, 'a, R: BufRead>(
    reader: &mut Reader<R>,
    opcode: u32,
    attrs: Attributes,
    bump: &'a Bump,
) -> &'a mut Message<'g, 'a> {
    let event = bump.alloc(Message::new(opcode));
    for attr in attrs.filter_map(|res| res.ok()) {
        match attr.key.into_inner() {
            b"name" => event.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"since" => event.since = parse_or_panic(&attr.value),
            _ => {}
        }
    }

    loop {
        match reader.read_event_into(&mut Vec::new()) {
            Ok(Event::Start(bytes)) => {
                if let b"arg" = bytes.name().into_inner() {
                    event.args.push(parse_arg(reader, bytes.attributes()))
                }
            }
            Ok(Event::End(bytes)) if bytes.name().into_inner() == b"event" => break,
            _ => {}
        }
    }

    event
}

fn parse_arg<R: BufRead>(_reader: &mut Reader<R>, attrs: Attributes) -> Arg {
    let mut arg = Arg::new();
    for attr in attrs.filter_map(|res| res.ok()) {
        match attr.key.into_inner() {
            b"name" => arg.name = decode_utf8_or_panic(attr.value.into_owned()),
            b"type" => arg.typ = parse_type(&attr.value),
            b"interface" => arg.interface = Some(parse_or_panic(&attr.value)),
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
