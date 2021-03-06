use std::{
    collections::{HashMap, VecDeque},
    sync::{mpsc, Arc, RwLock},
    thread,
    time::{Instant, Duration},
};

#[derive(Clone)]
struct Env {
    ep: Arc<RwLock<HashMap<String, ws::Sender>>>,
    chan: Arc<RwLock<HashMap<String, HashMap<String, (Instant, ws::Sender)>>>>, // chan_id -> ep
}

impl Env {
    pub fn new() -> Self {
        Env {
            ep: Arc::new(RwLock::new(HashMap::new())),
            chan: Arc::new(RwLock::new(HashMap::new()))
        }
    } 

    pub fn add_ep(&mut self, ep_id: String, sender: ws::Sender) {
        if let Ok(mut map) = self.ep.write() {
            map.insert(ep_id, sender);
        }
    }

    pub fn ep_reg_chan(&mut self, ep_id: String, chan_id: String, time_sec: u64, time_nanos: u64) -> Option<Instant> {
        if let Some(sender) = self.ep.read().unwrap().get(&ep_id) {
            let expire = Instant::now() + Duration::from_secs(time_sec) + Duration::from_nanos(time_nanos);
            self.chan.write().unwrap().entry(chan_id).or_insert(HashMap::new()).insert(ep_id, (expire, sender.clone()));
            Some(expire)
        } else {
            None
        }
    }    

    pub fn push_text(&mut self, src_ep_id: String, chan_id: String, text: String) -> ws::Result<usize> {
        let map = self.chan.read().unwrap();
        let now = Instant::now();
        let mut cnt = 0;
        if let Some(senders) = map.get(&chan_id) {
            for (ep_id, (valid_until, sender)) in senders.iter() {
                if valid_until >= &now {
                    if ep_id != &src_ep_id  {
                        sender.send(format!("1r|{}|{}|{}|{}|{}|{}", src_ep_id.len(), src_ep_id, chan_id.len(), chan_id, text.len(), text))?;
                        cnt += 1;
                    }
                } else {
                    if let Some(mp) = self.chan.write().unwrap().get_mut(&chan_id) {
                        mp.remove(ep_id);
                    }
                }
            }
        }
        Ok(cnt)
    }

    pub fn remove_ep(&mut self, ep_id: String) {
        self.ep.write().unwrap().remove(&ep_id);
        for (_chan, map) in self.chan.write().unwrap().iter_mut() {
            map.retain(|inner_ep_id, _instant_sender| &ep_id != inner_ep_id);
        }
    }
}

struct MsgServiceFactory {
    log_tx: mpsc::Sender<LogSignal>,
    msg_tx: mpsc::Sender<MsgSignal>,
}

#[derive(Clone)]
struct MsgServiceHandler {
    log_tx: mpsc::Sender<LogSignal>,
    msg_tx: mpsc::Sender<MsgSignal>,
    ws_sender: ws::Sender,
    ep_id: Option<String>,
}

impl MsgServiceFactory {
    pub fn new(log_tx: mpsc::Sender<LogSignal>, msg_tx: mpsc::Sender<MsgSignal>) -> Self {
        log_tx.send(LogSignal::ModuleStart(String::from("MSG-SERV"))).unwrap();
        Self { log_tx, msg_tx }
    }
}

impl ws::Factory for MsgServiceFactory {
    type Handler = MsgServiceHandler;
    fn connection_made(&mut self, ws_sender: ws::Sender) -> Self::Handler {
        Self::Handler { 
            ws_sender, 
            log_tx: self.log_tx.clone(), 
            msg_tx: self.msg_tx.clone(), 
            ep_id: None,
        }
    }
}

impl ws::Handler for MsgServiceHandler {
    fn on_open(&mut self, shake: ws::Handshake) -> ws::Result<()> {
        if let Some(addr) = shake.remote_addr()? {
            self.log_tx.send(LogSignal::ConnectionOpen(addr.clone())).unwrap();
            Ok(())
        } else {
            self.ws_sender.close(ws::CloseCode::Status)
        }
    }

    fn on_close(&mut self, code: ws::CloseCode, reason: &str) {
        self.msg_tx.send(MsgSignal::EpLogout { ep_id: self.ep_id.clone(), code, reason: reason.to_string() }).unwrap();
    }

