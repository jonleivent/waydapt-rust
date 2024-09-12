#![forbid(unsafe_code)]
#[allow(clippy::wildcard_imports)]
// Adapted from wayland-scanner/parse.rs, keeping only what we need
use super::protocol::*;
use crate::{basics::UnwindDo, crate_traits::Alloc};
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
    if let Ok(Event::Decl(_d)) = reader.read_event_into(&mut Vec::new()) {
        // TBD: maybe check _d?
    } else {
        panic!("Missing xml decl");
    }
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
    let _ud = UnwindDo(|| eprintln!("In protocol {}", &protocol.name));
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(bytes)) => {
                if let b"interface" = bytes.name().into_inner() {
                    let interface = parse_interface(&mut reader, bytes.attributes(), alloc);
                    protocol.interfaces.insert(&interface.name, interface);
                }
            }
            Ok(Event::End(bytes)) => {
                if let b"protocol" = bytes.name().into_inner() {
                    break;
                }
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

    let _ud = UnwindDo(|| eprintln!("In interface {}", &interface.name));
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            #[allow(clippy::cast_possible_truncation)]
            Ok(Event::Start(bytes)) => {
                let event_or_request = bytes.name().into_inner();
                let is_request = match event_or_request {
                    b"request" => true,
                    b"event" => false,
                    _ => continue,
                };
                let msg = parse_message(
                    reader,
                    if is_request {
                        interface.requests.len() as u32
                    } else {
                        interface.events.len() as u32
                    },
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
    let _ud = UnwindDo(|| {
        eprintln!("In {} {}", std::str::from_utf8(event_or_request).unwrap(), &message.name);
    });
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

#[cfg(test)]
mod tests {
    #![allow(unused)]
    use std::fs::File;
    use std::ptr;

    use crate::basics::LEAKER;

    use super::*;

    fn check_msg<'a>(
        interface: &'a Interface<'a>, msg_name: &'a str, opcode: usize, request: bool, since: u32,
    ) -> &'a Message<'a> {
        let msg = interface.get_message(!request, opcode);
        let ptr = ptr::from_ref(msg);
        if request {
            assert_eq!(ptr::from_ref(interface.get_request(opcode)), ptr);
            assert_eq!(ptr::from_ref(interface.get_request_by_name(msg_name).unwrap()), ptr);
            assert!(msg.is_request);
        } else {
            assert_eq!(ptr::from_ref(interface.get_event(opcode)), ptr);
            assert_eq!(ptr::from_ref(interface.get_event_by_name(msg_name).unwrap()), ptr);
            assert!(!msg.is_request);
        }
        assert_eq!(msg.name, msg_name);
        assert_eq!(msg.get_name(), msg_name);
        assert_eq!(msg.opcode as usize, opcode);
        assert_eq!(msg.since, since);
        msg
    }

    fn check_zero_args_msg<'a>(
        interface: &'a Interface<'a>, msg_name: &'a str, opcode: usize, request: bool, since: u32,
    ) {
        let msg = check_msg(interface, msg_name, opcode, request, since);
        assert_eq!(msg.num_fds, 0);
        assert_eq!(msg.args.len(), 0);
    }

    fn check_many_args_msg<'a>(
        interface: &'a Interface<'a>, msg_name: &'a str, opcode: usize, request: bool, since: u32,
    ) {
        let many_args_msg = check_msg(interface, msg_name, opcode, request, since);
        assert_eq!(many_args_msg.num_fds, 1);
        let args = &many_args_msg.args;
        let good_args = [
            ("unsigned_int", Type::Uint),
            ("signed_int", Type::Int),
            ("fixed_point", Type::Fixed),
            ("number_array", Type::Array),
            ("some_text", Type::String),
            ("file_descriptor", Type::Fd),
            ("object_id", Type::Object),
            ("new_id", Type::NewId),
        ];
        assert_eq!(args.len(), good_args.len());
        for ((good_arg_name, good_arg_typ), Arg { name, typ, .. }) in
            good_args.iter().zip(args.iter())
        {
            assert_eq!(good_arg_name, name);
            assert_eq!(good_arg_typ, typ);
        }
        assert_eq!(args[6].interface_name, Some("an-interface-name".into()));
        assert_eq!(args[7].interface_name, Some("another-interface-name".into()));
    }

    #[test]
    fn parse_test_protocol() {
        let file = File::open("./tests/parse-test-protocol.xml").unwrap();
        let protocol = parse(file, &LEAKER);
        assert_eq!(protocol.name, "parse test protocol");
        let test_global_interface = protocol.find_interface("test_global").unwrap();
        assert_eq!(test_global_interface.name, "test_global");
        assert_eq!(test_global_interface.parsed_version, 5);
        check_many_args_msg(test_global_interface, "many_args", 0, true, 4);
        check_many_args_msg(test_global_interface, "many_args_event", 0, false, 1);
        check_zero_args_msg(test_global_interface, "second_request", 1, true, 6);
        check_zero_args_msg(test_global_interface, "second_event", 1, false, 6);
        //
        let secondary_interface = protocol.find_interface("secondary").unwrap();
        assert_eq!(secondary_interface.name, "secondary");
        assert_eq!(secondary_interface.parsed_version, 3);
        check_zero_args_msg(secondary_interface, "destroy", 0, true, 2);
        //
        let tertiary_interface = protocol.find_interface("tertiary").unwrap();
        assert_eq!(tertiary_interface.name, "tertiary");
        assert_eq!(tertiary_interface.parsed_version, 5);
        check_zero_args_msg(tertiary_interface, "destroy", 0, true, 3);
    }
}
