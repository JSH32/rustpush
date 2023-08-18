use std::{collections::HashMap, io, sync::Arc, time::Duration};

use openssl::{sha::{Sha1, sha1}, pkey::PKey, error::ErrorStack, hash::MessageDigest, sign::Signer, rsa::Padding, x509::X509};
use ringbuf::{HeapConsumer, HeapRb};
use rustls::{Certificate, PrivateKey, client::{ServerCertVerifier, ServerCertVerified}};
use tokio::{net::TcpStream, io::{WriteHalf, ReadHalf, AsyncReadExt, AsyncWriteExt}, time, sync::{Mutex, oneshot, mpsc::{self, Receiver}}};
use tokio_rustls::{TlsConnector, client::TlsStream};
use rand::Rng;
use std::net::ToSocketAddrs;
use tokio::io::split;
use serde::{Serialize, Deserialize};

use crate::{albert::generate_push_cert, bags::{get_bag, APNS_BAG, BagError}, util::KeyPair, ids::signing::generate_nonce};

#[derive(Debug, Clone)]
pub struct APNSPayload {
    pub id: u8,
    pub fields: Vec<(u8, Vec<u8>)>
}

impl APNSPayload {
    fn new(id: u8, fields: Vec<(u8, Vec<u8>)>) -> Self {
        APNSPayload { id, fields }
    }

    async fn read(read: &mut ReadHalf<TlsStream<TcpStream>>) -> Result<Option<APNSPayload>, APNSError> {
        let id = read.read_u8().await?;

        if id == 0x0 {
            return Ok(None);
        }

        let len = read.read_u32().await?;
        let mut buf = vec![0; len as usize];
        read.read_exact(&mut buf).await?;

        let mut curr_buf: &[u8] = &buf;
        let mut fields: Vec<(u8, Vec<u8>)> = Vec::new();
        while curr_buf.len() > 0 {
            let fid = curr_buf[0];
            let flen = u16::from_be_bytes(curr_buf[1..3].try_into().unwrap()) as usize;
            let fval = &curr_buf[3..3+flen];
            fields.push((fid, fval.to_vec()));
            curr_buf = &curr_buf[3+flen..];
        }

        Ok(Some(APNSPayload {
            id,
            fields
        }))
    }

    pub fn get_field(&self, field: u8) -> Option<&Vec<u8>> {
        self.fields.iter().find(|f| f.0 == field).map(|i| &i.1)
    }

    fn serialize(&self) -> Vec<u8> {
        let payload: Vec<u8> = self.fields.iter().flat_map(|(id, val)| {
            [id.to_be_bytes().to_vec(), (val.len() as u16).to_be_bytes().to_vec(), val.clone()].concat()
        }).collect();
        [self.id.to_be_bytes().to_vec(), (payload.len() as u32).to_be_bytes().to_vec(), payload].concat()
    }
}

struct InnerSubmitter {
    stream: WriteHalf<TlsStream<TcpStream>>,
    token: Vec<u8>
}

#[derive(Clone)]
pub struct APNSSubmitter(Arc<Mutex<InnerSubmitter>>);

impl APNSSubmitter {
    fn make(stream: WriteHalf<TlsStream<TcpStream>>) -> APNSSubmitter {
        APNSSubmitter(Arc::new(Mutex::new(InnerSubmitter { stream, token: vec![] })))
    }

    async fn token(&self) -> Vec<u8> {
        let locked = self.0.lock().await;
        locked.token.clone()
    }

    async fn set_token(&self, token: &[u8]) {
        let mut locked = self.0.lock().await;
        locked.token = token.to_vec();
    }

    async fn write_data(&self, buf: &[u8]) {
        let mut locked = self.0.lock().await;
        locked.stream.write(buf).await.unwrap();
    }

    async fn send_payload(&self, id: u8, fields: Vec<(u8, Vec<u8>)>) {
        self.write_data(&APNSPayload::new(id, fields).serialize()).await;
    }

    pub async fn set_state(&self, state: u8) {
        println!("Sending state packet {}", state);
        let magic_num: u32 = 0x7FFFFFFF;
        self.send_payload(0x14, vec![(1, state.to_be_bytes().to_vec()), (2, magic_num.to_be_bytes().to_vec())]).await;
    }

    async fn send_message(&self, topic: &str, payload: &[u8], id: Option<&[u8]>) {
        let rand = rand::thread_rng().gen::<[u8; 4]>();
        let id = id.unwrap_or(&rand);
        self.send_payload(0x0A, vec![
            (4, id.to_vec()),
            (1, sha1(topic.as_bytes()).to_vec()),
            (2, self.token().await),
            (3, payload.to_vec())
        ]).await;
    }

