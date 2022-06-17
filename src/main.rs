use anyhow::Error;
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    net::{SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use structopt::StructOpt;
use tokio::runtime::Handle;
use trust_dns_proto::{
    op::{header::MessageType, response_code::ResponseCode, Message},
    rr::{rdata::txt::TXT, record_data::RData, resource::Record, Name, RecordType},
};

mod warp;

type Id = String;
type Data = String;
type Counter = u32;

type ChunkedData = BTreeMap<Counter, Data>;
static FILE_DATA: Lazy<Mutex<BTreeMap<Id, (ChunkedData, Instant)>>> = Lazy::new(Default::default);

/// Our state of currently connected users.
///
/// - Key is their id
/// - Value is a sender of `warp::ws::Message`
type Users = Arc<
    tokio::sync::Mutex<
        HashMap<
            usize,
            tokio::sync::mpsc::UnboundedSender<Result<::warp::ws::Message, ::warp::Error>>,
        >,
    >,
>;

#[derive(Clone, Debug, StructOpt)]
#[structopt(global_settings(&[
    structopt::clap::AppSettings::ColoredHelp,
    structopt::clap::AppSettings::VersionlessSubcommands
]))]
struct CliArgs {
    listen_addr: SocketAddr,
    #[structopt(long = "dump-files", short = "d")]
    dump_files: bool,
}

fn main() -> Result<(), Error> {
    env_logger::init();
    let args = CliArgs::from_args();

    let socket = UdpSocket::bind(args.listen_addr)?;

    let rt = tokio::runtime::Runtime::new()?;
    let handle = rt.handle().clone();
    let users = Users::default();
    thread::Builder::new()
        .name("dnssteal Websesrver".to_string())
        .spawn({
            let args = args.clone();
            let users = users.clone();
            move || rt.block_on(crate::warp::main(args, users))
        })?;
    thread::Builder::new()
        .name("dnssteal File Writer".to_string())
        .spawn({
            let args = args.clone();
            let users = users.clone();
            move || file_writer(args, handle, users)
        })?;

    loop {
        let mut buf = [0; 4096];
        let (size, addr) = socket.recv_from(&mut buf)?;
        let dns_packet = Message::from_vec(&buf[..size])?;
        // dbg!(&dns_packet);
        let query = &dns_packet.queries()[0];
        let mut response = Message::new();
        response
            .set_authoritative(true)
            .set_id(dns_packet.id())
            .set_message_type(MessageType::Response)
            .set_op_code(dns_packet.op_code())
            .set_recursion_available(false)
            .set_response_code(ResponseCode::NoError)
            .set_truncated(false);

        if query.query_type() == RecordType::TXT {
            // Send a tiny description or executable script
            let first_label = query.name().iter().next().unwrap();
            if first_label == b"file" {
                response.add_query(query.clone());
                let answer = Record::from_rdata(
                    query.name().clone(),
                    5,
                    RData::TXT(TXT::new(vec!["Hello \x1b[1;31m World".to_string()])),
                );
                response.add_answer(answer);
            } else if first_label == b"help" {
                todo!();
            } else {
                todo!()
                // response = Message::new()
            }
        } else {
            // Add the transmitted data to the buffer
            if let Err(err) = parse_name(query.name()) {
                log::warn!("{}", err);
                response.set_response_code(ResponseCode::ServFail);
            }
        }

        // response.to_vec();
        socket.send_to(&*(response.to_vec()?), addr)?;
    }
}

fn file_writer(args: CliArgs, handle: Handle, users: Users) {
    loop {
        thread::sleep(Duration::new(1, 0));

        let mut file_data = FILE_DATA.lock().unwrap();
        let mut delete_list = Vec::new();
        let now = Instant::now();
        let five_seconds = Duration::new(5, 0);
        for (id, (_data, time)) in &*file_data {
            if (now - *time) > five_seconds {
                delete_list.push(id.clone());
            }
        }
        for id in delete_list {
            log::info!("Flushing file with ID {}", id);
            if let Some((data, time)) = file_data.remove(&id) {
                match assemble_file(id, data, time) {
                    Ok(file) => {
                        if args.dump_files {
                            let _ = std::fs::write(
                                format!("transmitted_{}_{}", file.time.timestamp(), file.filename),
                                &file.content,
                            );
                        }
                        handle.spawn({
                            let users = users.clone();
                            crate::warp::user_message(file, users)
                        });
                    }
                    Err(err) => log::warn!("{}", err),
                };
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct File {
    filename: String,
    content: String,
    md5: String,
    #[serde(with = "chrono::serde::ts_seconds")]
    time: DateTime<Utc>,
}

fn assemble_file(id: Id, data: ChunkedData, _time: Instant) -> Result<File, Error> {
    let parts_count = data.len();
    let highest_part_id = data.keys().last();
    assert_eq!(Some(&(parts_count as u32 - 1)), highest_part_id);

    let mut payload = data.into_iter().map(|(_, s)| s).collect::<String>();
    // Check that we have seen the last part. The last part is special by not having the trailing `-`.
    assert_ne!("-", &payload[payload.len() - 1..]);

    // Cleanup the symbols which are a transmission artifact
    payload = payload.replace('*', "+").replace('-', "");
    let payload = base64::decode(payload)?;
    // Format is: Filename \0 Type \0 Content
    // Filename and type are guaranteed to not have 0-bytes
    // Types:
    //     - <empty>: The payload is transmitted base64 encoded
    //     - `z`: Payload is gzipped and base64 encoded
    let parts: Vec<_> = payload.splitn(3, |&x| x == 0).collect();
    let filename = String::from_utf8_lossy(parts[0]);
    let content_type = String::from_utf8_lossy(parts[1]);
    assert_eq!("", content_type);
    let content = String::from_utf8_lossy(parts[2]);

    let hash = format!("{:x}", md5::compute(content.as_bytes()));
    log::info!(
        "Extracted file (ID: {}, Hash: {}): {}\n{}",
        id,
        hash,
        filename,
        content
    );
    // check if extracted content matches with partial hash (id).
    assert_eq!(id, hash[0..4]);

    Ok(File {
        filename: filename.to_string(),
        content: content.to_string(),
        md5: hash,
        time: Utc::now(),
    })
}

fn parse_name(name: &Name) -> Result<(), Error> {
    let labels: Vec<_> = name.iter().rev().skip(1).collect();
    // dbg!(&labels);
    let id = String::from_utf8(labels[0].to_vec())?;
    let ctr: Counter = String::from_utf8_lossy(labels[1]).parse()?;
    let now = Instant::now();

    let mut file_data = FILE_DATA.lock().unwrap();
    let entry = file_data.entry(id).or_insert((BTreeMap::default(), now));
    entry.1 = now;
    entry.0.insert(
        ctr,
        labels[2..]
            .iter()
            .rev()
            .map(|bytes| String::from_utf8(bytes[..bytes.len()].to_vec()).unwrap())
            .collect(),
    );
    dbg!(&file_data);
    Ok(())
}