    fn on_message(&mut self, msg: ws::Message) -> ws::Result<()> {
        if let ws::Message::Text(text) = msg {
            self.handle_message_signal(text.chars().collect())
        } else {
            self.ws_sender.close_with_reason(ws::CloseCode::Unsupported, "Please input as string text")
        }
    }
}

impl MsgServiceHandler {
    // message string format: version+type|data
    // data of text: len|chan_id|len|msg_str
    // data of chan: len|chan_id|u64|u32(duration sec/nanos since unix epoch)
    // 1h|16|eafc5479a7e9f012
    // 1t|7|c/lobby|10|helloworld
    // 1c|7|c/lobby|1548507103|2140083600
    #[inline]
    fn handle_message_signal(&mut self, mut text: VecDeque<char>) -> ws::Result<()> {
        match text.pop_front() {
            Some('1') => self.handle_v1(text),
            _ => self.ws_sender.close_with_reason(ws::CloseCode::Protocol, "Protocol other than '1' is not supported")
        }
    }

    #[inline]
    fn handle_v1(&mut self, mut text: VecDeque<char>) -> ws::Result<()> {
        match (text.pop_front(), text.pop_front()) {
            (Some('h'), Some('|')) => self.handle_v1_handshake(text),
            (Some('t'), Some('|')) => self.handle_v1_text(text),
            (Some('c'), Some('|')) => self.handle_v1_chan(text),
            _ => self.ws_sender.close_with_reason(ws::CloseCode::Invalid, "Invalid message type: expected 'h', 't' or 'c'"),
        }
    }

    #[inline]
    fn read_number(text: &mut VecDeque<char>) -> u64 {
        let mut cur = text.pop_front();
        while let Some(c) = cur {
            if c.to_digit(10).is_some() {
                break;
            } 
            cur = text.pop_front();
        }
        let mut ans = 0;
        while let Some(c) = cur {
            if let Some(digit) = c.to_digit(10) {
                ans *= 10;
                ans += digit as u64;
                cur = text.pop_front();
            } else {
                return ans;
            }
        };
        return ans;
    }

    #[inline]
    fn read_string(text: &mut VecDeque<char>) -> String {
        let cap = Self::read_number(text);
        let mut ans = String::with_capacity(cap as usize);
        for _i in 0..cap {
            if let Some(ch) = text.pop_front() {
                ans.push(ch)
            }
        }
        ans
    }

    // 16|eafc5479a7e9f012
    #[inline]
    fn handle_v1_handshake(&mut self, mut text: VecDeque<char>) -> ws::Result<()> {
        let ep_id = Self::read_string(&mut text);
        self.ep_id = Some(ep_id.clone());
        self.msg_tx.send(MsgSignal::EpIdentify { ep_id: ep_id.clone(), ws_sender: self.ws_sender.clone() }).unwrap();
        Ok(())
    }

    // 7|c/lobby|10|helloworld
    #[inline]
    fn handle_v1_text(&mut self, mut text: VecDeque<char>) -> ws::Result<()> {
        if let Some(src_ep_id) = self.ep_id.clone() {
            let chan_id = Self::read_string(&mut text);
            let text = Self::read_string(&mut text);
            println!("{}, {}!", chan_id, text);
            self.msg_tx.send(MsgSignal::Text { src_ep_id, chan_id, text }).unwrap();
            Ok(())
        } else {
            self.ws_sender.close_with_reason(ws::CloseCode::Status, "Connection unidentified")
        }
    }

    // 7|c/lobby|1548507103|2140083600
    #[inline]
    fn handle_v1_chan(&mut self, mut text: VecDeque<char>) -> ws::Result<()> {
        if let Some(src_ep_id) = self.ep_id.clone() {
            let chan_id = Self::read_string(&mut text);
            let valid_until_sec = Self::read_number(&mut text);
            let valid_until_nanos = Self::read_number(&mut text);
            self.msg_tx.send(MsgSignal::Chan { src_ep_id, chan_id, valid_until_sec, valid_until_nanos }).unwrap();
            Ok(())
        } else {
            self.ws_sender.close_with_reason(ws::CloseCode::Status, "Connection unidentified")
        }
    }
}