    async fn keep_alive(&self) {
        self.send_payload(0x0C, vec![]).await;
        println!("Sending keep alive");
    }

    async fn send_ack(&self, id: &[u8]) {
        println!("Sending ack for {:?}", id);
        self.send_payload(0x0B, vec![(1, self.token().await), (4, id.to_vec()), (8, vec![0x0])]).await;
    }
    
    pub async fn filter(&self, topics: &[&str]) {
        println!("Sending filter for {:?}", topics);
        let mut fields = vec![(1, self.token().await)];
        for topic in topics {
            let mut hasher = Sha1::new();
            hasher.update(topic.as_bytes());
            fields.push((2, hasher.finish().to_vec()));
        }
        self.send_payload(9, fields).await;
    }
}

enum WaitingCb {
    OneShot(oneshot::Sender<APNSPayload>),
    Cont(mpsc::Sender<APNSPayload>)
}

struct WaitingTask {
    waiting_for: Box<dyn Fn(&APNSPayload) -> bool + Send + Sync>,
    when: WaitingCb,
}

pub struct APNSReader(Arc<Mutex<Vec<WaitingTask>>>);

impl APNSReader {
    fn new(mut read: ReadHalf<TlsStream<TcpStream>>, write: APNSSubmitter) -> APNSReader {
        let reader: Arc<Mutex<Vec<WaitingTask>>> = Arc::new(Mutex::new(vec![]));
        let reader_clone = reader.clone();
        tokio::spawn(async move {
            loop {
                let result = APNSPayload::read(&mut read).await;
                let Ok(payload) = result else {
                    println!("conn broken? {:?}", result);
                    break // maybe conn broken?
                };
                let Some(payload) = payload else {
                    continue
                };
                if payload.id == 0x0A {
                    println!("Sending automatic ACK");
                    write.send_ack(payload.get_field(4).unwrap()).await;
                }
                
                println!("Recieved payload");
                let mut locked = reader_clone.lock().await;
                let remove_idxs: Vec<usize> = locked.iter().enumerate().filter_map(|(i, item)| {
                    if (item.waiting_for)(&payload) {
                        Some(i)
                    } else {
                        None
                    }
                }).collect();
                for idx in remove_idxs.iter().rev() {
                    match &locked.get(*idx).unwrap().when {
                        WaitingCb::OneShot(cb) => {
                            let WaitingCb::OneShot(cb) = locked.remove(*idx).when else {
                                panic!("no")
                            };
                            cb.send(payload.clone()).unwrap();
                        },
                        WaitingCb::Cont(cb) => {
                            cb.send(payload.clone()).await.unwrap();
                        }
                    }
                }
            }
        });
        APNSReader(reader)
    }

    pub async fn register_for<F>(&self, p: F) -> Receiver<APNSPayload>
    where
        F: Fn(&APNSPayload) -> bool + Send + Sync + 'static,
    {
        let mut locked = self.0.lock().await;
        let (tx, rx) = mpsc::channel(20);
        locked.push(WaitingTask { waiting_for: Box::new(p), when: WaitingCb::Cont(tx) });
        rx
    }

    pub async fn wait_find_pred<F>(&self, p: F) -> APNSPayload
    where
        F: Fn(&APNSPayload) -> bool + Send + Sync + 'static,
    {
        let mut locked = self.0.lock().await;
        let (tx, rx) = oneshot::channel();
        locked.push(WaitingTask { waiting_for: Box::new(p), when: WaitingCb::OneShot(tx) });
        drop(locked);
        rx.await.unwrap()
        /*let mut interval = time::interval(Duration::from_millis(100));
        loop {
            let mut locked = self.0.lock().await;
            let item = locked.iter().position(|item| p(item));
            if let Some(item) = item {
                return locked.remove(item);
            }
            drop(locked);
            interval.tick().await;
        }*/
    }

    pub async fn wait_find(&self, id: u8) -> APNSPayload {
        self.wait_find_pred(move |item| item.id == id).await
    }
}

pub struct APNSConnection {
    pub submitter: APNSSubmitter,
    pub state: APNSState,
    pub reader: APNSReader
}

// serialize this to JSON to save state
#[derive(Serialize, Deserialize, Clone)]
pub struct APNSState {
    pub keypair: KeyPair,
    pub token: Option<Vec<u8>>
}

struct DangerousVerifier();
impl ServerCertVerifier for DangerousVerifier {
    fn verify_server_cert(
            &self,
            end_entity: &Certificate,
            intermediates: &[Certificate],
            server_name: &rustls::ServerName,
            scts: &mut dyn Iterator<Item = &[u8]>,
            ocsp_response: &[u8],
            now: std::time::SystemTime,
        ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}

#[derive(Debug)]
pub enum APNSError {
    RustlsError(rustls::Error),
    BagError(BagError),
    IoError(io::Error),
    SignError(ErrorStack),
    ConnectError
}
impl From<ErrorStack> for APNSError {
    fn from(value: ErrorStack) -> Self {
        APNSError::SignError(value)
    }
}
impl From<rustls::Error> for APNSError {
    fn from(value: rustls::Error) -> Self {
        APNSError::RustlsError(value)
    }
}
impl From<BagError> for APNSError {
    fn from(value: BagError) -> Self {
        APNSError::BagError(value)
    }
}
impl From<io::Error> for APNSError {
    fn from(value: io::Error) -> Self {
        APNSError::IoError(value)
    }
}

const APNS_PORT: u16 = 5223;

impl APNSConnection {
    async fn connect() -> Result<TlsStream<TcpStream>, APNSError> {
        let mut config = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(DangerousVerifier()))
            .with_no_client_auth();
        config.alpn_protocols.push(b"apns-security-v3".to_vec());

        let connector = TlsConnector::from(Arc::new(config));

        let bag = get_bag(APNS_BAG).await?;
        let host = format!("{}-{}", 
            rand::thread_rng().gen_range(1..bag.get("APNSCourierHostcount").unwrap().as_unsigned_integer().unwrap()),
            bag.get("APNSCourierHostname").unwrap().as_string().unwrap());
        let addr = (host.as_str(), APNS_PORT).to_socket_addrs()?.next().ok_or(io::Error::from(io::ErrorKind::NotFound))?;
        let stream = TcpStream::connect(&addr).await?;

        let domain = rustls::ServerName::try_from(host.as_str())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid dnsname"))?;
        
        let connection = connector.connect(domain, stream).await?;

        println!("Connected to APNs ({})", host);

        Ok(connection)
    }

    pub async fn send_message(&self, topic: &str, payload: &[u8], id: Option<&[u8]>) {
        self.submitter.send_message(topic, payload, id).await;
        let msg = self.reader.wait_find(0x0B).await;
        if msg.get_field(8).unwrap()[0] != 0x0 {
            panic!("Failed to send message");
        }
    }

    fn init_conn(write: APNSSubmitter) {
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                write.keep_alive().await;
            }
        });
    }

    pub async fn new(state: Option<APNSState>) -> Result<APNSConnection, APNSError> {
        let mut state = match state {
            Some(state) => state,
            None => {
                let keypair = generate_push_cert().await.unwrap();
                APNSState {
                    keypair,
                    token: None
                }
            }
        };
        let stream = APNSConnection::connect().await?;
        let (read, writer) = split(stream);
        let writer = APNSSubmitter::make(writer);
        let reader = APNSReader::new(read, writer.clone());

        // connect
        let flags: u32 = 0b01000001;

        let priv_key = PKey::private_key_from_der(&state.keypair.private)?;

        let mut signer = Signer::new(MessageDigest::sha1(), priv_key.as_ref())?;
        signer.set_rsa_padding(Padding::PKCS1)?;
        let nonce = generate_nonce(0x0);
        let signature = [
            vec![0x1, 0x1],
            signer.sign_oneshot_to_vec(&nonce)?
        ].concat();

        let mut fields = vec![
            (0x2, vec![0x01]),
            (0x5, flags.to_be_bytes().to_vec()),
            (0xC, state.keypair.cert.clone()),
            (0xD, nonce),
            (0xE, signature)
        ];

        if let Some(token) = &state.token {
            println!("Sending connect message with token {:?}", token);
            fields.push((1, token.clone()));
        } else {
            println!("Sending connect message without token");
        }
        
        writer.send_payload(7, fields).await;

        let response = reader.wait_find(8).await;
        if u8::from_be_bytes(response.get_field(1).unwrap().clone().try_into().unwrap()) != 0x00 {
            return Err(APNSError::ConnectError)
        }
        
        let new_token = response.get_field(3);
        let token = if let Some(new_token) = new_token {
            state.token = Some(new_token.clone());
            new_token
        } else if let Some(token) = &state.token {
            token
        } else {
            panic!("no token!")
        };
        writer.set_token(&token).await;

        println!("Recieved connect response with token {:?}", token);

        let conn: APNSConnection = APNSConnection {
            reader,
            submitter: writer.clone(),
            state
        };
        APNSConnection::init_conn(writer.clone());
        Ok(conn)
    }
}