#[derive(Debug)]
enum LogSignal {
    ModuleStart(String),
    ConnectionOpen(String),
    ConnectionIdentified(String),
    ConnectionClose(Option<String>, ws::CloseCode, String),
    ChannelAdd(String, String, Instant),
    MessageSent(String, String, usize, String),
    ShutdownRequest(),
}

enum MsgSignal {
    EpIdentify {
        ep_id: String,
        ws_sender: ws::Sender,
    },
    Text {
        src_ep_id: String,
        chan_id: String,
        text: String,
    },
    Chan {
        src_ep_id: String,
        chan_id: String,
        valid_until_sec: u64,
        valid_until_nanos: u64,
    },
    EpLogout {
        ep_id: Option<String>,
        code: ws::CloseCode,
        reason: String,
    }
}

fn main() {
    let (log_tx, log_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let mut env = Env::new();
    thread::spawn(move || {
        while let Ok(sig) = log_rx.recv() {
            use LogSignal::*;
            match sig {
                ShutdownRequest() => {
                    println!("Shutting down...");
                    std::process::exit(0)
                },
                ModuleStart(meta) => 
                    println!("[Module {}] Started!", meta),
                ConnectionOpen(client_addr) => 
                    println!("[Client Addr {}] Connection open!", client_addr),
                ConnectionIdentified(ep_id) => 
                    println!("[Client EPID {}] Connection identified!", ep_id),
                ConnectionClose(addr, code, reason) => {
                    if let Some(ep_id) = addr {
                        println!("[Client EPID {}] Connection closed, Code:[{:?}], Reason:[{}]", ep_id, code, reason)
                    } else { 
                        println!("[Unidentified Client] Connection closed, Code:[{:?}], Reason:[{}]", code, reason)
                    }
                },
                ChannelAdd(src_ep_id, chan_id, expire) => 
                    println!("[Client EPID {}] Registered new channel [{}], expire at {:?}", src_ep_id, chan_id, expire),
                MessageSent(src_ep_id, chan_id, ep_cnt, text) => 
                    println!("[Client EPID {}] Sent to {} [{} other client(s)]: {}", src_ep_id, chan_id, ep_cnt, text),
            }
        }
    });
    let log_tx1 = log_tx.clone();
    thread::spawn(move || {
        while let Ok(sig) = msg_rx.recv() {
            use MsgSignal::*;
            match sig {
                EpIdentify { ep_id, ws_sender } => {
                    env.add_ep(ep_id.clone(), ws_sender);
                    log_tx1.send(LogSignal::ConnectionIdentified(ep_id)).unwrap();
                },
                EpLogout { ep_id, code, reason } => {
                    if let Some(ep_id) = ep_id.clone() {
                        env.remove_ep(ep_id);
                    }
                    log_tx1.send(LogSignal::ConnectionClose(ep_id, code, String::from(reason))).unwrap()
                },
                Chan { src_ep_id, chan_id, valid_until_sec, valid_until_nanos } => {
                    if let Some(expire) = env.ep_reg_chan(src_ep_id.clone(), chan_id.clone(), valid_until_sec, valid_until_nanos) {
                        log_tx1.send(LogSignal::ChannelAdd(src_ep_id, chan_id, expire)).unwrap();
                    }
                },
                Text { src_ep_id, chan_id, text } => {
                    if let Ok(ep_cnt) = env.push_text(src_ep_id.clone(), chan_id.clone(), text.clone()) {
                        log_tx1.send(LogSignal::MessageSent(src_ep_id, chan_id, ep_cnt, text)).unwrap();
                    } else {
                        eprintln!("error!");
                    };
                },
            };
        }
    });
    let addr = "0.0.0.0:6500";
    let log_tx1 = log_tx.clone();
    let msg_tx1 = msg_tx.clone();
    thread::spawn(move || {
        let fac = MsgServiceFactory::new(log_tx1, msg_tx1);
        ws::WebSocket::new(fac).unwrap()
            .listen(addr).unwrap()
    });
    loop {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        match input.trim() {
            "q" => log_tx.send(LogSignal::ShutdownRequest()).unwrap(),
            _ => {}
        }
    }
}
